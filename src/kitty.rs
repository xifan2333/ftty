//! Native Kitty Graphics Protocol APC sequence parsing, chunking, and image decoding.

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use flate2::read::ZlibDecoder;
use nix::fcntl::OFlag;
use nix::sys::mman::{shm_open, shm_unlink};
use nix::sys::stat::Mode;
use std::collections::HashMap;
use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

// --- Model ---

/// Kitty graphics action requested by the client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum KittyAction {
    #[default]
    Transmit,
    TransmitAndDisplay,
    TransmitAndDisplayWithResponse,
    Query,
    Place,
    Delete,
    /// Animation frame transmission (`a=f`); recognized but unsupported.
    TransmitFrame,
    /// Animation control (`a=a`); recognized but unsupported.
    ControlAnimation,
    /// Animation frame composition (`a=c`); recognized but unsupported.
    ComposeFrame,
}

/// Payload compression requested through the `o=` key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KittyCompression {
    /// RFC 1950 zlib stream.
    Zlib,
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
    AllAndFree,
    ById(u32),
    ByIdAndFree(u32),
    /// Delete one placement of an image (`d=i` combined with `p=`).
    ByIdPlacement(u32, u32),
    ByIdPlacementAndFree(u32, u32),
    /// Newest image with a client image number (`d=n`), resolved by the parser.
    ByNumber {
        number: u32,
        placement: Option<u32>,
        free: bool,
    },
    AtCursor,
    AtCursorAndFree,
    /// Delete placements intersecting an absolute cell (`d=p`/`d=q`), 1-based.
    AtAbsoluteCell {
        x: u32,
        y: u32,
        z: Option<i32>,
        free: bool,
    },
    /// Delete placements intersecting a column (`d=x`), 1-based.
    ByColumn {
        x: u32,
        free: bool,
    },
    /// Delete placements intersecting a row (`d=y`), 1-based.
    ByRow {
        y: u32,
        free: bool,
    },
    /// Delete placements by z-index (`d=z`).
    ByZIndex(i32),
    ByZIndexAndFree(i32),
    /// Delete images whose id is within an inclusive range (`d=r`).
    ByIdRange {
        lo: u32,
        hi: u32,
        free: bool,
    },
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
    /// Image number requested through `I=`; the terminal allocates the actual id.
    pub image_number: Option<u32>,
    pub number_explicit: bool,
    pub placement_id: Option<u32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub cols: Option<u32>,
    pub rows: Option<u32>,
    /// Source crop rectangle (x, y, w, h) within the image.
    pub src_x: Option<u32>,
    pub src_y: Option<u32>,
    pub src_w: Option<u32>,
    pub src_h: Option<u32>,
    /// Cell-relative pixel offset (X, Y).
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
    /// Number of bytes to read for file/shared-memory payloads (`S=`).
    pub data_size: Option<u64>,
    /// Byte offset within a file/shared-memory payload (`O=`).
    pub data_offset: u64,
    /// Optional payload compression (`o=`).
    pub compression: Option<KittyCompression>,
    /// Whether `f=`/`t=` were explicitly supplied, for capability probes.
    pub format_explicit: bool,
    pub medium_explicit: bool,
    /// Standard error discovered while parsing the control data.
    pub error: Option<&'static str>,
    /// Parent image id for relative placements (`P=`), which are unsupported.
    pub parent_id: Option<u32>,
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
    pub rgba: Option<Vec<u8>>,
}

impl ImageData {
    #[must_use]
    pub fn new(id: u32, width: u32, height: u32, rgba: Vec<u8>) -> Self {
        Self {
            id,
            width,
            height,
            rgba: Some(rgba),
        }
    }

    #[must_use]
    pub fn byte_size(&self) -> usize {
        (self.width as usize)
            .saturating_mul(self.height as usize)
            .saturating_mul(4)
    }
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
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: Option<u32>,
    pub src_h: Option<u32>,
    pub offset_x: u32,
    pub offset_y: u32,
    pub z_index: i32,
}

