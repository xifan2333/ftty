//! Image payload decoding for Base64, raw RGB/RGBA, PNG, and POSIX shared memory.

use std::fs;
use std::io::{self, Cursor, Read};

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use nix::fcntl::OFlag;
use nix::sys::mman::shm_open;
use nix::sys::stat::Mode;

use crate::kitty::model::{ImageData, KittyCommand, KittyFormat, KittyMedium};

pub(crate) const MAX_SHM_PAYLOAD: u64 = 32 * 1024 * 1024;

pub(crate) fn decode_image_data(
    id: u32,
    cmd: &KittyCommand,
    raw_payload: &[u8],
) -> io::Result<ImageData> {
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

pub(crate) fn read_shm_payload(name: &str) -> io::Result<Vec<u8>> {
    let file = fs::File::from(shm_open(name, OFlag::O_RDONLY, Mode::empty())?);
    let size = file.metadata()?.len();
    read_shm_bytes(file, size)
}

pub(crate) fn read_shm_bytes(mut file: fs::File, size: u64) -> io::Result<Vec<u8>> {
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
