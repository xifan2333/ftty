//! Image payload decoding for Base64, raw RGB/RGBA, PNG, and POSIX shared memory.

use std::ffi::c_void;
use std::fs;
use std::io::{self, Cursor};
use std::num::NonZeroUsize;
use std::os::fd::AsFd;
use std::ptr::NonNull;
use std::sync::Arc;

use base64::Engine;
use base64::prelude::BASE64_STANDARD;
use nix::fcntl::OFlag;
use nix::sys::mman::{MapFlags, ProtFlags, mmap, munmap, shm_open};
use nix::sys::stat::Mode;

use crate::kitty::model::{ImageData, ImagePixels, KittyCommand, KittyFormat, KittyMedium};

pub(crate) const MAX_SHM_PAYLOAD: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
struct MmapInner {
    ptr: NonNull<c_void>,
    len: usize,
}

// SAFETY: MmapInner encapsulates an immutable read-only shared memory mapping that can safely be moved across threads.
unsafe impl Send for MmapInner {}
// SAFETY: MmapInner references immutable read-only pages and performs no interior mutation across threads.
unsafe impl Sync for MmapInner {}

impl Drop for MmapInner {
    fn drop(&mut self) {
        if self.len > 0 {
            // SAFETY: self.ptr is a valid NonNull pointer returned by a successful mmap call
            // and self.len is the exact length passed to mmap.
            unsafe {
                let _ = munmap(self.ptr, self.len);
            }
        }
    }
}

/// Thread-safe reference-counted handle to a POSIX shared memory mapped region.
#[derive(Debug, Clone)]
pub struct MmapPayload {
    inner: Arc<MmapInner>,
}

impl MmapPayload {
    /// Maps a shared memory file descriptor with read-only permissions.
    ///
    /// # Errors
    /// Returns an [`io::Error`] if the OS fails to map the descriptor.
    pub fn map_file<F: AsFd>(fd: &F, size: usize) -> io::Result<Self> {
        if size == 0 {
            return Ok(Self {
                inner: Arc::new(MmapInner {
                    ptr: NonNull::dangling(),
                    len: 0,
                }),
            });
        }
        let non_zero_len = match NonZeroUsize::new(size) {
            Some(n) => n,
            None => {
                return Ok(Self {
                    inner: Arc::new(MmapInner {
                        ptr: NonNull::dangling(),
                        len: 0,
                    }),
                });
            }
        };
        // SAFETY: fd is an open descriptor, non_zero_len matches its verified size,
        // MAP_SHARED with PROT_READ ensures read-only access without mutating file state.
        let ptr = unsafe {
            mmap(
                None,
                non_zero_len,
                ProtFlags::PROT_READ,
                MapFlags::MAP_SHARED,
                fd,
                0,
            )
        }
        .map_err(|e| io::Error::from_raw_os_error(e as i32))?;

        Ok(Self {
            inner: Arc::new(MmapInner { ptr, len: size }),
        })
    }

    /// Returns a slice over the mapped memory region.
    #[must_use]
    pub fn as_slice(&self) -> &[u8] {
        if self.inner.len == 0 {
            &[]
        } else {
            // SAFETY: ptr points to a valid mapped range of length self.inner.len with PROT_READ permissions.
            unsafe {
                std::slice::from_raw_parts(self.inner.ptr.as_ptr().cast::<u8>(), self.inner.len)
            }
        }
    }
}

impl std::ops::Deref for MmapPayload {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        self.as_slice()
    }
}

impl PartialEq for MmapPayload {
    fn eq(&self, other: &Self) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl PartialEq<[u8]> for MmapPayload {
    fn eq(&self, other: &[u8]) -> bool {
        self.as_slice() == other
    }
}

impl PartialEq<Vec<u8>> for MmapPayload {
    fn eq(&self, other: &Vec<u8>) -> bool {
        self.as_slice() == other.as_slice()
    }
}

impl Eq for MmapPayload {}

pub(crate) enum PayloadBuffer {
    Owned(Vec<u8>),
    Mmap(MmapPayload),
}

impl std::ops::Deref for PayloadBuffer {
    type Target = [u8];
    fn deref(&self) -> &Self::Target {
        match self {
            Self::Owned(bytes) => bytes.as_slice(),
            Self::Mmap(mapped) => mapped.as_slice(),
        }
    }
}

pub(crate) fn decode_image_data(
    id: u32,
    cmd: &KittyCommand,
    raw_payload: &[u8],
) -> io::Result<ImageData> {
    let payload = match cmd.medium {
        KittyMedium::Direct => {
            // Direct payload: Base64 encoded data
            PayloadBuffer::Owned(
                BASE64_STANDARD
                    .decode(raw_payload)
                    .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?,
            )
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
            PayloadBuffer::Owned(data)
        }
        KittyMedium::SharedMemory => {
            let name_bytes = BASE64_STANDARD
                .decode(raw_payload)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            let name_str = std::str::from_utf8(&name_bytes)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            PayloadBuffer::Mmap(read_shm_payload(name_str)?)
        }
    };

    match cmd.format {
        KittyFormat::Png => decode_png(id, &payload),
        KittyFormat::Rgba32 => {
            let width = cmd.width.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing width for raw RGBA")
            })?;
            let height = cmd.height.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing height for raw RGBA")
            })?;
            let expected_len = (width as usize) * (height as usize) * 4;
            if payload.len() != expected_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "raw RGBA byte size mismatch: expected {expected_len}, got {}",
                        payload.len()
                    ),
                ));
            }
            let pixels = match payload {
                PayloadBuffer::Owned(bytes) => ImagePixels::Owned(bytes),
                PayloadBuffer::Mmap(mapped) => ImagePixels::Mmap(mapped),
            };
            Ok(ImageData::with_pixels(id, width, height, pixels))
        }
        KittyFormat::Rgb24 => {
            let width = cmd.width.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing width for raw RGB")
            })?;
            let height = cmd.height.ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidInput, "missing height for raw RGB")
            })?;
            let expected_len = (width as usize) * (height as usize) * 3;
            if payload.len() != expected_len {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "raw RGB byte size mismatch: expected {expected_len}, got {}",
                        payload.len()
                    ),
                ));
            }
            let mut rgba = Vec::with_capacity((width as usize) * (height as usize) * 4);
            for chunk in payload.as_chunks::<3>().0 {
                rgba.extend_from_slice(&[chunk[0], chunk[1], chunk[2], 255]);
            }
            Ok(ImageData::with_pixels(
                id,
                width,
                height,
                ImagePixels::Owned(rgba),
            ))
        }
    }
}

pub(crate) fn read_shm_payload(name: &str) -> io::Result<MmapPayload> {
    let file = fs::File::from(shm_open(name, OFlag::O_RDONLY, Mode::empty())?);
    let size = file.metadata()?.len();
    read_shm_bytes(file, size)
}

pub(crate) fn read_shm_bytes(file: fs::File, size: u64) -> io::Result<MmapPayload> {
    if size > MAX_SHM_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared memory exceeds the 32 MiB image payload limit",
        ));
    }

    let current_len = file.metadata()?.len();
    if current_len < size {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "shared memory truncated during transfer",
        ));
    } else if current_len > size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "shared memory changed size during transfer",
        ));
    }

    MmapPayload::map_file(&file, size as usize)
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

    Ok(ImageData::with_pixels(
        id,
        width,
        height,
        ImagePixels::Owned(rgba),
    ))
}