impl Default for ImagePlacement {
    fn default() -> Self {
        Self {
            image_id: 0,
            placement_id: 0,
            line: 0,
            col: 0,
            cols: 1,
            rows: 1,
            src_x: 0,
            src_y: 0,
            src_w: None,
            src_h: None,
            offset_x: 0,
            offset_y: 0,
            z_index: 0,
        }
    }
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

// --- Command ---

/// Parses the comma-separated control keys string into a `KittyCommand`.
#[must_use]
pub fn parse_control_keys(s: &str) -> KittyCommand {
    let mut cmd = KittyCommand::default();
    let mut delete_selector: Option<&str> = None;

    fn set_error(cmd: &mut KittyCommand, message: &'static str) {
        if cmd.error.is_none() {
            cmd.error = Some(message);
        }
    }

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
                    "t" => KittyAction::Transmit,
                    "T" => KittyAction::TransmitAndDisplay,
                    "q" => KittyAction::Query,
                    "p" => KittyAction::Place,
                    "d" => KittyAction::Delete,
                    "f" => {
                        set_error(&mut cmd, "ENOTSUP: animation is not supported");
                        KittyAction::TransmitFrame
                    }
                    "a" => {
                        set_error(&mut cmd, "ENOTSUP: animation is not supported");
                        KittyAction::ControlAnimation
                    }
                    "c" => {
                        set_error(&mut cmd, "ENOTSUP: animation is not supported");
                        KittyAction::ComposeFrame
                    }
                    _ => {
                        set_error(&mut cmd, "EINVAL: unknown action");
                        KittyAction::Transmit
                    }
                };
            }
            "f" => {
                cmd.format_explicit = true;
                cmd.format = match val {
                    "32" => KittyFormat::Rgba32,
                    "24" => KittyFormat::Rgb24,
                    "100" => KittyFormat::Png,
                    _ => {
                        set_error(&mut cmd, "EINVAL: unknown image format");
                        KittyFormat::Rgba32
                    }
                };
            }
            "t" => {
                cmd.medium_explicit = true;
                cmd.medium = match val {
                    "d" => KittyMedium::Direct,
                    "f" => KittyMedium::File,
                    "t" => KittyMedium::TempFile,
                    "s" => KittyMedium::SharedMemory,
                    _ => {
                        set_error(&mut cmd, "EINVAL: unknown transmission medium");
                        KittyMedium::Direct
                    }
                };
            }
            "o" => match val {
                "z" => cmd.compression = Some(KittyCompression::Zlib),
                _ => set_error(&mut cmd, "EINVAL: unknown image compression"),
            },
            "d" => delete_selector = Some(val),
            "s" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid image width");
                    }
                    cmd.width = None;
                }
                Ok(width) => cmd.width = Some(width),
            },
            "v" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid image height");
                    }
                    cmd.height = None;
                }
                Ok(height) => cmd.height = Some(height),
            },
            "S" => match val.parse() {
                Ok(0) => cmd.data_size = None,
                Ok(size) => cmd.data_size = Some(size),
                Err(_) => set_error(&mut cmd, "EINVAL: invalid data size"),
            },
            "O" => match val.parse() {
                Ok(offset) => cmd.data_offset = offset,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid data offset"),
            },
            "i" => {
                cmd.id_explicit = true;
                match val.parse() {
                    Ok(0) | Err(_) => {
                        if val.parse::<u32>().is_err() {
                            set_error(&mut cmd, "EINVAL: invalid image id");
                        }
                        cmd.image_id = None;
                    }
                    Ok(id) => cmd.image_id = Some(id),
                }
            }
            "I" => {
                cmd.number_explicit = true;
                match val.parse() {
                    Ok(0) | Err(_) => {
                        if val.parse::<u32>().is_err() {
                            set_error(&mut cmd, "EINVAL: invalid image number");
                        }
                        cmd.image_number = None;
                    }
                    Ok(number) => cmd.image_number = Some(number),
                }
            }
            "p" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid placement id");
                    }
                    cmd.placement_id = None;
                }
                Ok(id) => cmd.placement_id = Some(id),
            },
            "c" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid column count");
                    }
                    cmd.cols = None;
                }
                Ok(cols) => cmd.cols = Some(cols),
            },
            "r" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid row count");
                    }
                    cmd.rows = None;
                }
                Ok(rows) => cmd.rows = Some(rows),
            },
            "x" => match val.parse() {
                Ok(x) => cmd.src_x = Some(x),
                Err(_) => set_error(&mut cmd, "EINVAL: invalid source x"),
            },
            "y" => match val.parse() {
                Ok(y) => cmd.src_y = Some(y),
                Err(_) => set_error(&mut cmd, "EINVAL: invalid source y"),
            },
            "w" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid source width");
                    }
                    cmd.src_w = None;
                }
                Ok(width) => cmd.src_w = Some(width),
            },
            "h" => match val.parse() {
                Ok(0) | Err(_) => {
                    if val.parse::<u32>().is_err() {
                        set_error(&mut cmd, "EINVAL: invalid source height");
                    }
                    cmd.src_h = None;
                }
                Ok(height) => cmd.src_h = Some(height),
            },
            "X" => match val.parse() {
                Ok(offset) => cmd.offset_x = offset,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid x offset"),
            },
            "Y" => match val.parse() {
                Ok(offset) => cmd.offset_y = offset,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid y offset"),
            },
            "z" => match val.parse() {
                Ok(z) => cmd.z_index = z,
                Err(_) => set_error(&mut cmd, "EINVAL: invalid z-index"),
            },
            "P" => {
                match val.parse() {
                    Ok(parent) => cmd.parent_id = Some(parent),
                    Err(_) => set_error(&mut cmd, "EINVAL: invalid parent image id"),
                }
                set_error(&mut cmd, "ENOTSUP: relative placements are not supported");
            }
            "m" => match val {
                "0" => cmd.more_chunks = false,
                "1" => cmd.more_chunks = true,
                _ => set_error(&mut cmd, "EINVAL: invalid chunk flag"),
            },
            "C" => match val {
                "0" => cmd.do_not_move_cursor = false,
                "1" => cmd.do_not_move_cursor = true,
                _ => set_error(&mut cmd, "EINVAL: invalid cursor flag"),
            },
            "U" => match val {
                "0" => cmd.is_virtual = false,
                "1" => cmd.is_virtual = true,
                _ => set_error(&mut cmd, "EINVAL: invalid virtual placement flag"),
            },
            "q" => match val {
                "0" => cmd.quiet = 0,
                "1" => cmd.quiet = 1,
                "2" => cmd.quiet = 2,
                _ => set_error(&mut cmd, "EINVAL: invalid quiet level"),
            },
            // Usage hints are advisory; ftty does not need to act on them.
            "N" => {}
            _ => {}
        }
    }

    if cmd.image_id.is_some() && cmd.image_number.is_some() {
        set_error(&mut cmd, "EINVAL: both image id and image number specified");
    }

    if let Some(val) = delete_selector {
        let x = cmd.src_x.unwrap_or(0);
        let y = cmd.src_y.unwrap_or(0);
        cmd.delete_target = match val {
            "a" => DeleteTarget::All,
            "A" => DeleteTarget::AllAndFree,
            "c" => DeleteTarget::AtCursor,
            "C" => DeleteTarget::AtCursorAndFree,
            "i" => match (cmd.image_id, cmd.placement_id) {
                (Some(id), Some(placement)) => DeleteTarget::ByIdPlacement(id, placement),
                (Some(id), None) => DeleteTarget::ById(id),
                (None, _) => {
                    set_error(&mut cmd, "EINVAL: image id required");
                    DeleteTarget::All
                }
            },
            "I" => match (cmd.image_id, cmd.placement_id) {
                (Some(id), Some(placement)) => DeleteTarget::ByIdPlacementAndFree(id, placement),
                (Some(id), None) => DeleteTarget::ByIdAndFree(id),
                (None, _) => {
                    set_error(&mut cmd, "EINVAL: image id required");
                    DeleteTarget::AllAndFree
                }
            },
            "n" | "N" => match cmd.image_number {
                Some(number) => DeleteTarget::ByNumber {
                    number,
                    placement: cmd.placement_id,
                    free: val == "N",
                },
                None => {
                    set_error(&mut cmd, "EINVAL: image number required");
                    DeleteTarget::All
                }
            },
            "p" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: None,
                free: false,
            },
            "P" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: None,
                free: true,
            },
            "q" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: Some(cmd.z_index),
                free: false,
            },
            "Q" => DeleteTarget::AtAbsoluteCell {
                x,
                y,
                z: Some(cmd.z_index),
                free: true,
            },
            "r" => DeleteTarget::ByIdRange {
                lo: x,
                hi: y,
                free: false,
            },
            "R" => DeleteTarget::ByIdRange {
                lo: x,
                hi: y,
                free: true,
            },
            "x" => DeleteTarget::ByColumn { x, free: false },
            "X" => DeleteTarget::ByColumn { x, free: true },
            "y" => DeleteTarget::ByRow { y, free: false },
            "Y" => DeleteTarget::ByRow { y, free: true },
            "z" => DeleteTarget::ByZIndex(cmd.z_index),
            "Z" => DeleteTarget::ByZIndexAndFree(cmd.z_index),
            "f" | "F" => {
                set_error(&mut cmd, "ENOTSUP: animation is not supported");
                DeleteTarget::All
            }
            _ => {
                set_error(&mut cmd, "ENOTSUP: unsupported delete selector");
                DeleteTarget::All
            }
        };
    } else {
        cmd.delete_target = DeleteTarget::All;
    }

    cmd
}

