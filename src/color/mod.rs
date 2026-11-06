//! Terminal color representations and default 256-color palette.

mod palette;

pub use palette::default_256_palette;

/// An RGB color with 8-bit channels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

impl std::fmt::Display for Rgb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{:02x}{:02x}{:02x}", self.r, self.g, self.b)
    }
}

impl std::str::FromStr for Rgb {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let s = s.trim();
        let s = s.strip_prefix('#').unwrap_or(s);
        let s = s.strip_prefix("0x").unwrap_or(s);
        if !s.is_ascii() {
            return Err(format!(
                "invalid hex color containing non-ASCII characters: '{s}'"
            ));
        }
        if s.len() == 6 {
            let r = u8::from_str_radix(&s[0..2], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&s[2..4], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&s[4..6], 16).map_err(|e| e.to_string())?;
            Ok(Self::new(r, g, b))
        } else if s.len() == 3 {
            let r = u8::from_str_radix(&s[0..1], 16).map_err(|e| e.to_string())?;
            let g = u8::from_str_radix(&s[1..2], 16).map_err(|e| e.to_string())?;
            let b = u8::from_str_radix(&s[2..3], 16).map_err(|e| e.to_string())?;
            Ok(Self::new(r * 17, g * 17, b * 17))
        } else {
            Err(format!(
                "invalid hex color: '{s}', expected 3 or 6 hex digits"
            ))
        }
    }
}

impl<'de> serde::Deserialize<'de> for Rgb {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct RgbVisitor;

        impl serde::de::Visitor<'_> for RgbVisitor {
            type Value = Rgb;

            fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
                formatter.write_str("a hex color string (e.g. \"#181818\" or \"#fff\")")
            }

            fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                v.parse::<Rgb>().map_err(serde::de::Error::custom)
            }
        }

        deserializer.deserialize_str(RgbVisitor)
    }
}

impl serde::Serialize for Rgb {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

/// Color specification for terminal text and backgrounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Color {
    #[default]
    DefaultForeground,
    DefaultBackground,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Color {
    /// Resolves this color to an RGB value using the provided palette and defaults.
    #[must_use]
    pub fn to_rgb(self, palette: &[Rgb; 256], default_fg: Rgb, default_bg: Rgb) -> Rgb {
        match self {
            Self::DefaultForeground => default_fg,
            Self::DefaultBackground => default_bg,
            Self::Indexed(idx) => palette[idx as usize],
            Self::Rgb(r, g, b) => Rgb::new(r, g, b),
        }
    }
}

/// Parses a color string in hex (`#RRGGBB`, `#RGB`) or X11 notation (`rgb:RR/GG/BB` or `rgb:RRRR/GGGG/BBBB`).
#[must_use]
pub fn parse_color_spec(s: &str) -> Option<Rgb> {
    let s = s.trim();
    if let Some(rest) = s.strip_prefix("rgb:") {
        let mut parts = rest.split('/');
        let r_str = parts.next()?;
        let g_str = parts.next()?;
        let b_str = parts.next()?;
        if parts.next().is_some() {
            return None;
        }
        let r = parse_x11_component(r_str)?;
        let g = parse_x11_component(g_str)?;
        let b = parse_x11_component(b_str)?;
        return Some(Rgb::new(r, g, b));
    }
    s.parse::<Rgb>().ok()
}

fn parse_x11_component(s: &str) -> Option<u8> {
    if !s.is_ascii() {
        return None;
    }
    match s.len() {
        1 => u8::from_str_radix(s, 16).ok().map(|v| v * 17),
        2 => u8::from_str_radix(s, 16).ok(),
        3 | 4 => u8::from_str_radix(&s[0..2], 16).ok(),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    #[test]
    fn test_parse_color_spec() {
        assert_eq!(parse_color_spec("#181818"), Some(Rgb::new(24, 24, 24)));
        assert_eq!(parse_color_spec("#fff"), Some(Rgb::new(255, 255, 255)));
        assert_eq!(parse_color_spec("rgb:18/24/36"), Some(Rgb::new(24, 36, 54)));
        assert_eq!(
            parse_color_spec("rgb:1818/2424/3636"),
            Some(Rgb::new(24, 36, 54))
        );
        assert_eq!(parse_color_spec("invalid"), None);
        assert_eq!(parse_color_spec("rgb:€/00/00"), None);
        assert_eq!(parse_color_spec("rgb:00/€/00"), None);
        assert_eq!(parse_color_spec("rgb:00/00/€€"), None);
        assert_eq!(parse_color_spec("rgb:😀/00/00"), None);
    }
}
