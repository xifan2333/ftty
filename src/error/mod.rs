//! Domain-specific error types for terminal emulation, Wayland protocols, and rendering.

use std::fmt;
use std::io;
use std::path::PathBuf;

#[cfg(test)]
mod tests;

/// Top-level domain error enum encapsulating all ftty failure modes.
#[derive(Debug)]
pub enum FttyError {
    /// PTY allocation, process spawning, or terminal sizing failure.
    Pty(PtyError),
    /// Wayland connection, protocol dispatch, or state machine violation.
    Wayland(WaylandError),
    /// EGL context setup, shader compilation, or OpenGL rendering failure.
    Render(RenderError),
    /// Font discovery, parsing, or rasterization failure.
    Font(FontError),
    /// Configuration loading, parsing, or include hierarchy failure.
    Config(ConfigError),
    /// Kitty graphics protocol parsing, decoding, or shared memory failure.
    Kitty(KittyError),
    /// General I/O or system call failure.
    Io(io::Error),
}

impl fmt::Display for FttyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Pty(err) => write!(f, "PTY error: {err}"),
            Self::Wayland(err) => write!(f, "Wayland error: {err}"),
            Self::Render(err) => write!(f, "Render error: {err}"),
            Self::Font(err) => write!(f, "Font error: {err}"),
            Self::Config(err) => write!(f, "Config error: {err}"),
            Self::Kitty(err) => write!(f, "Kitty protocol error: {err}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
        }
    }
}

impl std::error::Error for FttyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Pty(err) => Some(err),
            Self::Wayland(err) => Some(err),
            Self::Render(err) => Some(err),
            Self::Font(err) => Some(err),
            Self::Config(err) => Some(err),
            Self::Kitty(err) => Some(err),
            Self::Io(err) => Some(err),
        }
    }
}

impl From<PtyError> for FttyError {
    fn from(err: PtyError) -> Self {
        Self::Pty(err)
    }
}

impl From<WaylandError> for FttyError {
    fn from(err: WaylandError) -> Self {
        Self::Wayland(err)
    }
}

impl From<RenderError> for FttyError {
    fn from(err: RenderError) -> Self {
        Self::Render(err)
    }
}

impl From<FontError> for FttyError {
    fn from(err: FontError) -> Self {
        Self::Font(err)
    }
}

impl From<ConfigError> for FttyError {
    fn from(err: ConfigError) -> Self {
        Self::Config(err)
    }
}

impl From<KittyError> for FttyError {
    fn from(err: KittyError) -> Self {
        Self::Kitty(err)
    }
}

impl From<io::Error> for FttyError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<FttyError> for io::Error {
    fn from(err: FttyError) -> Self {
        match err {
            FttyError::Pty(e) => e.into(),
            FttyError::Wayland(e) => e.into(),
            FttyError::Render(e) => e.into(),
            FttyError::Font(e) => e.into(),
            FttyError::Config(e) => e.into(),
            FttyError::Kitty(e) => e.into(),
            FttyError::Io(e) => e,
        }
    }
}

/// Errors originating from POSIX PTY allocation and sub-process lifecycle.
#[derive(Debug)]
pub enum PtyError {
    /// `openpty` system call failed.
    Openpty(i32),
    /// `fork` system call failed.
    Fork(i32),
    /// Setting window dimensions via `TIOCSWINSZ` failed.
    SetWindowSize(i32),
    /// Duplicating master file descriptor failed.
    CloneMaster(i32),
    /// Subprocess spawning or command execution failed.
    Spawn(String),
    /// Underlying I/O error.
    Io(io::Error),
}

impl fmt::Display for PtyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Openpty(code) => write!(f, "openpty failed with errno {code}"),
            Self::Fork(code) => write!(f, "fork failed with errno {code}"),
            Self::SetWindowSize(code) => write!(f, "TIOCSWINSZ ioctl failed with errno {code}"),
            Self::CloneMaster(code) => write!(f, "dup master fd failed with errno {code}"),
            Self::Spawn(msg) => write!(f, "failed to spawn process: {msg}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for PtyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<PtyError> for io::Error {
    fn from(err: PtyError) -> Self {
        match err {
            PtyError::Openpty(code)
            | PtyError::Fork(code)
            | PtyError::SetWindowSize(code)
            | PtyError::CloneMaster(code) => io::Error::from_raw_os_error(code),
            PtyError::Spawn(msg) => io::Error::other(msg),
            PtyError::Io(e) => e,
        }
    }
}

