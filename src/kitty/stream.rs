//! Incremental APC framing: chunk aggregation, PTY stream filtering, and reply encoding.

use std::collections::HashMap;
use std::io;

use super::command::parse_control_keys;
use super::model::{DeleteTarget, KittyAction, KittyCommand, KittyEvent};
use super::payload::{decode_image_data, validate_capability_query};

const MAX_APC_PAYLOAD: usize = 32 * 1024 * 1024;

/// Upper bound on the clean-output scratch buffer retained for reuse. PTY reads are 64 KiB, so a
/// normal buffer is always recycled; anything larger is dropped instead of pinned indefinitely.
#[cfg(test)]
pub const MAX_RECYCLED_CLEAN_BYTES: usize = 256 * 1024;

#[inline]
fn may_contain_kitty_apc(bytes: &[u8]) -> bool {
    let mut rest = bytes;
    while let Some(pos) = memchr::memchr(0x1b, rest) {
        if pos + 1 >= rest.len() || rest[pos + 1] == b'_' {
            return true;
        }
        rest = &rest[pos + 1..];
    }
    false
}

/// State machine intercepting Kitty APC graphics sequences (`\x1b_G...;payload\x1b\`) from the byte stream.
#[derive(Default)]
pub struct KittyParser {
    pub in_apc: bool,
    apc_buffer: Vec<u8>,
    pub(crate) pending_stream: Vec<u8>,
    chunked_command: Option<KittyCommand>,
    chunked_payload: Vec<u8>,
    next_image_id: u32,
    /// Latest image id allocated for each client-supplied `I=` image number.
    image_numbers: HashMap<u32, u32>,
    #[cfg(test)]
    pub clean_scratch: Vec<u8>,
    #[cfg(test)]
    pub events_scratch: Vec<KittyEvent>,
}

/// A parsed chunk from the Kitty stream: either a slice of clean terminal text or a graphics event.
#[derive(Debug, PartialEq)]
pub enum KittyStreamChunk<'a> {
    Text(&'a [u8]),
    Event(Box<KittyEvent>),
}

