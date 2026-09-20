use std::fs;

use xkbcommon::xkb;

use crate::color::Rgb;
use crate::config::Config;
use crate::grid::CursorShape;

#[test]
fn test_default_config_values() {
    let config = Config::default();
    assert_eq!(config.font_family(), "monospace");
    assert_eq!(config.font_size(), 14.0);
    assert_eq!(config.padding(), [0, 0]);
    assert_eq!(config.cursor_shape(), CursorShape::Block);
    assert_eq!(config.foreground(), Rgb::new(220, 220, 220));
    assert_eq!(config.background(), Rgb::new(24, 24, 24));
}

#[test]
fn test_parse_basic_toml() {
    let toml_str = r##"
    [font]
    family = ["JetBrains Mono", "Noto Sans CJK SC", "Noto Color Emoji"]
    size = 16.5

    [window]
    padding = [4, 6]

    [cursor]
    shape = "beam"

    [colors]
    foreground = "#ffffff"
    background = "#000000"

    [colors.palette]
    red = "#ff0000"
    bright_red = "#ff5555"
    "##;

    let config: Config = toml::from_str(toml_str).expect("parse toml");
    assert_eq!(config.font_family(), "JetBrains Mono");
    assert_eq!(
        config.font_families(),
        vec![
            "JetBrains Mono".to_string(),
            "Noto Sans CJK SC".to_string(),
            "Noto Color Emoji".to_string(),
        ]
    );
    assert_eq!(config.font_size(), 16.5);
    assert_eq!(config.padding(), [4, 6]);
    assert_eq!(config.padding_x(), 4);
    assert_eq!(config.padding_y(), 6);
    assert_eq!(config.cursor_shape(), CursorShape::Beam);
    assert_eq!(config.foreground(), Rgb::new(255, 255, 255));
    assert_eq!(config.background(), Rgb::new(0, 0, 0));

    let palette = config.build_palette();
    assert_eq!(palette[1], Rgb::new(255, 0, 0));
    assert_eq!(palette[9], Rgb::new(255, 85, 85));
}

#[test]
fn test_parse_padding_scalar() {
    let toml_str = r#"
    [window]
    padding = 8
    "#;
    let config: Config = toml::from_str(toml_str).expect("parse padding scalar");
    assert_eq!(config.padding(), [8, 8]);
    assert_eq!(config.padding_x(), 8);
    assert_eq!(config.padding_y(), 8);
}

#[test]
fn test_parse_font_chain() {
    let toml_str = r#"
    [font]
    families = ["Fira Code", "Symbols Nerd Font", "Noto Sans CJK SC"]
    size = 15.0
    "#;
    let config: Config = toml::from_str(toml_str).expect("parse toml with font chain");
    assert_eq!(config.font_family(), "Fira Code");
    assert_eq!(
        config.font_families(),
        vec!["Fira Code", "Symbols Nerd Font", "Noto Sans CJK SC"]
    );
    assert_eq!(config.font_size(), 15.0);
}

#[test]
fn test_parse_scrollback_and_keybindings() {
    let toml_str = r##"
    [scrollback]
    lines = 5000
    multiplier = 5.5
    auto_scroll = false

    [keybindings]
    "Shift+PageUp" = "scrollback_up_page"
    "Shift+KP_PageUp" = "scrollback_up_page"
    "Shift+PageDown" = "scrollback_down_page"
    "Ctrl+Shift+C" = "clipboard_copy"
    "Ctrl+Shift+V" = "none"
    "Ctrl+Shift+U" = { pipe_visible = ["urlscan"] }
    "Ctrl+Shift+F" = { pipe_scrollback = ["sh", "-c", "fzf | wl-copy"] }
    "Ctrl+Shift+Y" = { pipe_selection = "wl-copy" }
    "##;

    let config: Config = toml::from_str(toml_str).expect("parse toml");
    assert_eq!(config.scrollback_lines(), 5000);
    assert_eq!(config.scroll_multiplier(), 5.5);
    assert!(!config.auto_scroll());

    let resolved = config.keybindings.resolve_bindings();
    let pipe_u = resolved.iter().find(|((_, sym), _)| {
        *sym == xkb::Keysym::new(xkb::keysyms::KEY_u)
            || *sym == xkb::Keysym::new(xkb::keysyms::KEY_U)
    });
    assert!(pipe_u.is_some());
    assert_eq!(
        pipe_u.unwrap().1,
        crate::input::KeyAction::PipeVisible(vec!["urlscan".to_string()])
    );

    let pipe_y = resolved.iter().find(|((_, sym), _)| {
        *sym == xkb::Keysym::new(xkb::keysyms::KEY_y)
            || *sym == xkb::Keysym::new(xkb::keysyms::KEY_Y)
    });
    assert!(pipe_y.is_some());
    assert_eq!(
        pipe_y.unwrap().1,
        crate::input::KeyAction::PipeSelection(vec![
            "sh".to_string(),
            "-c".to_string(),
            "wl-copy".to_string()
        ])
    );

    // Verify unbind "none" removed Ctrl+Shift+V
    let paste = resolved.iter().find(|((_, sym), _)| {
        *sym == xkb::Keysym::new(xkb::keysyms::KEY_v)
            || *sym == xkb::Keysym::new(xkb::keysyms::KEY_V)
    });
    assert!(paste.is_none());
}

