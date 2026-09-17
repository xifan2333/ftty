//! Native Kitty Graphics Protocol APC sequence parsing, chunking, and image decoding.

pub mod command;
pub mod model;
pub(crate) mod payload;

#[cfg(test)]
mod tests;

pub use command::parse_control_keys;
pub use model::{
    DeleteTarget, ImageData, ImagePlacement, KittyAction, KittyCommand, KittyEvent, KittyFormat,
    KittyMedium,
};
use payload::decode_image_data;

const MAX_APC_PAYLOAD: usize = 32 * 1024 * 1024;

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
    pub(crate) in_apc: bool,
    apc_buffer: Vec<u8>,
    pub(crate) pending_stream: Vec<u8>,
    chunked_command: Option<KittyCommand>,
    chunked_payload: Vec<u8>,
    next_image_id: u32,
    pub(crate) clean_scratch: Vec<u8>,
    pub(crate) events_scratch: Vec<KittyEvent>,
}

impl KittyParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_image_id: 1,
            ..Default::default()
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
    pub fn filter_bytes(&mut self, incoming: &[u8]) -> (Vec<u8>, Vec<KittyEvent>) {
        if self.is_fast_path(incoming) {
            return (incoming.to_vec(), Vec::new());
        }

        self.filter_internal(incoming);
        let text = self.clean_scratch.clone();
        self.clean_scratch.clear();
        let events = std::mem::take(&mut self.events_scratch);
        (text, events)
    }

    fn filter_internal(&mut self, incoming: &[u8]) {
        self.clean_scratch.clear();
        self.events_scratch.clear();

        let mut bytes_buf;
        let bytes: &[u8] = if self.pending_stream.is_empty() {
            incoming
        } else {
            bytes_buf = std::mem::take(&mut self.pending_stream);
            bytes_buf.extend_from_slice(incoming);
            &bytes_buf
        };

        let mut i = 0;

        while i < bytes.len() {
            if self.in_apc {
                if self.apc_buffer.len() > MAX_APC_PAYLOAD {
                    self.in_apc = false;
                    self.apc_buffer.clear();
                    self.chunked_payload.clear();
                    self.chunked_command = None;
                    i += 1;
                    continue;
                }

                // Look for APC terminator: \x1b\ (ST) or \x07 (BEL)
                let byte = bytes[i];
                if byte == 0x07 {
                    self.in_apc = false;
                    if let Some(event) = self.finish_apc() {
                        self.events_scratch.push(event);
                    }
                    self.apc_buffer.clear();
                    i += 1;
                } else if byte == 0x1b {
                    if i + 1 < bytes.len() {
                        if bytes[i + 1] == b'\\' {
                            self.in_apc = false;
                            if let Some(event) = self.finish_apc() {
                                self.events_scratch.push(event);
                            }
                            self.apc_buffer.clear();
                            i += 2;
                        } else {
                            self.apc_buffer.push(byte);
                            i += 1;
                        }
                    } else {
                        self.pending_stream.push(byte);
                        break;
                    }
                } else {
                    self.apc_buffer.push(byte);
                    i += 1;
                }
            } else if bytes[i] == 0x1b {
                if i + 2 < bytes.len() {
                    if bytes[i + 1] == b'_' && bytes[i + 2] == b'G' {
                        self.in_apc = true;
                        self.apc_buffer.clear();
                        i += 3;
                    } else {
                        self.clean_scratch.push(bytes[i]);
                        i += 1;
                    }
                } else if i + 1 < bytes.len() {
                    if bytes[i + 1] == b'_' {
                        self.pending_stream.extend_from_slice(&bytes[i..]);
                        break;
                    } else {
                        self.clean_scratch.push(bytes[i]);
                        i += 1;
                    }
                } else {
                    self.pending_stream.push(bytes[i]);
                    break;
                }
            } else {
                self.clean_scratch.push(bytes[i]);
                i += 1;
            }
        }
    }

    fn finish_apc(&mut self) -> Option<KittyEvent> {
        let buffer = &self.apc_buffer;
        let semicolon_pos = buffer.iter().position(|&b| b == b';')?;
        let (keys_bytes, payload_bytes) = buffer.split_at(semicolon_pos);
        let payload_bytes = &payload_bytes[1..]; // skip semicolon

        let keys_str = std::str::from_utf8(keys_bytes).ok()?;
        let command = parse_control_keys(keys_str);

        // Capability probing query (a=q)
        if command.action == KittyAction::Query {
            let id = command.image_id.unwrap_or(0);
            return Some(KittyEvent::Response(kitty_response(id, None, "OK")));
        }

        // Delete action (a=d)
        if command.action == KittyAction::Delete {
            return Some(KittyEvent::Delete {
                target: command.delete_target,
            });
        }

        // Placement of existing image (a=p)
        if command.action == KittyAction::Place {
            return Some(KittyEvent::Place { command });
        }

        // Handle chunking (m=1 / m=0)
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

        let image_id = full_command.image_id.unwrap_or_else(|| {
            let id = self.next_image_id;
            self.next_image_id += 1;
            id
        });

        // Decode payload to raw image data
        match decode_image_data(image_id, &full_command, &raw_payload) {
            Ok(image) => {
                let mut cmd = full_command;
                cmd.image_id = Some(image_id);
                Some(KittyEvent::Transmit {
                    command: cmd,
                    image,
                })
            }
            Err(err) => {
                if full_command.quiet < 2 {
                    Some(KittyEvent::Response(kitty_response(
                        image_id,
                        full_command.placement_id,
                        &err.to_string(),
                    )))
                } else {
                    None
                }
            }
        }
    }
}

/// Encodes a Kitty graphics acknowledgement for an image and optional placement id.
///
/// The wire format is `<ESC>_Gi=<id>[,p=<placement id>];<message><ESC>\\`.
#[must_use]
pub fn kitty_response(image_id: u32, placement_id: Option<u32>, message: &str) -> Vec<u8> {
    match placement_id.filter(|id| *id != 0) {
        Some(placement_id) => {
            format!("\x1b_Gi={image_id},p={placement_id};{message}\x1b\\").into_bytes()
        }
        None => format!("\x1b_Gi={image_id};{message}\x1b\\").into_bytes(),
    }
}
