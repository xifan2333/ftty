//! Native Kitty Graphics Protocol APC sequence parser and image loader.

use std::fs;
use std::io::{self, Cursor};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;

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

/// State machine intercepting Kitty APC graphics sequences (`\x1b_G...;payload\x1b\`) from the byte stream.
#[derive(Default)]
pub struct KittyParser {
    in_apc: bool,
    apc_buffer: Vec<u8>,
    pending_stream: Vec<u8>,
    chunked_command: Option<KittyCommand>,
    chunked_payload: Vec<u8>,
    next_image_id: u32,
}

impl KittyParser {
    #[must_use]
    pub fn new() -> Self {
        Self {
            next_image_id: 1,
            ..Default::default()
        }
    }

    /// Filters an incoming byte stream, stripping Kitty APC sequences and emitting parsed graphics events.
    ///
    /// Non-graphics bytes are returned in the first vector to be processed by the standard VT parser.
    pub fn filter_bytes(&mut self, incoming: &[u8]) -> (Vec<u8>, Vec<KittyEvent>) {
        let mut bytes_buf;
        let bytes: &[u8] = if self.pending_stream.is_empty() {
            incoming
        } else {
            bytes_buf = std::mem::take(&mut self.pending_stream);
            bytes_buf.extend_from_slice(incoming);
            &bytes_buf
        };

        let mut text_output = Vec::with_capacity(bytes.len());
        let mut events = Vec::new();
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
                        events.push(event);
                    }
                    self.apc_buffer.clear();
                    i += 1;
                } else if byte == 0x1b {
                    if i + 1 < bytes.len() {
                        if bytes[i + 1] == b'\\' {
                            self.in_apc = false;
                            if let Some(event) = self.finish_apc() {
                                events.push(event);
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
                        text_output.push(bytes[i]);
                        i += 1;
                    }
                } else if i + 1 < bytes.len() {
                    if bytes[i + 1] == b'_' {
                        self.pending_stream.extend_from_slice(&bytes[i..]);
                        break;
                    } else {
                        text_output.push(bytes[i]);
                        i += 1;
                    }
                } else {
                    self.pending_stream.push(bytes[i]);
                    break;
                }
            } else {
                text_output.push(bytes[i]);
                i += 1;
            }
        }

        (text_output, events)
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
            let response = format!("\x1b_Gi={id};OK\x1b\\").into_bytes();
            return Some(KittyEvent::Response(response));
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
                    let response = format!("\x1b_Gi={image_id};{err}\x1b\\").into_bytes();
                    Some(KittyEvent::Response(response))
                } else {
                    None
                }
            }
        }
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
            }
            "p" => cmd.placement_id = val.parse().ok(),
            "c" => cmd.cols = val.parse().ok(),
            "r" => cmd.rows = val.parse().ok(),
            "X" => cmd.offset_x = val.parse().unwrap_or(0),
            "Y" => cmd.offset_y = val.parse().unwrap_or(0),
            "z" => cmd.z_index = val.parse().unwrap_or(0),
            "m" => cmd.more_chunks = val == "1",
            "C" => cmd.do_not_move_cursor = val == "1",
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
    let c_name =
        std::ffi::CString::new(name).map_err(|e| io::Error::new(io::ErrorKind::InvalidInput, e))?;
    let fd = unsafe { libc::shm_open(c_name.as_ptr(), libc::O_RDONLY, 0) };
    if fd < 0 {
        return Err(io::Error::last_os_error());
    }

    let mut stat: libc::stat = unsafe { std::mem::zeroed() };
    if unsafe { libc::fstat(fd, &mut stat) } < 0 {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    let size = stat.st_size as usize;
    if size == 0 {
        unsafe { libc::close(fd) };
        return Ok(Vec::new());
    }

    let ptr = unsafe {
        libc::mmap(
            std::ptr::null_mut(),
            size,
            libc::PROT_READ,
            libc::MAP_SHARED,
            fd,
            0,
        )
    };

    if ptr == libc::MAP_FAILED {
        let err = io::Error::last_os_error();
        unsafe { libc::close(fd) };
        return Err(err);
    }

    let slice = unsafe { std::slice::from_raw_parts(ptr.cast::<u8>(), size) };
    let bytes = slice.to_vec();

    unsafe {
        libc::munmap(ptr, size);
        libc::close(fd);
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
}
