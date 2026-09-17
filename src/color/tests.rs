use crate::color::palette::default_256_palette;
use crate::color::{Color, Rgb};

#[test]
fn test_default_palette_values() {
    let palette = default_256_palette();
    assert_eq!(palette[0], Rgb::new(0, 0, 0));
    assert_eq!(palette[15], Rgb::new(255, 255, 255));
    assert_eq!(palette[16], Rgb::new(0, 0, 0));
    assert_eq!(palette[231], Rgb::new(255, 255, 255));
    assert_eq!(palette[232], Rgb::new(8, 8, 8));
    assert_eq!(palette[255], Rgb::new(238, 238, 238));
}

#[test]
fn test_color_resolution() {
    let palette = default_256_palette();
    let fg = Rgb::new(200, 200, 200);
    let bg = Rgb::new(20, 20, 20);

    assert_eq!(Color::DefaultForeground.to_rgb(&palette, fg, bg), fg);
    assert_eq!(Color::DefaultBackground.to_rgb(&palette, fg, bg), bg);
    assert_eq!(
        Color::Indexed(1).to_rgb(&palette, fg, bg),
        Rgb::new(205, 0, 0)
    );
    assert_eq!(
        Color::Rgb(42, 43, 44).to_rgb(&palette, fg, bg),
        Rgb::new(42, 43, 44)
    );
}

#[test]
fn test_rgb_from_hex_and_serde() {
    assert_eq!("#181818".parse::<Rgb>().unwrap(), Rgb::new(24, 24, 24));
    assert_eq!("181818".parse::<Rgb>().unwrap(), Rgb::new(24, 24, 24));
    assert_eq!("0x181818".parse::<Rgb>().unwrap(), Rgb::new(24, 24, 24));
    assert_eq!("#fff".parse::<Rgb>().unwrap(), Rgb::new(255, 255, 255));
    assert_eq!("#000".parse::<Rgb>().unwrap(), Rgb::new(0, 0, 0));
    assert_eq!(Rgb::new(24, 24, 24).to_string(), "#181818");
    assert!("invalid".parse::<Rgb>().is_err());
    assert!("#1234".parse::<Rgb>().is_err());
    assert!("aéabc".parse::<Rgb>().is_err());
    assert!("#éff".parse::<Rgb>().is_err());
}