/// Errors originating from Wayland client connection and protocol state machines.
#[derive(Debug)]
pub enum WaylandError {
    /// Compositor connection failed.
    Connection(String),
    /// Required Wayland global interface not advertised by compositor.
    MissingGlobal(&'static str),
    /// Attempted action before window surface was created.
    WindowNotCreated,
    /// Attempted rendering or commit before initial configure event.
    UnconfiguredSurface,
    /// Invalid window lifecycle transition.
    InvalidStateTransition {
        from: &'static str,
        to: &'static str,
    },
    /// Wayland queue dispatch error.
    Dispatch(String),
    /// Underlying I/O error.
    Io(io::Error),
}

impl fmt::Display for WaylandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Connection(msg) => write!(f, "failed to connect to Wayland compositor: {msg}"),
            Self::MissingGlobal(name) => write!(f, "required Wayland global missing: {name}"),
            Self::WindowNotCreated => write!(f, "Wayland window surface not created"),
            Self::UnconfiguredSurface => {
                write!(f, "Wayland surface not configured by compositor yet")
            }
            Self::InvalidStateTransition { from, to } => {
                write!(f, "invalid window state transition from {from} to {to}")
            }
            Self::Dispatch(msg) => write!(f, "Wayland dispatch error: {msg}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for WaylandError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<WaylandError> for io::Error {
    fn from(err: WaylandError) -> Self {
        match err {
            WaylandError::Connection(msg) => io::Error::new(io::ErrorKind::ConnectionRefused, msg),
            WaylandError::MissingGlobal(name) => {
                io::Error::new(io::ErrorKind::NotFound, format!("missing global: {name}"))
            }
            WaylandError::WindowNotCreated | WaylandError::UnconfiguredSurface => {
                io::Error::new(io::ErrorKind::NotConnected, err.to_string())
            }
            WaylandError::InvalidStateTransition { .. } => {
                io::Error::new(io::ErrorKind::InvalidInput, err.to_string())
            }
            WaylandError::Dispatch(msg) => io::Error::other(msg),
            WaylandError::Io(e) => e,
        }
    }
}

/// Errors originating from EGL, OpenGL, or procedural rendering pipelines.
#[derive(Debug)]
pub enum RenderError {
    /// `wl_egl_window` creation failed.
    EglSurface(String),
    /// EGL display retrieval failed.
    EglDisplay(String),
    /// EGL initialization failed.
    EglInit(String),
    /// No matching EGL configuration supports required attributes.
    NoSupportedConfig,
    /// EGL context creation failed.
    ContextCreation(String),
    /// EGL window surface creation failed.
    SurfaceCreation(String),
    /// `eglMakeCurrent` failed.
    MakeCurrent(String),
    /// Vertex buffer object creation failed.
    BufferCreation(String),
    /// Texture object creation failed.
    TextureCreation(String),
    /// Shader object creation failed.
    ShaderCreation(String),
    /// Shader compilation failed.
    ShaderCompile { kind: &'static str, log: String },
    /// Program creation failed.
    ProgramCreation(String),
    /// Shader program linking failed.
    ProgramLink { log: String },
    /// Invalid window dimensions (e.g. non-positive or exceeding maximum integer range).
    InvalidDimensions(u32, u32),
    /// EGL window surface is not initialized.
    SurfaceNotInitialized,
    /// General OpenGL driver error.
    Gl(String),
}

impl fmt::Display for RenderError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EglSurface(msg) => write!(f, "failed to create wl_egl_window: {msg}"),
            Self::EglDisplay(msg) => write!(f, "eglGetDisplay failed: {msg}"),
            Self::EglInit(msg) => write!(f, "eglInitialize failed: {msg}"),
            Self::NoSupportedConfig => write!(f, "no EGL configuration supports OpenGL ES 2"),
            Self::ContextCreation(msg) => write!(f, "failed to create EGL context: {msg}"),
            Self::SurfaceCreation(msg) => write!(f, "failed to create EGL surface: {msg}"),
            Self::MakeCurrent(msg) => write!(f, "eglMakeCurrent failed: {msg}"),
            Self::BufferCreation(msg) => write!(f, "failed to create GL buffer: {msg}"),
            Self::TextureCreation(msg) => write!(f, "failed to create GL texture: {msg}"),
            Self::ShaderCreation(msg) => write!(f, "failed to create GL shader: {msg}"),
            Self::ShaderCompile { kind, log } => {
                write!(f, "{kind} shader compilation failed:\n{log}")
            }
            Self::ProgramCreation(msg) => write!(f, "failed to create GL program: {msg}"),
            Self::ProgramLink { log } => write!(f, "shader program link failed:\n{log}"),
            Self::InvalidDimensions(w, h) => write!(f, "invalid render dimensions: {w}x{h}"),
            Self::SurfaceNotInitialized => write!(f, "EGL window surface is not initialized"),
            Self::Gl(msg) => write!(f, "OpenGL error: {msg}"),
        }
    }
}

