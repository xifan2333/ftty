use std::fs;

use xkbcommon::xkb;

use crate::color::Rgb;
use crate::config::{Config, FreeTypeLoadFlags, FreeTypeLoadTarget, FreeTypeRenderTarget};
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

#[test]
fn test_flattened_colors_all_ansi_and_indexed() {
    let toml_str = r##"
    [colors]
    foreground = "#dcdcdc"
    background = "#181818"
    black = "#000000"
    red = "#cc0403"
    green = "#19cb00"
    yellow = "#cecb00"
    blue = "#0d73cc"
    magenta = "#cb1ed1"
    cyan = "#0dcdcd"
    white = "#e5e5e5"
    bright_black = "#767676"
    bright_red = "#f2201f"
    bright_green = "#23fd00"
    bright_yellow = "#fffd00"
    bright_blue = "#1a8fff"
    bright_magenta = "#fd28ff"
    bright_cyan = "#14ffff"
    bright_white = "#ffffff"

    [colors.indexed]
    16 = "#ff0088"
    255 = "#112233"
    "##;

    let config: Config = toml::from_str(toml_str).expect("parse flat colors toml");
    assert_eq!(config.foreground(), Rgb::new(0xdc, 0xdc, 0xdc));
    assert_eq!(config.background(), Rgb::new(0x18, 0x18, 0x18));

    let palette = config.build_palette();
    assert_eq!(palette[0], Rgb::new(0, 0, 0));
    assert_eq!(palette[1], Rgb::new(0xcc, 0x04, 0x03));
    assert_eq!(palette[2], Rgb::new(0x19, 0xcb, 0x00));
    assert_eq!(palette[3], Rgb::new(0xce, 0xcb, 0x00));
    assert_eq!(palette[4], Rgb::new(0x0d, 0x73, 0xcc));
    assert_eq!(palette[5], Rgb::new(0xcb, 0x1e, 0xd1));
    assert_eq!(palette[6], Rgb::new(0x0d, 0xcd, 0xcd));
    assert_eq!(palette[7], Rgb::new(0xe5, 0xe5, 0xe5));
    assert_eq!(palette[8], Rgb::new(0x76, 0x76, 0x76));
    assert_eq!(palette[9], Rgb::new(0xf2, 0x20, 0x1f));
    assert_eq!(palette[10], Rgb::new(0x23, 0xfd, 0x00));
    assert_eq!(palette[11], Rgb::new(0xff, 0xfd, 0x00));
    assert_eq!(palette[12], Rgb::new(0x1a, 0x8f, 0xff));
    assert_eq!(palette[13], Rgb::new(0xfd, 0x28, 0xff));
    assert_eq!(palette[14], Rgb::new(0x14, 0xff, 0xff));
    assert_eq!(palette[15], Rgb::new(0xff, 0xff, 0xff));
    assert_eq!(palette[16], Rgb::new(0xff, 0x00, 0x88));
    assert_eq!(palette[255], Rgb::new(0x11, 0x22, 0x33));
}

#[test]
fn test_legacy_palette_subtable_backward_compatibility() {
    let toml_str = r##"
    [colors]
    foreground = "#ffffff"
    background = "#000000"

    [colors.palette]
    red = "#ff0000"
    blue = "#0000ff"
    "##;

    let config: Config = toml::from_str(toml_str).expect("parse legacy palette toml");
    let palette = config.build_palette();
    assert_eq!(palette[1], Rgb::new(255, 0, 0));
    assert_eq!(palette[4], Rgb::new(0, 0, 255));
}

#[test]
fn test_case_insensitive_freetype_config_parsing() {
    let toml_pascal = r#"
    [font]
    freetype_load_target = "Light"
    freetype_render_target = "HorizontalLcd"
    freetype_load_flags = "NO_HINTING"
    "#;
    let cfg1: Config = toml::from_str(toml_pascal).expect("parse pascal");
    assert_eq!(cfg1.freetype_load_target(), FreeTypeLoadTarget::Light);
    assert_eq!(
        cfg1.freetype_render_target(),
        FreeTypeRenderTarget::HorizontalLcd
    );
    assert_eq!(cfg1.freetype_load_flags(), FreeTypeLoadFlags::NoHinting);

    let toml_snake_lower = r#"
    [font]
    freetype_load_target = "light"
    freetype_render_target = "horizontal_lcd"
    freetype_load_flags = "no_hinting"
    "#;
    let cfg2: Config = toml::from_str(toml_snake_lower).expect("parse snake lower");
    assert_eq!(cfg2.freetype_load_target(), FreeTypeLoadTarget::Light);
    assert_eq!(
        cfg2.freetype_render_target(),
        FreeTypeRenderTarget::HorizontalLcd
    );
    assert_eq!(cfg2.freetype_load_flags(), FreeTypeLoadFlags::NoHinting);

    let toml_upper = r#"
    [font]
    freetype_load_target = "LIGHT"
    freetype_render_target = "NORMAL"
    freetype_load_flags = "DEFAULT"
    "#;
    let cfg3: Config = toml::from_str(toml_upper).expect("parse upper");
    assert_eq!(cfg3.freetype_load_target(), FreeTypeLoadTarget::Light);
    assert_eq!(cfg3.freetype_render_target(), FreeTypeRenderTarget::Normal);
    assert_eq!(cfg3.freetype_load_flags(), FreeTypeLoadFlags::Default);

    // Invalid values are rejected with informative errors
    let toml_invalid = r#"
    [font]
    freetype_load_target = "super_heavy"
    "#;
    assert!(toml::from_str::<Config>(toml_invalid).is_err());
}
