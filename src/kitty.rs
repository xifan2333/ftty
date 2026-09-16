//! Native Kitty Graphics Protocol APC sequence parser and image loader.

use std::fs;
use std::io::{self, Cursor, Read};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use nix::fcntl::OFlag;
use nix::sys::mman::shm_open;
use nix::sys::stat::Mode;

/// Kitty graphics action requested by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyAction {
    #[default]
    TransmitAndDisplay,
    TransmitAndDisplayWithResponse,
    Query,
    Place,
    Delete,
}

/// Pixel format of image payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyFormat {
    #[default]
    Rgba32,
    Rgb24,
    Png,
}

/// Transmission medium for image payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyMedium {
    #[default]
    Direct,
    File,
    TempFile,
    SharedMemory,
}

/// Delete target selector for `a=d`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum DeleteTarget {
    #[default]
    All,
    ById(u32),
    ByPlacement(u32),
    AtCursor,
}

/// Parsed Kitty APC command parameters.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct KittyCommand {
    pub action: KittyAction,
    pub format: KittyFormat,
    pub medium: KittyMedium,
    pub delete_target: DeleteTarget,
    pub image_id: Option<u32>,
    /// Whether the client sent an explicit `i=` key. The protocol only requires an
    /// `OK`/error acknowledgement when the client opted in by supplying an image id.
    pub id_explicit: bool,
    pub placement_id: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
    pub more_chunks: bool,
    pub do_not_move_cursor: bool,
    pub quiet: u8,
    pub is_virtual: bool,
}

/// Loaded RGBA image data ready for GPU texture upload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageData {
    pub id: u32,
    pub width: u32,
    pub height: u32,
    pub rgba: Vec<u8>,
}

/// On-screen image placement anchored to grid coordinates.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImagePlacement {
    pub image_id: u32,
    pub placement_id: u32,
    pub line: usize,
    pub col: usize,
    pub cols: usize,
    pub rows: usize,
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
}

/// Event emitted by the Kitty APC parser.
#[derive(Debug, Clone, PartialEq)]
pub enum KittyEvent {
    /// Transmit and place new image
    Transmit {
        command: KittyCommand,
        image: ImageData,
    },
    /// Place an existing image by ID
    Place { command: KittyCommand },
    /// Delete image(s)
    Delete { target: DeleteTarget },
    /// Query support or acknowledge command; contains bytes to write to PTY
    Response(Vec<u8>),
}

const MAX_APC_PAYLOAD: usize = 32 * 1024 * 1024;
// A shared-memory APC contains only a name, so its backing object needs a separate cap.
const MAX_SHM_PAYLOAD: u64 = 32 * 1024 * 1024;

#[inline]
fn may_contain_kitty_apc(bytes: &[u8]) -> bool {
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && (i + 1 >= bytes.len() || bytes[i + 1] == b'_') {
            return true;
        }
        i += 1;
    }
    false
}

