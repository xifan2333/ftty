use std::fs;

use crate::color::Rgb;
use crate::config::Config;
use crate::grid::CursorShape;

#[test]
fn test_default_config_values() {
    let config = Config::default();
    assert_eq!(config.font_family(), "monospace");
    assert_eq!(config.font_size(), 14.0);
    assert_eq!(config.columns(), 80);
    assert_eq!(config.rows(), 24);
    assert_eq!(config.cursor_shape(), CursorShape::Block);
    assert_eq!(config.foreground(), Rgb::new(220, 220, 220));
    assert_eq!(config.background(), Rgb::new(24, 24, 24));
}

#[test]
fn test_parse_basic_toml() {
    let toml_str = r##"
    [font]
    family = "JetBrains Mono"
    size = 16.5

    [window]
    columns = 100
    rows = 30

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
    assert_eq!(config.font_families(), vec!["JetBrains Mono"]);
    assert_eq!(config.font_size(), 16.5);
    assert_eq!(config.columns(), 100);
    assert_eq!(config.rows(), 30);
    assert_eq!(config.cursor_shape(), CursorShape::Beam);
    assert_eq!(config.foreground(), Rgb::new(255, 255, 255));
    assert_eq!(config.background(), Rgb::new(0, 0, 0));

    let palette = config.build_palette();
    assert_eq!(palette[1], Rgb::new(255, 0, 0));
    assert_eq!(palette[9], Rgb::new(255, 85, 85));
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
    scrollback_up_page = ["Shift+PageUp", "Shift+KP_PageUp"]
    scrollback_down_page = "Shift+PageDown"
    clipboard_copy = "Ctrl+Shift+C"
    clipboard_paste = "none"
    "##;

    let config: Config = toml::from_str(toml_str).expect("parse toml");
    assert_eq!(config.scrollback_lines(), 5000);
    assert_eq!(config.scroll_multiplier(), 5.5);
    assert!(!config.auto_scroll());

    let up_combos = config.keybindings.scrollback_up_page.unwrap();
    assert_eq!(
        up_combos.to_combos(),
        vec!["Shift+PageUp", "Shift+KP_PageUp"]
    );

    let paste_combos = config.keybindings.clipboard_paste.unwrap();
    assert_eq!(paste_combos.to_combos(), Vec::<&str>::new());
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
fn test_parse_subpixel_antialiasing_config() {
    let toml_default = r#"
        [font]
        size = 14.0
    "#;
    let config: Config = toml::from_str(toml_default).unwrap();
    assert!(!config.font_subpixel());

    let toml_enabled = r#"
        [font]
        size = 14.0
        subpixel = true
    "#;
    let config_subpixel: Config = toml::from_str(toml_enabled).unwrap();
    assert!(config_subpixel.font_subpixel());

    let mut base = Config::default();
    let mut inc = Config::default();
    inc.font.subpixel = Some(true);
    base.merge(inc);
    assert!(base.font_subpixel());
}

#[test]
fn test_include_keybindings_prompt_navigation() {
    let mut base = Config::default();
    let mut inc = Config::default();
    inc.keybindings.prompt_prev =
        Some(crate::config::KeyCombos::Single("Ctrl+Shift+K".to_string()));
    inc.keybindings.prompt_next =
        Some(crate::config::KeyCombos::Single("Ctrl+Shift+J".to_string()));
    base.merge(inc);
    assert_eq!(
        base.keybindings.prompt_prev.unwrap().to_combos(),
        vec!["Ctrl+Shift+K"]
    );
    assert_eq!(
        base.keybindings.prompt_next.unwrap().to_combos(),
        vec!["Ctrl+Shift+J"]
    );
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