// --- Payload ---

#[doc(hidden)]
pub const MAX_SHM_PAYLOAD: u64 = 32 * 1024 * 1024;

/// Upper bound for a zlib-compressed payload after inflation. This matches the
/// grid's total image budget so a crafted stream cannot exhaust memory.
pub const MAX_DECOMPRESSED_PAYLOAD: usize = 64 * 1024 * 1024;

#[doc(hidden)]
pub fn decode_image_data(id: u32, cmd: &KittyCommand, raw_payload: &[u8]) -> io::Result<ImageData> {
    if let Some(error) = cmd.error {
        return Err(io::Error::new(io::ErrorKind::InvalidInput, error));
    }

    let bytes = read_payload_bytes(cmd, raw_payload)?;
    let bytes = match cmd.compression {
        Some(KittyCompression::Zlib) => inflate_zlib(&bytes)?,
        None => bytes,
    };
    decode_image_bytes(id, cmd, bytes)
}

fn decode_image_bytes(id: u32, cmd: &KittyCommand, bytes: Vec<u8>) -> io::Result<ImageData> {
    match cmd.format {
        KittyFormat::Png => decode_png(id, &bytes),
        KittyFormat::Rgba32 => {
            let width = cmd.width.filter(|&w| w > 0).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing width for raw RGBA")
            })?;
            let height = cmd.height.filter(|&h| h > 0).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing height for raw RGBA")
            })?;
            let expected_len = (width as usize)
                .checked_mul(height as usize)
                .and_then(|size| size.checked_mul(4))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "image size overflow"))?;
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
                rgba: Some(bytes),
            })
        }
        KittyFormat::Rgb24 => {
            let width = cmd.width.filter(|&w| w > 0).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing width for raw RGB")
            })?;
            let height = cmd.height.filter(|&h| h > 0).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing height for raw RGB")
            })?;
            let expected_len = (width as usize)
                .checked_mul(height as usize)
                .and_then(|size| size.checked_mul(3))
                .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "image size overflow"))?;
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
                rgba: Some(rgba),
            })
        }
    }
}