/// State machine intercepting Kitty APC graphics sequences (`\x1b_G...;payload\x1b\`) from the byte stream.
#[derive(Default)]
pub struct KittyParser {
    in_apc: bool,
    apc_buffer: Vec<u8>,
    pending_stream: Vec<u8>,
    chunked_command: Option<KittyCommand>,
    chunked_payload: Vec<u8>,
    next_image_id: u32,
    clean_scratch: Vec<u8>,
    events_scratch: Vec<KittyEvent>,
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

/// Parses the comma-separated control keys string into a `KittyCommand`.
pub fn parse_control_keys(s: &str) -> KittyCommand {
    let mut cmd = KittyCommand::default();
    let mut delete_selector = None;

    for pair in s.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let val = parts.next().unwrap_or("").trim();

        match key {
            "a" => {
                cmd.action = match val {
                    "t" => KittyAction::TransmitAndDisplay,
                    "T" => KittyAction::TransmitAndDisplayWithResponse,
                    "q" => KittyAction::Query,
                    "p" => KittyAction::Place,
                    "d" => KittyAction::Delete,
                    _ => KittyAction::TransmitAndDisplay,
                };
            }
            "f" => {
                cmd.format = match val {
                    "32" => KittyFormat::Rgba32,
                    "24" => KittyFormat::Rgb24,
                    "100" => KittyFormat::Png,
                    _ => KittyFormat::Rgba32,
                };
            }
            "t" => {
                cmd.medium = match val {
                    "d" => KittyMedium::Direct,
                    "f" => KittyMedium::File,
                    "t" => KittyMedium::TempFile,
                    "s" => KittyMedium::SharedMemory,
                    _ => KittyMedium::Direct,
                };
            }
            "d" => {
                delete_selector = Some(val);
            }
            "s" => cmd.width = val.parse().ok(),
            "v" => cmd.height = val.parse().ok(),
            "i" => {
                cmd.image_id = val.parse().ok();
                cmd.id_explicit = true;
            }
            "p" => cmd.placement_id = val.parse().ok(),
            "c" => cmd.cols = val.parse().ok(),
            "r" => cmd.rows = val.parse().ok(),
            "X" => cmd.offset_x = val.parse().unwrap_or(0),
            "Y" => cmd.offset_y = val.parse().unwrap_or(0),
            "z" => cmd.z_index = val.parse().unwrap_or(0),
            "m" => cmd.more_chunks = val == "1",
            "C" => cmd.do_not_move_cursor = val == "1",
            "U" => cmd.is_virtual = val == "1",
            "q" => cmd.quiet = val.parse().unwrap_or(0),
            _ => {}
        }
    }

    if let Some(val) = delete_selector {
        cmd.delete_target = match val {
            "a" | "A" => DeleteTarget::All,
            "c" | "C" => DeleteTarget::AtCursor,
            "i" | "I" => cmd.image_id.map_or(DeleteTarget::All, DeleteTarget::ById),
            "p" | "P" => cmd
                .placement_id
                .map_or(DeleteTarget::All, DeleteTarget::ByPlacement),
            _ => DeleteTarget::All,
        };
    }

    cmd
}

fn decode_image_data(id: u32, cmd: &KittyCommand, raw_payload: &[u8]) -> io::Result<ImageData> {
    let bytes = match cmd.medium {
        KittyMedium::Direct => {
            // Direct payload: Base64 encoded data
            BASE64_STANDARD
                .decode(raw_payload)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?
        }
        KittyMedium::File | KittyMedium::TempFile => {
            let path_bytes = BASE64_STANDARD
                .decode(raw_payload)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let path_str = std::str::from_utf8(&path_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let path = std::path::Path::new(path_str);
            let data = fs::read(path)?;
            if cmd.medium == KittyMedium::TempFile {
                let temp_dir = std::env::temp_dir();
                // Security: only remove file if it resides in an approved temporary directory
                if path.starts_with(&temp_dir)
                    || path.starts_with("/tmp")
                    || path.starts_with("/dev/shm")
                {
                    let _ = fs::remove_file(path);
                }
            }
            data
        }
        KittyMedium::SharedMemory => {
            let name_bytes = BASE64_STANDARD
                .decode(raw_payload)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let name_str = std::str::from_utf8(&name_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            read_shm_payload(name_str)?
        }
    };

    match cmd.format {
        KittyFormat::Png => decode_png(id, &bytes),
        KittyFormat::Rgba32 => {
            let width = cmd.width.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing width for raw RGBA")
            })?;
            let height = cmd.height.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing height for raw RGBA")
            })?;
            let expected_len = (width as usize) * (height as usize) * 4;
            if bytes.len() != expected_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "raw RGBA byte size mismatch: expected {expected_len}, got {}",
                        bytes.len()
                    ),
                ));
            }
            Ok(ImageData {
                id,
                width,
                height,
                rgba: bytes,
            })
        }
        KittyFormat::Rgb24 => {
            let width = cmd.width.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing width for raw RGB")
            })?;
            let height = cmd.height.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing height for raw RGB")
            })?;
            let expected_len = (width as usize) * (height as usize) * 3;
            if bytes.len() != expected_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "raw RGB byte size mismatch: expected {expected_len}, got {}",
                        bytes.len()
                    ),
                ));
            }
            let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for chunk in bytes.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            Ok(ImageData {
                id,
                width,
                height,
                rgba,
            })
        }
    }
}

