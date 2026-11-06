//! Payload acquisition (direct, file, or shared memory) and PNG/RGBA decoding.

use std::fs;
use std::io::{self, Cursor, Read, Seek, SeekFrom};

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use flate2::read::ZlibDecoder;
use nix::fcntl::OFlag;
use nix::sys::mman::{shm_open, shm_unlink};
use nix::sys::stat::Mode;

use super::model::{ImageData, KittyCommand, KittyCompression, KittyFormat, KittyMedium};

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

pub(crate) fn validate_capability_query(cmd: &KittyCommand, raw_payload: &[u8]) -> io::Result<()> {
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
#[cfg(test)]
pub fn read_shm_payload(name: &str) -> io::Result<Vec<u8>> {
    let file = fs::File::from(shm_open(name, OFlag::O_RDONLY, Mode::empty())?);
    let size = file.metadata()?.len();
    read_shm_bytes(file, size)
}

#[doc(hidden)]
#[cfg(test)]
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
