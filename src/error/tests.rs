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
fn test_pty_error_variants() {
    assert_eq!(PtyError::Fork(11).to_string(), "fork failed with errno 11");
    assert_eq!(
        PtyError::CloneMaster(9).to_string(),
        "dup master fd failed with errno 9"
    );
    assert_eq!(
        PtyError::Spawn("sh not found".into()).to_string(),
        "failed to spawn process: sh not found"
    );
    let io_sub = PtyError::Io(io::Error::from_raw_os_error(5));
    assert!(io_sub.source().is_some());

    let io_converted: io::Error = PtyError::Spawn("exec failure".into()).into();
    assert_eq!(io_converted.to_string(), "exec failure");
}

#[test]
fn test_wayland_error_variants() {
    assert_eq!(
        WaylandError::WindowNotCreated.to_string(),
        "Wayland window surface not created"
    );
    assert_eq!(
        WaylandError::UnconfiguredSurface.to_string(),
        "Wayland surface not configured by compositor yet"
    );
    assert_eq!(
        WaylandError::InvalidStateTransition {
            from: "Unmapped",
            to: "Active",
        }
        .to_string(),
        "invalid window state transition from Unmapped to Active"
    );
    assert_eq!(
        WaylandError::Dispatch("queue error".into()).to_string(),
        "Wayland dispatch error: queue error"
    );

    let io_from_dispatch: io::Error = WaylandError::Dispatch("broken pipe".into()).into();
    assert_eq!(io_from_dispatch.to_string(), "broken pipe");

    let io_from_transition: io::Error =
        WaylandError::InvalidStateTransition { from: "A", to: "B" }.into();
    assert_eq!(io_from_transition.kind(), ErrorKind::InvalidInput);
}

#[test]
fn test_render_error_variants() {
    assert_eq!(
        RenderError::EglSurface("bad window".into()).to_string(),
        "failed to create wl_egl_window: bad window"
    );
    assert_eq!(
        RenderError::EglDisplay("bad display".into()).to_string(),
        "eglGetDisplay failed: bad display"
    );
    assert_eq!(
        RenderError::EglInit("init fail".into()).to_string(),
        "eglInitialize failed: init fail"
    );
    assert_eq!(
        RenderError::ContextCreation("ctx fail".into()).to_string(),
        "failed to create EGL context: ctx fail"
    );
    assert_eq!(
        RenderError::SurfaceCreation("surf fail".into()).to_string(),
        "failed to create EGL surface: surf fail"
    );
    assert_eq!(
        RenderError::MakeCurrent("make curr fail".into()).to_string(),
        "eglMakeCurrent failed: make curr fail"
    );
    assert_eq!(
        RenderError::BufferCreation("vbo fail".into()).to_string(),
        "failed to create GL buffer: vbo fail"
    );
    assert_eq!(
        RenderError::TextureCreation("tex fail".into()).to_string(),
        "failed to create GL texture: tex fail"
    );
    assert_eq!(
        RenderError::ShaderCreation("shader fail".into()).to_string(),
        "failed to create GL shader: shader fail"
    );
    assert_eq!(
        RenderError::ShaderCompile {
            kind: "vertex",
            log: "syntax error".into()
        }
        .to_string(),
        "vertex shader compilation failed:\nsyntax error"
    );
    assert_eq!(
        RenderError::ProgramCreation("prog fail".into()).to_string(),
        "failed to create GL program: prog fail"
    );
    assert_eq!(
        RenderError::ProgramLink {
            log: "link error".into()
        }
        .to_string(),
        "shader program link failed:\nlink error"
    );
    assert_eq!(
        RenderError::SurfaceNotInitialized.to_string(),
        "EGL window surface is not initialized"
    );
    assert_eq!(
        RenderError::Gl("driver reset".into()).to_string(),
        "OpenGL error: driver reset"
    );
}

#[test]
fn test_font_error_variants() {
    assert_eq!(
        FontError::MonospaceNotFound.to_string(),
        "no monospace font found"
    );
    assert_eq!(
        FontError::FontParse("corrupted ttf".into()).to_string(),
        "failed to parse font: corrupted ttf"
    );
    assert_eq!(
        FontError::NotFound("/usr/share/fonts/missing.ttf".into()).to_string(),
        "font not found: /usr/share/fonts/missing.ttf"
    );

    let io_parse: io::Error = FontError::FontParse("bad font".into()).into();
    assert_eq!(io_parse.kind(), ErrorKind::InvalidData);

    let io_not_found: io::Error = FontError::NotFound("/path".into()).into();
    assert_eq!(io_not_found.kind(), ErrorKind::NotFound);
}

#[test]
fn test_kitty_error_variants() {
    assert_eq!(
        KittyError::InvalidPayload("bad dim".into()).to_string(),
        "invalid kitty payload: bad dim"
    );
    assert_eq!(
        KittyError::PngDecode("crc fail".into()).to_string(),
        "failed to decode PNG: crc fail"
    );
    let shm_err = KittyError::Shm(io::Error::new(ErrorKind::PermissionDenied, "access denied"));
    assert_eq!(shm_err.to_string(), "kitty shm error: access denied");
    assert!(shm_err.source().is_some());

    let io_payload: io::Error = KittyError::InvalidPayload("len".into()).into();
    assert_eq!(io_payload.kind(), ErrorKind::InvalidInput);

    let io_png: io::Error = KittyError::PngDecode("err".into()).into();
    assert_eq!(io_png.kind(), ErrorKind::InvalidData);
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

    let ftty_pty: FttyError = PtyError::Openpty(13).into();
    let io_pty: io::Error = ftty_pty.into();
    assert_eq!(io_pty.raw_os_error(), Some(13));

    let ftty_cfg: FttyError = ConfigError::Parse("bad toml".into()).into();
    let io_cfg: io::Error = ftty_cfg.into();
    assert_eq!(io_cfg.kind(), ErrorKind::InvalidData);

    let ftty_kit: FttyError = KittyError::InvalidPayload("wrong format".into()).into();
    let io_kit: io::Error = ftty_kit.into();
    assert_eq!(io_kit.kind(), ErrorKind::InvalidInput);
}