fn read_shm_payload(name: &str) -> io::Result<Vec<u8>> {
    let file = fs::File::from(shm_open(name, OFlag::O_RDONLY, Mode::empty())?);
    let size = file.metadata()?.len();
    read_shm_bytes(file, size)
}

fn read_shm_bytes(mut file: fs::File, size: u64) -> io::Result<Vec<u8>> {
    if size > MAX_SHM_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared memory exceeds the 32 MiB image payload limit",
        ));
    }

    // The sender may resize its object at any time. Read into owned memory so a
    // concurrent truncate returns an I/O error instead of faulting mapped pages.
    let mut bytes = vec![0; size as usize];
    file.read_exact(&mut bytes)?;
    if file.metadata()?.len() != size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared memory changed size during transfer",
        ));
    }

    Ok(bytes)
}

fn decode_png(id: u32, bytes: &[u8]) -> io::Result<ImageData> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let mut buf = vec![0; reader.output_buffer_size().unwrap_or(4096)];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let width = info.width;
    let height = info.height;

    let (color_type, _) = reader.output_color_type();
    let rgba = match color_type {
        png::ColorType::Rgba => buf[..info.buffer_size()].to_vec(),
        png::ColorType::Rgb => {
            let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for chunk in buf[..info.buffer_size()].as_chunks::<3>().0 {
                out.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            out
        }
        png::ColorType::Grayscale => {
            let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for &gray in &buf[..info.buffer_size()] {
                out.extend_from_slice(&[gray, gray, gray, 255]);
            }
            out
        }
        png::ColorType::GrayscaleAlpha => {
            let mut out = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for chunk in buf[..info.buffer_size()].as_chunks::<2>().0 {
                out.extend_from_slice(&[chunk[0], chunk[0], chunk[0], chunk[1]]);
            }
            out
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "unsupported PNG color format",
            ));
        }
    };

    Ok(ImageData {
        id,
        width,
        height,
        rgba,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_memory_loads_images_and_rejects_oversized_payloads() {
        use nix::sys::mman::shm_unlink;
        use std::io::Write;

        struct ShmName(String);
        impl Drop for ShmName {
            fn drop(&mut self) {
                let _ = shm_unlink(self.0.as_str());
            }
        }

        let name = format!("/ftty-kitty-test-{}", std::process::id());
        let fd = shm_open(
            name.as_str(),
            OFlag::O_CREAT | OFlag::O_EXCL | OFlag::O_RDWR,
            Mode::S_IRUSR | Mode::S_IWUSR,
        )
        .unwrap();
        let name = ShmName(name);
        let mut file = fs::File::from(fd);
        assert_eq!(read_shm_payload(&name.0).unwrap(), Vec::<u8>::new());

        let pixels = [255, 128, 0, 255];
        file.write_all(&pixels).unwrap();
        let command = KittyCommand {
            medium: KittyMedium::SharedMemory,
            width: Some(1),
            height: Some(1),
            ..Default::default()
        };
        let image =
            decode_image_data(7, &command, BASE64_STANDARD.encode(&name.0).as_bytes()).unwrap();
        assert_eq!(image.rgba, pixels);
        assert_eq!((image.id, image.width, image.height), (7, 1, 1));

        // A sparse object can exceed the cap without consuming that much RAM.
        file.set_len(MAX_SHM_PAYLOAD + 1).unwrap();
        assert_eq!(
            read_shm_payload(&name.0).unwrap_err().kind(),
            io::ErrorKind::InvalidData,
        );
    }

    #[test]
    fn shared_memory_size_changes_are_rejected() {
        use nix::sys::memfd::{MFdFlags, memfd_create};

        for (initial, changed, error_kind) in [
            (4, 2, io::ErrorKind::UnexpectedEof),
            (4, 8, io::ErrorKind::InvalidData),
            (0, 1, io::ErrorKind::InvalidData),
        ] {
            let fd = memfd_create(c"ftty-shm-resize-test", MFdFlags::MFD_CLOEXEC).unwrap();
            let file = fs::File::from(fd);
            file.set_len(initial).unwrap();
            let observed_size = file.metadata().unwrap().len();

            // Deterministically simulate the sender resizing after the size query.
            file.set_len(changed).unwrap();
            assert_eq!(
                read_shm_bytes(file, observed_size).unwrap_err().kind(),
                error_kind,
            );
        }
    }

    #[test]
    fn test_parse_kitty_query() {
        let mut parser = KittyParser::new();
        let (text, events) = parser.filter_bytes(b"\x1b_Gi=42,a=q;\x1b\\");
        assert!(text.is_empty());
        assert_eq!(events.len(), 1);
        match &events[0] {
            KittyEvent::Response(resp) => {
                assert_eq!(resp, b"\x1b_Gi=42;OK\x1b\\");
            }
            other => panic!("Expected query response, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_kitty_direct_rgba() {
        let mut parser = KittyParser::new();
        // 2x1 RGBA: [255, 0, 0, 255, 0, 255, 0, 255]
        let raw_pixels = [255u8, 0, 0, 255, 0, 255, 0, 255];
        let b64 = BASE64_STANDARD.encode(raw_pixels);
        let seq = format!("\x1b_Ga=t,f=32,s=2,v=1,i=10;{b64}\x1b\\");

        let (text, events) = parser.filter_bytes(seq.as_bytes());
        assert!(text.is_empty());
        assert_eq!(events.len(), 1);

        match &events[0] {
            KittyEvent::Transmit { command, image } => {
                assert_eq!(command.image_id, Some(10));
                assert_eq!(image.width, 2);
                assert_eq!(image.height, 1);
                assert_eq!(image.rgba, raw_pixels.to_vec());
            }
            other => panic!("Expected Transmit, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_kitty_chunking() {
        let mut parser = KittyParser::new();
        let raw_pixels = [10u8, 20, 30, 255];
        let b64 = BASE64_STANDARD.encode(raw_pixels);
        let part1 = &b64[..2];
        let part2 = &b64[2..];

        let seq1 = format!("\x1b_Ga=t,f=32,s=1,v=1,i=5,m=1;{part1}\x1b\\");
        let seq2 = format!("\x1b_Gm=0;{part2}\x1b\\");

        let (t1, e1) = parser.filter_bytes(seq1.as_bytes());
        assert!(t1.is_empty());
        assert!(e1.is_empty());

        let (t2, e2) = parser.filter_bytes(seq2.as_bytes());
        assert!(t2.is_empty());
        assert_eq!(e2.len(), 1);

        match &e2[0] {
            KittyEvent::Transmit { command, image } => {
                assert_eq!(command.image_id, Some(5));
                assert_eq!(image.rgba, raw_pixels.to_vec());
            }
            other => panic!("Expected Transmit, got {:?}", other),
        }
    }

    #[test]
    fn test_parse_kitty_delete() {
        let mut parser = KittyParser::new();
        let (text, events) = parser.filter_bytes(b"\x1b_Ga=d,d=a;\x1b\\");
        assert!(text.is_empty());
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            KittyEvent::Delete {
                target: DeleteTarget::All
            }
        );
    }

    #[test]
    fn test_parse_kitty_png_format() {
        // Create 1x1 red PNG
        let mut png_bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut png_bytes, 1, 1);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            let mut writer = encoder.write_header().unwrap();
            writer.write_image_data(&[255, 0, 0, 255]).unwrap();
        }

        let b64 = BASE64_STANDARD.encode(&png_bytes);
        let seq = format!("\x1b_Ga=t,f=100,i=99;{b64}\x1b\\");

        let mut parser = KittyParser::new();
        let (text, events) = parser.filter_bytes(seq.as_bytes());
        assert!(text.is_empty());
        assert_eq!(events.len(), 1);

        match &events[0] {
            KittyEvent::Transmit { command, image } => {
                assert_eq!(command.image_id, Some(99));
                assert_eq!(image.width, 1);
                assert_eq!(image.height, 1);
                assert_eq!(image.rgba, vec![255, 0, 0, 255]);
            }
            other => panic!("Expected Transmit with PNG, got {:?}", other),
        }
    }

    #[test]
    fn test_kitty_transmit_with_response() {
        let raw = [0u8, 0, 255, 255];
        let b64 = BASE64_STANDARD.encode(raw);
        let seq = format!("\x1b_Ga=T,f=32,s=1,v=1,i=77;{b64}\x1b\\");

        let mut parser = KittyParser::new();
        let (text, events) = parser.filter_bytes(seq.as_bytes());
        assert!(text.is_empty());
        assert_eq!(events.len(), 1);

        match &events[0] {
            KittyEvent::Transmit { command, image } => {
                assert_eq!(command.action, KittyAction::TransmitAndDisplayWithResponse);
                assert_eq!(command.image_id, Some(77));
                assert_eq!(image.rgba, raw.to_vec());
            }
            other => panic!("Expected Transmit with response, got {:?}", other),
        }
    }

    #[test]
    fn test_split_apc_framing() {
        let mut parser = KittyParser::new();
        let (t1, e1) = parser.filter_bytes(b"hello\x1b");
        assert_eq!(t1, b"hello");
        assert!(e1.is_empty());

        let (t2, e2) = parser.filter_bytes(b"_Gi=1,a=q;\x1b");
        assert!(t2.is_empty());
        assert!(e2.is_empty());

        let (t3, e3) = parser.filter_bytes(b"\\world");
        assert_eq!(t3, b"world");
        assert_eq!(e3.len(), 1);
        assert_eq!(e3[0], KittyEvent::Response(b"\x1b_Gi=1;OK\x1b\\".to_vec()));
    }

    #[test]
    fn test_order_independent_delete() {
        let cmd = parse_control_keys("d=i,i=7,a=d");
        assert_eq!(cmd.action, KittyAction::Delete);
        assert_eq!(cmd.delete_target, DeleteTarget::ById(7));
    }

    #[test]
    fn explicit_image_id_tracking() {
        assert!(parse_control_keys("a=t,i=7").id_explicit);
        assert!(parse_control_keys("i=7").image_id == Some(7));
        assert!(!parse_control_keys("a=t").id_explicit);
        assert!(parse_control_keys("a=t").image_id.is_none());
    }

    #[test]
    fn response_encoding_includes_only_real_placement_ids() {
        assert_eq!(
            kitty_response(3, None, "OK"),
            b"\x1b_Gi=3;OK\x1b\\".to_vec()
        );
        assert_eq!(
            kitty_response(3, Some(0), "OK"),
            b"\x1b_Gi=3;OK\x1b\\".to_vec()
        );
        assert_eq!(
            kitty_response(3, Some(5), "ENOENT:image not found"),
            b"\x1b_Gi=3,p=5;ENOENT:image not found\x1b\\".to_vec()
        );
    }

    #[test]
    fn test_kitty_probe_interleaving() {
        let mut kitty_parser = KittyParser::new();
        let query = b"\x1b_Gi=31,s=1,v=1,a=q,t=d,f=24;AAAA\x1b\\\x1b[c\x1b[16t\x1b]11;?\x07\x1b[5n";
        let (clean, events) = kitty_parser.filter_bytes(query);
        println!("clean: {:?}", std::str::from_utf8(&clean).unwrap());
        println!("events: {:?}", events);
    }

    #[test]
    fn chunked_transmit_preserves_explicit_id_and_quiet() {
        let b64 = BASE64_STANDARD.encode([1u8, 2, 3, 4]);
        let first = format!("\x1b_Ga=t,f=32,s=1,v=1,i=9,q=0,m=1;{}\x1b\\", &b64[..2]);
        let last = format!("\x1b_Gm=0;{}\x1b\\", &b64[2..]);
        let mut parser = KittyParser::new();
        assert!(parser.filter_bytes(first.as_bytes()).1.is_empty());
        let (_, events) = parser.filter_bytes(last.as_bytes());
        match &events[0] {
            KittyEvent::Transmit { command, .. } => {
                assert!(command.id_explicit);
                assert_eq!(command.image_id, Some(9));
                assert_eq!(command.quiet, 0);
            }
            other => panic!("Expected Transmit, got {other:?}"),
        }
    }

    #[test]
    fn test_filter_fast_path_direct_borrow() {
        let mut parser = KittyParser::new();

        // 1. Plain text is fast-path eligible
        let plain = b"Hello world, normal text from cat or grep\n";
        assert!(parser.is_fast_path(plain));
        let (clean, events) = parser.filter_bytes(plain);
        assert_eq!(clean, plain);
        assert!(events.is_empty());

        // 2. ANSI color and cursor escapes are also fast-path eligible
        let ansi = b"\x1b[31mRed text\x1b[0m and \x1b[10;20Hcursor\x1b]8;;https://x.com\x1b\\";
        assert!(parser.is_fast_path(ansi));
        let (clean, events) = parser.filter_bytes(ansi);
        assert_eq!(clean, ansi);
        assert!(events.is_empty());

        // 3. Kitty graphics sequence requires slow-path filtering
        let kitty_payload = b"before\x1b_Gi=42,a=q;\x1b\\after";
        assert!(!parser.is_fast_path(kitty_payload));
        let (text, events) = parser.filter_bytes(kitty_payload);
        assert_eq!(text, b"beforeafter");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            KittyEvent::Response(b"\x1b_Gi=42;OK\x1b\\".to_vec())
        );
        // Events must be transferred out; parser must not retain image/event data
        assert!(parser.events_scratch.is_empty());
    }

    #[test]
    fn test_scratch_buffer_reused_without_reallocation() {
        let mut parser = KittyParser::new();
        let payload = b"start\x1b_Gi=10,a=q;\x1b\\end";

        // First call populates scratch buffer
        let _ = parser.filter_bytes(payload);
        let cap_text = parser.clean_scratch.capacity();
        assert!(cap_text > 0);
        // Events are drained so events_scratch does not keep image memory
        assert!(parser.events_scratch.is_empty());

        // Subsequent calls reuse existing capacity
        for _ in 0..100 {
            let _ = parser.filter_bytes(payload);
            assert_eq!(parser.clean_scratch.capacity(), cap_text);
            assert!(parser.events_scratch.is_empty());
        }
    }

    #[test]
    fn test_split_escape_crosses_chunk_into_processed() {
        let mut parser = KittyParser::new();
        // Chunk 1 ends with escape (not fast path)
        let chunk1 = b"text\x1b";
        assert!(!parser.is_fast_path(chunk1));
        let (text, events) = parser.filter_bytes(chunk1);
        assert_eq!(text, b"text");
        assert!(events.is_empty());

        // Chunk 2 completes the Kitty sequence
        let chunk2 = b"_Gi=99,a=q;\x1b\\more";
        assert!(!parser.is_fast_path(chunk2));
        let (text, events) = parser.filter_bytes(chunk2);
        assert_eq!(text, b"more");
        assert_eq!(events.len(), 1);
        assert_eq!(
            events[0],
            KittyEvent::Response(b"\x1b_Gi=99;OK\x1b\\".to_vec())
        );
        assert!(parser.events_scratch.is_empty());
    }

    #[test]
    fn test_is_fast_path() {
        let mut parser = KittyParser::new();
        // Plain text is fast path
        assert!(parser.is_fast_path(b"Hello world\n"));
        // ANSI escape codes (colors, cursor, hyperlinks) are fast path
        assert!(parser.is_fast_path(b"\x1b[31mRed\x1b[0m \x1b[10;20H \x1b]8;;https://x.com\x1b\\"));
        // Kitty sequence is NOT fast path
        assert!(!parser.is_fast_path(b"\x1b_Gi=1,a=q;\x1b\\"));
        // Chunk ending in 0x1b is NOT fast path (could split across read boundaries)
        assert!(!parser.is_fast_path(b"hello\x1b"));

        // While in APC, even plain text is NOT fast path
        parser.in_apc = true;
        assert!(!parser.is_fast_path(b"more data"));
    }
}