impl std::error::Error for RenderError {}

impl From<RenderError> for io::Error {
    fn from(err: RenderError) -> Self {
        match err {
            RenderError::InvalidDimensions(w, h) => io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("invalid dimensions: {w}x{h}"),
            ),
            _ => io::Error::other(err.to_string()),
        }
    }
}

/// Errors originating from font loading, fontconfig querying, or rasterization.
#[derive(Debug)]
pub enum FontError {
    /// Non-positive or unsupported font size.
    InvalidSize(f32),
    /// Fontconfig library is not available on host system.
    FontconfigUnavailable,
    /// No usable monospace font found on host system.
    MonospaceNotFound,
    /// Font file parsing failed.
    FontParse(String),
    /// Font file not found at expected path.
    NotFound(String),
    /// Underlying I/O error.
    Io(io::Error),
}

impl fmt::Display for FontError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidSize(size) => write!(f, "invalid font size: {size}"),
            Self::FontconfigUnavailable => write!(f, "fontconfig not available"),
            Self::MonospaceNotFound => write!(f, "no monospace font found"),
            Self::FontParse(msg) => write!(f, "failed to parse font: {msg}"),
            Self::NotFound(path) => write!(f, "font not found: {path}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for FontError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<FontError> for io::Error {
    fn from(err: FontError) -> Self {
        match err {
            FontError::InvalidSize(size) => io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("font size must be positive, got {size}"),
            ),
            FontError::FontconfigUnavailable => {
                io::Error::new(io::ErrorKind::NotFound, "fontconfig not available")
            }
            FontError::MonospaceNotFound => {
                io::Error::new(io::ErrorKind::NotFound, "no monospace font found")
            }
            FontError::FontParse(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            FontError::NotFound(path) => {
                io::Error::new(io::ErrorKind::NotFound, format!("font not found: {path}"))
            }
            FontError::Io(e) => e,
        }
    }
}

/// Errors originating from configuration loading and inclusion recursion.
#[derive(Debug)]
pub enum ConfigError {
    /// Circular include detected in configuration hierarchy.
    CircularInclude(PathBuf),
    /// TOML parsing failed.
    Parse(String),
    /// Underlying I/O error when reading configuration file.
    Io(io::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::CircularInclude(path) => {
                write!(f, "circular include detected: {}", path.display())
            }
            Self::Parse(msg) => write!(f, "failed to parse configuration: {msg}"),
            Self::Io(err) => write!(f, "{err}"),
        }
    }
}

impl std::error::Error for ConfigError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(err) => Some(err),
            _ => None,
        }
    }
}

impl From<ConfigError> for io::Error {
    fn from(err: ConfigError) -> Self {
        match err {
            ConfigError::CircularInclude(path) => io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("circular include detected: {}", path.display()),
            ),
            ConfigError::Parse(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            ConfigError::Io(e) => e,
        }
    }
}

/// Errors originating from Kitty graphics protocol processing.
#[derive(Debug)]
pub enum KittyError {
    /// Malformed or invalid Kitty payload command parameters.
    InvalidPayload(String),
    /// Unsupported image compression or color format.
    UnsupportedFormat(u32),
    /// PNG decompression failed.
    PngDecode(String),
    /// POSIX shared memory payload read error.
    Shm(io::Error),
}

impl fmt::Display for KittyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidPayload(msg) => write!(f, "invalid kitty payload: {msg}"),
            Self::UnsupportedFormat(fmt) => write!(f, "unsupported kitty format: {fmt}"),
            Self::PngDecode(msg) => write!(f, "failed to decode PNG: {msg}"),
            Self::Shm(err) => write!(f, "kitty shm error: {err}"),
        }
    }
}

impl std::error::Error for KittyError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Shm(err) => Some(err),
            _ => None,
        }
    }
}

impl From<KittyError> for io::Error {
    fn from(err: KittyError) -> Self {
        match err {
            KittyError::InvalidPayload(msg) => io::Error::new(io::ErrorKind::InvalidInput, msg),
            KittyError::UnsupportedFormat(fmt) => io::Error::new(
                io::ErrorKind::Unsupported,
                format!("unsupported kitty format: {fmt}"),
            ),
            KittyError::PngDecode(msg) => io::Error::new(io::ErrorKind::InvalidData, msg),
            KittyError::Shm(e) => e,
        }
    }
}
