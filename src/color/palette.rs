//! Standard xterm-compatible 256-color palette definition.

use crate::color::Rgb;

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
