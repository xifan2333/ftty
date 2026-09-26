//! Terminal color representations and default 256-color palette.

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

/// Constructs the standard xterm-compatible 256-color palette.
#[must_use]
pub fn default_256_palette() -> [Rgb; 256] {
    let mut palette = [Rgb::new(0, 0, 0); 256];

    // Standard 16 colors
    palette[0] = Rgb::new(0, 0, 0); // Black
    palette[1] = Rgb::new(205, 0, 0); // Red
    palette[2] = Rgb::new(0, 205, 0); // Green
    palette[3] = Rgb::new(205, 205, 0); // Yellow
    palette[4] = Rgb::new(0, 0, 238); // Blue
    palette[5] = Rgb::new(205, 0, 205); // Magenta
    palette[6] = Rgb::new(0, 205, 205); // Cyan
    palette[7] = Rgb::new(229, 229, 229); // White
    palette[8] = Rgb::new(127, 127, 127); // Bright Black
    palette[9] = Rgb::new(255, 0, 0); // Bright Red
    palette[10] = Rgb::new(0, 255, 0); // Bright Green
    palette[11] = Rgb::new(255, 255, 0); // Bright Yellow
    palette[12] = Rgb::new(92, 92, 255); // Bright Blue
    palette[13] = Rgb::new(255, 0, 255); // Bright Magenta
    palette[14] = Rgb::new(0, 255, 255); // Bright Cyan
    palette[15] = Rgb::new(255, 255, 255); // Bright White

    // 6x6x6 color cube (indices 16..=231)
    let steps = [0x00, 0x5f, 0x87, 0xaf, 0xd7, 0xff];
    let mut idx = 16;
    for r in steps {
        for g in steps {
            for b in steps {
                palette[idx] = Rgb::new(r, g, b);
                idx += 1;
            }
        }
    }

    // 24 grayscale levels (indices 232..=255)
    for i in 0..24 {
        let v = 8 + i * 10;
        palette[232 + i as usize] = Rgb::new(v, v, v);
    }

    palette
}

pub mod palette {
    pub use super::default_256_palette;
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
