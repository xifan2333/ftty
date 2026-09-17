use super::*;
use std::error::Error;
use std::io::ErrorKind;

#[test]
fn test_ftty_error_display_and_source() {
    let pty_err = PtyError::Openpty(2);
    let ftty_pty: FttyError = pty_err.into();
    assert_eq!(
        ftty_pty.to_string(),
        "PTY error: openpty failed with errno 2"
    );
    assert!(ftty_pty.source().is_some());

    let wayland_err = WaylandError::MissingGlobal("wl_compositor");
    let ftty_wayland: FttyError = wayland_err.into();
    assert_eq!(
        ftty_wayland.to_string(),
        "Wayland error: required Wayland global missing: wl_compositor"
    );

    let render_err = RenderError::NoSupportedConfig;
    let ftty_render: FttyError = render_err.into();
    assert_eq!(
        ftty_render.to_string(),
        "Render error: no EGL configuration supports OpenGL ES 2"
    );

    let font_err = FontError::InvalidSize(-5.0);
    let ftty_font: FttyError = font_err.into();
    assert_eq!(ftty_font.to_string(), "Font error: invalid font size: -5");

    let config_err = ConfigError::Parse("invalid toml syntax".into());
    let ftty_config: FttyError = config_err.into();
    assert_eq!(
        ftty_config.to_string(),
        "Config error: failed to parse configuration: invalid toml syntax"
    );

    let kitty_err = KittyError::UnsupportedFormat(99);
    let ftty_kitty: FttyError = kitty_err.into();
    assert_eq!(
        ftty_kitty.to_string(),
        "Kitty protocol error: unsupported kitty format: 99"
    );

    let io_err = io::Error::new(ErrorKind::NotFound, "file not found");
    let ftty_io: FttyError = io_err.into();
    assert_eq!(ftty_io.to_string(), "I/O error: file not found");
    assert!(ftty_io.source().is_some());
}

#[test]
fn test_io_error_conversions() {
    let pty_err = PtyError::SetWindowSize(22);
    let io_err: io::Error = pty_err.into();
    assert_eq!(io_err.raw_os_error(), Some(22));

    let font_err = FontError::InvalidSize(0.0);
    let io_err: io::Error = font_err.into();
    assert_eq!(io_err.kind(), ErrorKind::InvalidInput);

    let config_err = ConfigError::CircularInclude(PathBuf::from("/tmp/config.toml"));
    let io_err: io::Error = config_err.into();
    assert_eq!(io_err.kind(), ErrorKind::InvalidInput);
    assert!(io_err.to_string().contains("circular include detected"));

    let kitty_err = KittyError::UnsupportedFormat(42);
    let io_err: io::Error = kitty_err.into();
    assert_eq!(io_err.kind(), ErrorKind::Unsupported);

    let render_err = RenderError::InvalidDimensions(0, 0);
    let io_err: io::Error = render_err.into();
    assert_eq!(io_err.kind(), ErrorKind::InvalidInput);

    let wayland_err = WaylandError::Connection("socket unavailable".into());
    let io_err: io::Error = wayland_err.into();
    assert_eq!(io_err.kind(), ErrorKind::ConnectionRefused);
}

#[test]
fn test_ftty_error_to_io_error_conversion() {
    let ftty_err = FttyError::Font(FontError::FontconfigUnavailable);
    let io_err: io::Error = ftty_err.into();
    assert_eq!(io_err.kind(), ErrorKind::NotFound);
}