fn validate_capability_query(cmd: &KittyCommand, raw_payload: &[u8]) -> io::Result<()> {
    let bytes = read_payload_bytes(cmd, raw_payload)?;
    let bytes = match cmd.compression {
        Some(KittyCompression::Zlib) => inflate_zlib(&bytes)?,
        None => bytes,
    };
    // When the probe supplies image dimensions or a self-describing PNG, validate
    // the complete image. Otherwise it is a transport/encoding probe (e.g. viuer
    // testing `t=t` file access without s/v/f) and successful reading is enough.
    if cmd.format == KittyFormat::Png || cmd.width.is_some() || cmd.height.is_some() {
        let _ = decode_image_bytes(0, cmd, bytes)?;
    }
    Ok(())
}

fn read_payload_bytes(cmd: &KittyCommand, raw_payload: &[u8]) -> io::Result<Vec<u8>> {
    match cmd.medium {
        KittyMedium::Direct => BASE64_STANDARD
            .decode(raw_payload)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error)),
        KittyMedium::File | KittyMedium::TempFile => {
            let path = decode_medium_name(raw_payload)?;
            let path = std::path::Path::new(&path);
            let result = read_file_range(path, cmd.data_offset, cmd.data_size);
            if cmd.medium == KittyMedium::TempFile && is_temp_payload(path) {
                let _ = fs::remove_file(path);
            }
            result
        }
        KittyMedium::SharedMemory => {
            let name = decode_medium_name(raw_payload)?;
            read_shm_payload_for_image(&name, cmd.data_offset, cmd.data_size)
        }
    }
}

fn decode_medium_name(raw_payload: &[u8]) -> io::Result<String> {
    let decoded = BASE64_STANDARD
        .decode(raw_payload)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    String::from_utf8(decoded).map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
}

fn is_temp_payload(path: &std::path::Path) -> bool {
    let temp_dir = std::env::temp_dir();
    path.starts_with(&temp_dir) || path.starts_with("/tmp") || path.starts_with("/dev/shm")
}

fn bounded_read_length(available: u64, requested: Option<u64>) -> io::Result<u64> {
    let length = requested.filter(|&size| size > 0).unwrap_or(available);
    if length > MAX_SHM_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload exceeds the 32 MiB image payload limit",
        ));
    }
    if length > available {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload is smaller than the requested data size",
        ));
    }
    Ok(length)
}