impl KittyParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_image_id: 1,
            ..Default::default()
        }
    }

    /// Processes an incoming byte stream and yields chunks (Text or Event) in strict chronological order.
    pub fn process_stream<F>(&mut self, incoming: &[u8], mut callback: F)
    where
        F: FnMut(KittyStreamChunk<'_>),
    {
        if self.is_fast_path(incoming) {
            callback(KittyStreamChunk::Text(incoming));
            return;
        }

        let mut bytes_buf;
        let bytes: &[u8] = if self.pending_stream.is_empty() {
            incoming
        } else {
            bytes_buf = std::mem::take(&mut self.pending_stream);
            bytes_buf.extend_from_slice(incoming);
            &bytes_buf
        };

        let mut i = 0;
        let mut text_start = 0;

        while i < bytes.len() {
            if self.in_apc {
                if self.apc_buffer.len() > MAX_APC_PAYLOAD {
                    self.in_apc = false;
                    self.apc_buffer.clear();
                    self.chunked_payload.clear();
                    self.chunked_command = None;
                    i += 1;
                    text_start = i;
                    continue;
                }

                // Look for APC terminator: \x1b\ (ST) or \x07 (BEL)
                let byte = bytes[i];
                if byte == 0x07 {
                    self.in_apc = false;
                    if let Some(event) = self.finish_apc() {
                        callback(KittyStreamChunk::Event(Box::new(event)));
                    }
                    self.apc_buffer.clear();
                    i += 1;
                    text_start = i;
                } else if byte == 0x1b {
                    if i + 1 < bytes.len() {
                        if bytes[i + 1] == b'\\' {
                            self.in_apc = false;
                            if let Some(event) = self.finish_apc() {
                                callback(KittyStreamChunk::Event(Box::new(event)));
                            }
                            self.apc_buffer.clear();
                            i += 2;
                            text_start = i;
                        } else {
                            self.apc_buffer.push(byte);
                            i += 1;
                        }
                    } else {
                        self.pending_stream.push(byte);
                        return;
                    }
                } else {
                    self.apc_buffer.push(byte);
                    i += 1;
                }
            } else if bytes[i] == 0x1b {
                if i + 2 < bytes.len() {
                    if bytes[i + 1] == b'_' && bytes[i + 2] == b'G' {
                        if i > text_start {
                            callback(KittyStreamChunk::Text(&bytes[text_start..i]));
                        }
                        self.in_apc = true;
                        self.apc_buffer.clear();
                        i += 3;
                    } else {
                        i += 1;
                    }
                } else if i + 1 < bytes.len() {
                    if bytes[i + 1] == b'_' {
                        if i > text_start {
                            callback(KittyStreamChunk::Text(&bytes[text_start..i]));
                        }
                        self.pending_stream.extend_from_slice(&bytes[i..]);
                        return;
                    } else {
                        i += 1;
                    }
                } else {
                    if i > text_start {
                        callback(KittyStreamChunk::Text(&bytes[text_start..i]));
                    }
                    self.pending_stream.push(bytes[i]);
                    return;
                }
            } else {
                i += 1;
            }
        }

        if !self.in_apc && i > text_start {
            callback(KittyStreamChunk::Text(&bytes[text_start..i]));
        }
    }

    /// Returns `true` if the incoming byte slice is guaranteed to contain no Kitty APC sequences
    /// and the parser is not currently in the middle of parsing an APC stream.
    ///
    /// When this returns `true`, callers can pass the raw byte slice directly to the VT parser,
    /// completely bypassing all allocation, filtering, and copying overhead (0-alloc fast path).
    #[inline]
    #[must_use]
    pub fn is_fast_path(&self, incoming: &[u8]) -> bool {
        !self.in_apc && self.pending_stream.is_empty() && !may_contain_kitty_apc(incoming)
    }

    /// Filters an incoming byte stream, stripping Kitty APC sequences and emitting parsed graphics events.
    ///
    /// Non-graphics bytes are returned in the first vector to be processed by the standard VT parser.
    /// Events are transferred without cloning nested image buffers, and internal event storage
    /// is left empty to avoid retaining image payloads in parser memory.
    #[cfg(test)]
    pub fn filter_bytes(&mut self, incoming: &[u8]) -> (Vec<u8>, Vec<KittyEvent>) {
        if self.is_fast_path(incoming) {
            return (incoming.to_vec(), Vec::new());
        }
        // Convenience path: return an owned copy and keep the scratch capacity internal, bounded
        // by the same retention limit as the recycled hot path. The PTY path uses
        // `filter_bytes_slow` + `recycle_clean_buffer` to avoid this copy entirely.
        self.filter_internal(incoming);
        let text = self.clean_scratch.clone();
        if self.clean_scratch.capacity() > MAX_RECYCLED_CLEAN_BYTES {
            self.clean_scratch = Vec::new();
        } else {
            self.clean_scratch.clear();
        }
        let events = std::mem::take(&mut self.events_scratch);
        (text, events)
    }

    /// Filters an incoming byte stream known to require slow-path APC filtering,
    /// bypassing redundant `is_fast_path` inspection.
    ///
    /// The cleaned bytes are transferred by move as the parser's own scratch buffer rather than
    /// cloned. Hot-path callers must return that buffer via [`Self::recycle_clean_buffer`] once the
    /// bytes have been consumed so its capacity can be reused.
    #[cfg(test)]
    pub fn filter_bytes_slow(&mut self, incoming: &[u8]) -> (Vec<u8>, Vec<KittyEvent>) {
        self.filter_internal(incoming);
        let text = std::mem::take(&mut self.clean_scratch);
        let events = std::mem::take(&mut self.events_scratch);
        (text, events)
    }

    /// Returns a buffer previously handed out by [`Self::filter_bytes_slow`] for reuse.
    ///
    /// Capacity is retained only while it stays within [`MAX_RECYCLED_CLEAN_BYTES`]; larger
    /// transient buffers are dropped to avoid pinning peak allocations.
    #[cfg(test)]
    pub fn recycle_clean_buffer(&mut self, mut buffer: Vec<u8>) {
        buffer.clear();
        self.clean_scratch = if buffer.capacity() <= MAX_RECYCLED_CLEAN_BYTES {
            buffer
        } else {
            Vec::new()
        };
    }

    #[cfg(test)]
    fn filter_internal(&mut self, incoming: &[u8]) {
        let mut clean = std::mem::take(&mut self.clean_scratch);
        let mut events = std::mem::take(&mut self.events_scratch);
        clean.clear();
        events.clear();

        self.process_stream(incoming, |chunk| match chunk {
            KittyStreamChunk::Text(text) => {
                clean.extend_from_slice(text);
            }
            KittyStreamChunk::Event(event) => {
                events.push(*event);
            }
        });

        self.clean_scratch = clean;
        self.events_scratch = events;
    }

    fn finish_apc(&mut self) -> Option<KittyEvent> {
        let buffer = &self.apc_buffer;
        let (keys_bytes, payload_bytes) = match buffer.iter().position(|&b| b == b';') {
            Some(pos) => (&buffer[..pos], &buffer[pos + 1..]),
            None => (&buffer[..], &[][..]),
        };

        let keys_str = std::str::from_utf8(keys_bytes).ok()?;
        let command = parse_control_keys(keys_str);

        // Resolve chunked commands before dispatching so a chunked query is only
        // validated once the complete payload has arrived.
        let (full_command, raw_payload) = if command.more_chunks {
            if self.chunked_command.is_none() {
                self.chunked_command = Some(command);
            }
            self.chunked_payload.extend_from_slice(payload_bytes);
            return None;
        } else if let Some(mut first_cmd) = self.chunked_command.take() {
            self.chunked_payload.extend_from_slice(payload_bytes);
            let full_payload = std::mem::take(&mut self.chunked_payload);
            first_cmd.more_chunks = false;
            (first_cmd, full_payload)
        } else {
            (command, payload_bytes.to_vec())
        };

        // Capability probing query (a=q): load and validate, but never store or display.
        if full_command.action == KittyAction::Query {
            let Some(id) = full_command.image_id else {
                // Kitty requires an image id to address the response.
                return None;
            };
            if let Some(error) = full_command.error {
                return kitty_error_response(&full_command, id, error);
            }
            return match validate_capability_query(&full_command, &raw_payload) {
                Ok(()) => kitty_success_response(&full_command, id, "OK"),
                Err(error) => kitty_error_response(&full_command, id, &kitty_error_message(&error)),
            };
        }

        if let Some(error) = full_command.error {
            let id = full_command
                .image_id
                .or_else(|| {
                    full_command
                        .image_number
                        .and_then(|number| self.image_numbers.get(&number).copied())
                })
                .unwrap_or(0);
            return if id == 0 {
                None
            } else {
                kitty_error_response(&full_command, id, error)
            };
        }

        if full_command.action == KittyAction::Delete {
            let target = match full_command.delete_target {
                DeleteTarget::ByNumber {
                    number,
                    placement,
                    free,
                } => match self.image_numbers.get(&number).copied() {
                    Some(id) => match (placement, free) {
                        (Some(placement), true) => {
                            DeleteTarget::ByIdPlacementAndFree(id, placement)
                        }
                        (Some(placement), false) => DeleteTarget::ByIdPlacement(id, placement),
                        (None, true) => DeleteTarget::ByIdAndFree(id),
                        (None, false) => DeleteTarget::ById(id),
                    },
                    None => DeleteTarget::ByNumber {
                        number,
                        placement,
                        free,
                    },
                },
                other => other,
            };
            return Some(KittyEvent::Delete { target });
        }

        if full_command.action == KittyAction::Place {
            let mut command = full_command;
            if command.image_id.is_none()
                && let Some(number) = command.image_number
            {
                command.image_id = self.image_numbers.get(&number).copied();
            }
            let Some(id) = command.image_id else {
                return kitty_error_response(&command, 0, "ENOENT:image not found");
            };
            command.image_id = Some(id);
            return Some(KittyEvent::Place { command });
        }

        let mut command = full_command;
        let image_id = command.image_id.unwrap_or_else(|| self.allocate_image_id());
        command.image_id = Some(image_id);

        match decode_image_data(image_id, &command, &raw_payload) {
            Ok(image) => {
                if let Some(number) = command.image_number {
                    self.image_numbers.insert(number, image_id);
                }
                Some(KittyEvent::Transmit { command, image })
            }
            Err(error) => kitty_error_response(&command, image_id, &kitty_error_message(&error)),
        }
    }

    fn allocate_image_id(&mut self) -> u32 {
        let id = self.next_image_id.max(1);
        self.next_image_id = id.wrapping_add(1).max(1);
        id
    }
}