#[test]
fn test_include_merging_and_override() {
    let temp_dir = std::env::temp_dir().join(format!("ftty_test_include_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let theme_path = temp_dir.join("theme.toml");
    fs::write(
        &theme_path,
        r##"
        [colors]
        foreground = "#aaaaaa"
        background = "#111111"

        [colors.palette]
        blue = "#0000ff"
        "##,
    )
    .unwrap();

    let main_path = temp_dir.join("ftty.toml");
    fs::write(
        &main_path,
        r##"
        include = "theme.toml"

        [font]
        size = 18.0

        [colors]
        background = "#222222"
        "##,
    )
    .unwrap();

    let config = Config::load(&main_path).expect("load config with include");
    assert_eq!(config.font_size(), 18.0);
    assert_eq!(config.foreground(), Rgb::new(170, 170, 170)); // from theme.toml
    assert_eq!(config.background(), Rgb::new(34, 34, 34)); // overridden by main
    let palette = config.build_palette();
    assert_eq!(palette[4], Rgb::new(0, 0, 255)); // from theme.toml

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_circular_include_rejection() {
    let temp_dir = std::env::temp_dir().join(format!("ftty_test_circ_{}", std::process::id()));
    fs::create_dir_all(&temp_dir).unwrap();

    let a_path = temp_dir.join("a.toml");
    let b_path = temp_dir.join("b.toml");

    fs::write(&a_path, "include = 'b.toml'").unwrap();
    fs::write(&b_path, "include = 'a.toml'").unwrap();

    let res = Config::load(&a_path);
    assert!(res.is_err());

    let _ = fs::remove_dir_all(&temp_dir);
}

#[test]
fn test_parse_font_size_and_family() {
    let toml_default = r#"
        [font]
        size = 14.0
    "#;
    let config: Config = toml::from_str(toml_default).unwrap();
    assert_eq!(config.font_size(), 14.0);

    let mut base = Config::default();
    let mut inc = Config::default();
    inc.font.size = Some(16.0);
    base.merge(inc);
    assert_eq!(base.font_size(), 16.0);
}

#[test]
fn test_include_keybindings_prompt_navigation() {
    let mut base = Config::default();
    let mut inc = Config::default();
    inc.keybindings.bindings.insert(
        "Ctrl+Shift+K".to_string(),
        crate::config::ActionDef::Simple("prompt_prev".to_string()),
    );
    inc.keybindings.bindings.insert(
        "Ctrl+Shift+J".to_string(),
        crate::config::ActionDef::Simple("prompt_next".to_string()),
    );
    base.merge(inc);
    let resolved = base.keybindings.resolve_bindings();
    let prev = resolved.iter().find(|((_, sym), _)| {
        *sym == xkb::Keysym::new(xkb::keysyms::KEY_k)
            || *sym == xkb::Keysym::new(xkb::keysyms::KEY_K)
    });
    assert!(prev.is_some());
    assert_eq!(prev.unwrap().1, crate::input::KeyAction::PromptPrev);
}

#[test]
fn test_clipboard_security_config() {
    let toml_str = r#"
        [clipboard]
        allow_osc52_read = true
        allow_osc52_write = false
    "#;
    let config: Config = toml::from_str(toml_str).unwrap();
    assert!(config.allow_osc52_read());
    assert!(!config.allow_osc52_write());

    let default_config = Config::default();
    assert!(!default_config.allow_osc52_read());
    assert!(default_config.allow_osc52_write());

    let mut base = Config::default();
    let mut inc = Config::default();
    inc.clipboard.allow_osc52_read = Some(true);
    base.merge(inc);
    assert!(base.allow_osc52_read());
}