fn read_file_range(path: &std::path::Path, offset: u64, size: Option<u64>) -> io::Result<Vec<u8>> {
    let mut file = fs::File::open(path)?;
    let total = file.metadata()?.len();
    if offset > total {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "payload offset exceeds the file size",
        ));
    }
    let length = bounded_read_length(total - offset, size)?;
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = vec![0; length as usize];
    file.read_exact(&mut bytes)?;
    Ok(bytes)
}

fn read_shm_payload_for_image(name: &str, offset: u64, size: Option<u64>) -> io::Result<Vec<u8>> {
    let file = fs::File::from(shm_open(name, OFlag::O_RDONLY, Mode::empty())?);
    // Kitty unlinks a successfully opened client object immediately so the sender
    // can observe reception and cannot leak names into /dev/shm.
    let _ = shm_unlink(name);
    let total = file.metadata()?.len();
    if offset > total {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared memory offset exceeds the object size",
        ));
    }
    let length = bounded_read_length(total - offset, size)?;
    let mut bytes = vec![0; length as usize];
    let mut file = file;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(&mut bytes)?;
    if file.metadata()?.len() != total {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared memory changed size during transfer",
        ));
    }
    Ok(bytes)
}

fn inflate_zlib(bytes: &[u8]) -> io::Result<Vec<u8>> {
    let mut decoder = ZlibDecoder::new(bytes);
    let mut output = Vec::new();
    let mut buffer = [0u8; 8192];
    loop {
        let read = decoder.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        if output.len().saturating_add(read) > MAX_DECOMPRESSED_PAYLOAD {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "decompressed image exceeds the 64 MiB limit",
            ));
        }
        output.extend_from_slice(&buffer[..read]);
    }
    Ok(output)
}

#[doc(hidden)]
pub fn read_shm_payload(name: &str) -> io::Result<Vec<u8>> {
    let file = fs::File::from(shm_open(name, OFlag::O_RDONLY, Mode::empty())?);
    let size = file.metadata()?.len();
    read_shm_bytes(file, size)
}

#[doc(hidden)]
pub fn read_shm_bytes(mut file: fs::File, size: u64) -> io::Result<Vec<u8>> {
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

pub(crate) fn decode_png(id: u32, bytes: &[u8]) -> io::Result<ImageData> {
    let mut decoder = png::Decoder::new(Cursor::new(bytes));
    decoder.set_transformations(
        png::Transformations::EXPAND | png::Transformations::STRIP_16 | png::Transformations::ALPHA,
    );
    let mut reader = decoder
        .read_info()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let output_size = reader
        .output_buffer_size()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid PNG output size"))?;
    if output_size > MAX_DECOMPRESSED_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "decoded PNG exceeds the 64 MiB limit",
        ));
    }
    let mut buf = vec![0; output_size];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let width = info.width;
    let height = info.height;
    if (width as usize)
        .checked_mul(height as usize)
        .and_then(|pixels| pixels.checked_mul(4))
        .is_none_or(|size| size > MAX_DECOMPRESSED_PAYLOAD)
    {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "PNG dimensions exceed the image budget",
        ));
    }

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
        rgba: Some(rgba),
    })
}

// --- Parser & State Machine ---

const MAX_APC_PAYLOAD: usize = 32 * 1024 * 1024;

/// Upper bound on the clean-output scratch buffer retained for reuse. PTY reads are 64 KiB, so a
/// normal buffer is always recycled; anything larger is dropped instead of pinned indefinitely.
#[doc(hidden)]
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
    #[doc(hidden)]
    pub in_apc: bool,
    apc_buffer: Vec<u8>,
    pub(crate) pending_stream: Vec<u8>,
    chunked_command: Option<KittyCommand>,
    chunked_payload: Vec<u8>,
    next_image_id: u32,
    /// Latest image id allocated for each client-supplied `I=` image number.
    image_numbers: HashMap<u32, u32>,
    #[doc(hidden)]
    pub clean_scratch: Vec<u8>,
    #[doc(hidden)]
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
    pub fn recycle_clean_buffer(&mut self, mut buffer: Vec<u8>) {
        buffer.clear();
        self.clean_scratch = if buffer.capacity() <= MAX_RECYCLED_CLEAN_BYTES {
            buffer
        } else {
            Vec::new()
        };
    }

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

pub mod command {
    pub use super::parse_control_keys;
}

pub mod model {
    pub use super::{
        DeleteTarget, ImageData, ImagePlacement, KittyAction, KittyCommand, KittyCompression,
        KittyEvent, KittyFormat, KittyMedium,
    };
}

pub mod payload {
    pub use super::{
        MAX_DECOMPRESSED_PAYLOAD, MAX_SHM_PAYLOAD, decode_image_data, read_shm_bytes,
        read_shm_payload,
    };
}