fn kitty_error_message(error: &io::Error) -> String {
    let code = match error.kind() {
        io::ErrorKind::NotFound => "ENOENT",
        io::ErrorKind::Unsupported => "ENOTSUP",
        _ => "EINVAL",
    };
    format!("{code}: {error}")
}

fn kitty_success_response(
    command: &KittyCommand,
    image_id: u32,
    message: &str,
) -> Option<KittyEvent> {
    if command.quiet > 0 {
        return None;
    }
    Some(KittyEvent::Response(kitty_response_full(
        image_id,
        command.image_number,
        command.placement_id,
        message,
    )))
}

fn kitty_error_response(
    command: &KittyCommand,
    image_id: u32,
    message: &str,
) -> Option<KittyEvent> {
    if command.quiet > 1 {
        return None;
    }
    Some(KittyEvent::Response(kitty_response_full(
        image_id,
        command.image_number,
        command.placement_id,
        message,
    )))
}

/// Encodes a Kitty graphics acknowledgement for an image and optional placement id.
///
/// The wire format is `<ESC>_Gi=<id>[,p=<placement id>];<message><ESC>\\` per official Kitty graphics specification.
#[must_use]
#[cfg(test)]
pub fn kitty_response(image_id: u32, placement_id: Option<u32>, message: &str) -> Vec<u8> {
    kitty_response_full(image_id, None, placement_id, message)
}

/// Encodes a Kitty acknowledgement including an optional `I=` image number.
#[must_use]
pub fn kitty_response_full(
    image_id: u32,
    image_number: Option<u32>,
    placement_id: Option<u32>,
    message: &str,
) -> Vec<u8> {
    let mut control = String::new();
    if image_id != 0 {
        control.push_str(&format!("i={image_id}"));
    }
    if let Some(number) = image_number.filter(|number| *number != 0) {
        if !control.is_empty() {
            control.push(',');
        }
        control.push_str(&format!("I={number}"));
    }
    if let Some(placement_id) = placement_id.filter(|id| *id != 0) {
        if !control.is_empty() {
            control.push(',');
        }
        control.push_str(&format!("p={placement_id}"));
    }
    if control.is_empty() {
        control.push_str("i=0");
    }
    format!("\x1b_G{control};{message}\x1b\\").into_bytes()
}
