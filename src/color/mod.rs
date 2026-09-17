//! Terminal color representations and default 256-color palette.

pub mod palette;

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

pub use palette::default_256_palette;

#[cfg(test)]
mod tests;
