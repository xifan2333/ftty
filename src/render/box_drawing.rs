//! Built-in procedural geometry rendering for Box Drawing and Block Elements.
//!
//! Rather than relying on font rasterizers—which frequently introduce side-bearings
//! and subpixel padding causing visible gaps in TUI progress bars and box borders—
//! this module renders `U+2500..=U+259F` with pixel-perfect geometry directly
//! to the OpenGL vertex buffer.

#![forbid(unsafe_code)]

const SOLID_UV: [[f32; 2]; 2] = [[-1.0, -1.0]; 2];

/// Pushes a solid quad with `SOLID_UV` to the vertex buffer.
#[inline]
pub(crate) fn push_solid_quad(
    vertices: &mut Vec<f32>,
    [x0, y0, x1, y1]: [f32; 4],
    color: [f32; 4],
) {
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    for [x, y, u, v] in [
        [x0, y0, SOLID_UV[0][0], SOLID_UV[0][1]],
        [x1, y0, SOLID_UV[1][0], SOLID_UV[0][1]],
        [x0, y1, SOLID_UV[0][0], SOLID_UV[1][1]],
        [x1, y0, SOLID_UV[1][0], SOLID_UV[0][1]],
        [x1, y1, SOLID_UV[1][0], SOLID_UV[1][1]],
        [x0, y1, SOLID_UV[0][0], SOLID_UV[1][1]],
    ] {
        vertices.extend_from_slice(&[x, y, u, v, color[0], color[1], color[2], color[3]]);
    }
}

/// Pushes an arbitrary solid convex quadrilateral (two triangles) with `SOLID_UV`.
#[inline]
pub(crate) fn push_solid_poly_quad(
    vertices: &mut Vec<f32>,
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    p3: [f32; 2],
    color: [f32; 4],
) {
    for [x, y] in [p0, p1, p2, p1, p3, p2] {
        vertices.extend_from_slice(&[
            x,
            y,
            SOLID_UV[0][0],
            SOLID_UV[0][1],
            color[0],
            color[1],
            color[2],
            color[3],
        ]);
    }
}

/// Approximates a circular ring sector between inner and outer radii using segmented quads.
fn push_arc_ring(
    vertices: &mut Vec<f32>,
    cx: f32,
    cy: f32,
    radius: f32,
    thick: f32,
    start_angle: f32,
    color: [f32; 4],
) {
    let r_inner = (radius - thick / 2.0).max(0.0);
    let r_outer = radius + thick / 2.0;
    const STEPS: usize = 6;
    let step_angle = std::f32::consts::FRAC_PI_2 / STEPS as f32;

    for i in 0..STEPS {
        let a0 = start_angle + i as f32 * step_angle;
        let a1 = start_angle + (i + 1) as f32 * step_angle;
        let (sin0, cos0) = a0.sin_cos();
        let (sin1, cos1) = a1.sin_cos();

        let p0 = [cx + r_inner * cos0, cy + r_inner * sin0];
        let p1 = [cx + r_outer * cos0, cy + r_outer * sin0];
        let p2 = [cx + r_inner * cos1, cy + r_inner * sin1];
        let p3 = [cx + r_outer * cos1, cy + r_outer * sin1];

        push_solid_poly_quad(vertices, p0, p1, p2, p3, color);
    }
}

/// Returns `true` if the character is in the procedural box drawing or block elements range.
#[inline]
pub fn is_procedural_glyph(c: char) -> bool {
    matches!(c, '\u{2500}'..='\u{259F}')
}

/// Stroke type for a box drawing line branch.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Stroke {
    None,
    Light,
    Heavy,
    Double,
}

/// Renders a Block Element (`U+2580..=U+259F`) directly to vertices.
/// Returns `true` if the character was handled.
pub fn render_block_element(
    vertices: &mut Vec<f32>,
    c: char,
    x: f32,
    y: f32,
    width: f32,
    ch: f32,
    color: [f32; 4],
) -> bool {
    let x_right = x + width;
    let y_bot = y + ch;

    match c {
        // U+2580: Upper half block ▀
        '\u{2580}' => {
            let mid_y = (y + ch * 0.5).round();
            push_solid_quad(vertices, [x, y, x_right, mid_y], color);
            true
        }
        // U+2581..=U+2587: Lower 1/8 to 7/8 blocks (  ▂ ▃ ▄ ▅ ▆ ▇)
        '\u{2581}'..='\u{2587}' => {
            let fraction = (c as u32 - 0x2580) as f32 / 8.0;
            let top = (y + ch * (1.0 - fraction)).round();
            push_solid_quad(vertices, [x, top, x_right, y_bot], color);
            true
        }
        // U+2588: Full block █ (The quintessential progress bar block)
        '\u{2588}' => {
            push_solid_quad(vertices, [x, y, x_right, y_bot], color);
            true
        }
        // U+2589..=U+258F: Left 7/8 to 1/8 blocks (▉ ▊ ▋ ▌ ▍ ▎ ▏)
        '\u{2589}'..='\u{258F}' => {
            let fraction = (8 - (c as u32 - 0x2588)) as f32 / 8.0;
            let right = (x + width * fraction).round();
            push_solid_quad(vertices, [x, y, right, y_bot], color);
            true
        }
        // U+2590: Right half block ▐
        '\u{2590}' => {
            let mid_x = (x + width * 0.5).round();
            push_solid_quad(vertices, [mid_x, y, x_right, y_bot], color);
            true
        }
        // U+2591: Light shade ░ (25% opacity)
        '\u{2591}' => {
            let mut c = color;
            c[3] *= 0.25;
            push_solid_quad(vertices, [x, y, x_right, y_bot], c);
            true
        }
        // U+2592: Medium shade ▒ (50% opacity)
        '\u{2592}' => {
            let mut c = color;
            c[3] *= 0.50;
            push_solid_quad(vertices, [x, y, x_right, y_bot], c);
            true
        }
        // U+2593: Dark shade ▓ (75% opacity)
        '\u{2593}' => {
            let mut c = color;
            c[3] *= 0.75;
            push_solid_quad(vertices, [x, y, x_right, y_bot], c);
            true
        }
        // U+2594: Upper 1/8 block ▔
        '\u{2594}' => {
            let bot = (y + ch / 8.0).round();
            push_solid_quad(vertices, [x, y, x_right, bot], color);
            true
        }
        // U+2595: Right 1/8 block ▕
        '\u{2595}' => {
            let left = (x + width * 7.0 / 8.0).round();
            push_solid_quad(vertices, [left, y, x_right, y_bot], color);
            true
        }
        // U+2596..=U+259F: Quadrant blocks (2x2 sub-cells)
        '\u{2596}'..='\u{259F}' => {
            let mid_x = (x + width * 0.5).round();
            let mid_y = (y + ch * 0.5).round();
            let ul = [x, y, mid_x, mid_y];
            let ur = [mid_x, y, x_right, mid_y];
            let ll = [x, mid_y, mid_x, y_bot];
            let lr = [mid_x, mid_y, x_right, y_bot];

            match c {
                '\u{2596}' => push_solid_quad(vertices, ll, color),
                '\u{2597}' => push_solid_quad(vertices, lr, color),
                '\u{2598}' => push_solid_quad(vertices, ul, color),
                '\u{2599}' => {
                    push_solid_quad(vertices, [x, y, mid_x, y_bot], color);
                    push_solid_quad(vertices, lr, color);
                }
                '\u{259A}' => {
                    push_solid_quad(vertices, ul, color);
                    push_solid_quad(vertices, lr, color);
                }
                '\u{259B}' => {
                    push_solid_quad(vertices, [x, y, x_right, mid_y], color);
                    push_solid_quad(vertices, ll, color);
                }
                '\u{259C}' => {
                    push_solid_quad(vertices, [x, y, x_right, mid_y], color);
                    push_solid_quad(vertices, lr, color);
                }
                '\u{259D}' => push_solid_quad(vertices, ur, color),
                '\u{259E}' => {
                    push_solid_quad(vertices, ur, color);
                    push_solid_quad(vertices, ll, color);
                }
                '\u{259F}' => {
                    push_solid_quad(vertices, [x, mid_y, x_right, y_bot], color);
                    push_solid_quad(vertices, ur, color);
                }
                _ => return false,
            }
            true
        }
        _ => false,
    }
}

/// Renders a Box Drawing character (`U+2500..=U+257F`) directly to vertices.
/// Returns `true` if the character was handled.
pub fn render_box_drawing(
    vertices: &mut Vec<f32>,
    c: char,
    x: f32,
    y: f32,
    width: f32,
    ch: f32,
    color: [f32; 4],
) -> bool {
    let thick = (ch * 0.08).round().max(1.0);
    let heavy = (thick * 2.0).max(thick + 1.0);
    let gap = thick.max(1.0);
    let mid_x = (x + width * 0.5).floor();
    let mid_y = (y + ch * 0.5).floor();
    let x_right = x + width;
    let y_bot = y + ch;

    // Special: Dashed horizontal lines
    let draw_dashed_h = |vertices: &mut Vec<f32>, count: usize, t: f32| {
        let total_units = (count * 2 - 1) as f32;
        let dash_w = (width / total_units).max(1.0);
        let top = mid_y - t / 2.0;
        let bot = top + t;
        for k in 0..count {
            let x0 = x + (k * 2) as f32 * dash_w;
            let x1 = (x0 + dash_w).min(x_right);
            push_solid_quad(vertices, [x0, top, x1, bot], color);
        }
    };

    // Special: Dashed vertical lines
    let draw_dashed_v = |vertices: &mut Vec<f32>, count: usize, t: f32| {
        let total_units = (count * 2 - 1) as f32;
        let dash_h = (ch / total_units).max(1.0);
        let left = mid_x - t / 2.0;
        let right = left + t;
        for k in 0..count {
            let y0 = y + (k * 2) as f32 * dash_h;
            let y1 = (y0 + dash_h).min(y_bot);
            push_solid_quad(vertices, [left, y0, right, y1], color);
        }
    };

    // Special: Rounded corners (Arcs)
    let draw_arc = |vertices: &mut Vec<f32>, down: bool, right: bool| {
        let r = (width.min(ch) * 0.4).round().max(2.0);
        let t = thick;
        if down && right {
            // ╭ Arc down & right: connects [mid_x + r, mid_y] to [mid_x, mid_y + r]
            push_solid_quad(
                vertices,
                [mid_x + r, mid_y - t / 2.0, x_right, mid_y + t / 2.0],
                color,
            );
            push_solid_quad(
                vertices,
                [mid_x - t / 2.0, mid_y + r, mid_x + t / 2.0, y_bot],
                color,
            );
            push_arc_ring(
                vertices,
                mid_x + r,
                mid_y + r,
                r,
                t,
                std::f32::consts::PI,
                color,
            );
        } else if down && !right {
            // ╮ Arc down & left
            push_solid_quad(
                vertices,
                [x, mid_y - t / 2.0, mid_x - r, mid_y + t / 2.0],
                color,
            );
            push_solid_quad(
                vertices,
                [mid_x - t / 2.0, mid_y + r, mid_x + t / 2.0, y_bot],
                color,
            );
            push_arc_ring(
                vertices,
                mid_x - r,
                mid_y + r,
                r,
                t,
                1.5 * std::f32::consts::PI,
                color,
            );
        } else if !down && !right {
            // ╯ Arc up & left
            push_solid_quad(
                vertices,
                [x, mid_y - t / 2.0, mid_x - r, mid_y + t / 2.0],
                color,
            );
            push_solid_quad(
                vertices,
                [mid_x - t / 2.0, y, mid_x + t / 2.0, mid_y - r],
                color,
            );
            push_arc_ring(vertices, mid_x - r, mid_y - r, r, t, 0.0, color);
        } else {
            // ╰ Arc up & right
            push_solid_quad(
                vertices,
                [mid_x + r, mid_y - t / 2.0, x_right, mid_y + t / 2.0],
                color,
            );
            push_solid_quad(
                vertices,
                [mid_x - t / 2.0, y, mid_x + t / 2.0, mid_y - r],
                color,
            );
            push_arc_ring(
                vertices,
                mid_x + r,
                mid_y - r,
                r,
                t,
                std::f32::consts::FRAC_PI_2,
                color,
            );
        }
    };

    // Special: Diagonals
    let draw_diagonal = |vertices: &mut Vec<f32>, forward: bool| {
        let steps = (width.min(ch) as usize).max(4);
        let step_w = width / steps as f32;
        let step_h = ch / steps as f32;
        let t = thick;
        for s in 0..steps {
            let (x0, x1) = if forward {
                (x + s as f32 * step_w, x + (s + 1) as f32 * step_w)
            } else {
                (
                    x + width - (s + 1) as f32 * step_w,
                    x + width - s as f32 * step_w,
                )
            };
            let y0 = y + s as f32 * step_h;
            let y1 = (y0 + step_h + t).min(y_bot);
            push_solid_quad(vertices, [x0, y0, x1, y1], color);
        }
    };

    use Stroke::*;

    let strokes: (Stroke, Stroke, Stroke, Stroke) = match c {
        // Dashed lines
        '\u{2504}' => {
            draw_dashed_h(vertices, 3, thick);
            return true;
        }
        '\u{2505}' => {
            draw_dashed_h(vertices, 3, heavy);
            return true;
        }
        '\u{2506}' => {
            draw_dashed_v(vertices, 3, thick);
            return true;
        }
        '\u{2507}' => {
            draw_dashed_v(vertices, 3, heavy);
            return true;
        }
        '\u{2508}' => {
            draw_dashed_h(vertices, 4, thick);
            return true;
        }
        '\u{2509}' => {
            draw_dashed_h(vertices, 4, heavy);
            return true;
        }
        '\u{250A}' => {
            draw_dashed_v(vertices, 4, thick);
            return true;
        }
        '\u{250B}' => {
            draw_dashed_v(vertices, 4, heavy);
            return true;
        }
        '\u{254C}' => {
            draw_dashed_h(vertices, 2, thick);
            return true;
        }
        '\u{254D}' => {
            draw_dashed_h(vertices, 2, heavy);
            return true;
        }
        '\u{254E}' => {
            draw_dashed_v(vertices, 2, thick);
            return true;
        }
        '\u{254F}' => {
            draw_dashed_v(vertices, 2, heavy);
            return true;
        }

        // Rounded corners
        '\u{256D}' => {
            draw_arc(vertices, true, true);
            return true;
        }
        '\u{256E}' => {
            draw_arc(vertices, true, false);
            return true;
        }
        '\u{256F}' => {
            draw_arc(vertices, false, false);
            return true;
        }
        '\u{2570}' => {
            draw_arc(vertices, false, true);
            return true;
        }

        // Diagonals
        '\u{2571}' => {
            draw_diagonal(vertices, false);
            return true;
        }
        '\u{2572}' => {
            draw_diagonal(vertices, true);
            return true;
        }
        '\u{2573}' => {
            draw_diagonal(vertices, false);
            draw_diagonal(vertices, true);
            return true;
        }

        // Horizontal & Vertical
        '\u{2500}' => (Light, Light, None, None),
        '\u{2501}' => (Heavy, Heavy, None, None),
        '\u{2502}' => (None, None, Light, Light),
        '\u{2503}' => (None, None, Heavy, Heavy),

        // Corners (Down & Right)
        '\u{250C}' => (None, Light, None, Light),
        '\u{250D}' => (None, Heavy, None, Light),
        '\u{250E}' => (None, Light, None, Heavy),
        '\u{250F}' => (None, Heavy, None, Heavy),

        // Corners (Down & Left)
        '\u{2510}' => (Light, None, None, Light),
        '\u{2511}' => (Heavy, None, None, Light),
        '\u{2512}' => (Light, None, None, Heavy),
        '\u{2513}' => (Heavy, None, None, Heavy),

        // Corners (Up & Right)
        '\u{2514}' => (None, Light, Light, None),
        '\u{2515}' => (None, Heavy, Light, None),
        '\u{2516}' => (None, Light, Heavy, None),
        '\u{2517}' => (None, Heavy, Heavy, None),

        // Corners (Up & Left)
        '\u{2518}' => (Light, None, Light, None),
        '\u{2519}' => (Heavy, None, Light, None),
        '\u{251A}' => (Light, None, Heavy, None),
        '\u{251B}' => (Heavy, None, Heavy, None),

        // Tees (Vertical & Right)
        '\u{251C}' => (None, Light, Light, Light),
        '\u{251D}' => (None, Heavy, Light, Light),
        '\u{251E}' => (None, Light, Heavy, Light),
        '\u{251F}' => (None, Light, Light, Heavy),
        '\u{2520}' => (None, Light, Heavy, Heavy),
        '\u{2521}' => (None, Heavy, Heavy, Light),
        '\u{2522}' => (None, Heavy, Light, Heavy),
        '\u{2523}' => (None, Heavy, Heavy, Heavy),

        // Tees (Vertical & Left)
        '\u{2524}' => (Light, None, Light, Light),
        '\u{2525}' => (Heavy, None, Light, Light),
        '\u{2526}' => (Light, None, Heavy, Light),
        '\u{2527}' => (Light, None, Light, Heavy),
        '\u{2528}' => (Light, None, Heavy, Heavy),
        '\u{2529}' => (Heavy, None, Heavy, Light),
        '\u{252A}' => (Heavy, None, Light, Heavy),
        '\u{252B}' => (Heavy, None, Heavy, Heavy),

        // Tees (Down & Horizontal)
        '\u{252C}' => (Light, Light, None, Light),
        '\u{252D}' => (Heavy, Light, None, Light),
        '\u{252E}' => (Light, Heavy, None, Light),
        '\u{252F}' => (Heavy, Heavy, None, Light),
        '\u{2530}' => (Light, Light, None, Heavy),
        '\u{2531}' => (Heavy, Light, None, Heavy),
        '\u{2532}' => (Light, Heavy, None, Heavy),
        '\u{2533}' => (Heavy, Heavy, None, Heavy),

        // Tees (Up & Horizontal)
        '\u{2534}' => (Light, Light, Light, None),
        '\u{2535}' => (Heavy, Light, Light, None),
        '\u{2536}' => (Light, Heavy, Light, None),
        '\u{2537}' => (Heavy, Heavy, Light, None),
        '\u{2538}' => (Light, Light, Heavy, None),
        '\u{2539}' => (Heavy, Light, Heavy, None),
        '\u{253A}' => (Light, Heavy, Heavy, None),
        '\u{253B}' => (Heavy, Heavy, Heavy, None),

        // Crosses
        '\u{253C}' => (Light, Light, Light, Light),
        '\u{253D}' => (Heavy, Light, Light, Light),
        '\u{253E}' => (Light, Heavy, Light, Light),
        '\u{253F}' => (Heavy, Heavy, Light, Light),
        '\u{2540}' => (Light, Light, Heavy, Light),
        '\u{2541}' => (Light, Light, Light, Heavy),
        '\u{2542}' => (Light, Light, Heavy, Heavy),
        '\u{2543}' => (Heavy, Light, Heavy, Light),
        '\u{2544}' => (Light, Heavy, Heavy, Light),
        '\u{2545}' => (Heavy, Light, Light, Heavy),
        '\u{2546}' => (Light, Heavy, Light, Heavy),
        '\u{2547}' => (Heavy, Heavy, Heavy, Light),
        '\u{2548}' => (Heavy, Heavy, Light, Heavy),
        '\u{2549}' => (Heavy, Light, Heavy, Heavy),
        '\u{254A}' => (Light, Heavy, Heavy, Heavy),
        '\u{254B}' => (Heavy, Heavy, Heavy, Heavy),

        // Double lines
        '\u{2550}' => (Double, Double, None, None),
        '\u{2551}' => (None, None, Double, Double),

        // Double corners
        '\u{2552}' => (None, Double, None, Light),
        '\u{2553}' => (None, Light, None, Double),
        '\u{2554}' => (None, Double, None, Double),
        '\u{2555}' => (Double, None, None, Light),
        '\u{2556}' => (Light, None, None, Double),
        '\u{2557}' => (Double, None, None, Double),
        '\u{2558}' => (None, Double, Light, None),
        '\u{2559}' => (None, Light, Double, None),
        '\u{255A}' => (None, Double, Double, None),
        '\u{255B}' => (Double, None, Light, None),
        '\u{255C}' => (Light, None, Double, None),
        '\u{255D}' => (Double, None, Double, None),

        // Double tees
        '\u{255E}' => (None, Double, Light, Light),
        '\u{255F}' => (None, Light, Double, Double),
        '\u{2560}' => (None, Double, Double, Double),
        '\u{2561}' => (Double, None, Light, Light),
        '\u{2562}' => (Light, None, Double, Double),
        '\u{2563}' => (Double, None, Double, Double),
        '\u{2564}' => (Double, Double, None, Light),
        '\u{2565}' => (Light, Light, None, Double),
        '\u{2566}' => (Double, Double, None, Double),
        '\u{2567}' => (Double, Double, Light, None),
        '\u{2568}' => (Light, Light, Double, None),
        '\u{2569}' => (Double, Double, Double, None),

        // Double crosses
        '\u{256A}' => (Double, Double, Light, Light),
        '\u{256B}' => (Light, Light, Double, Double),
        '\u{256C}' => (Double, Double, Double, Double),

        // Half rays
        '\u{2574}' => (Light, None, None, None),
        '\u{2575}' => (None, None, Light, None),
        '\u{2576}' => (None, Light, None, None),
        '\u{2577}' => (None, None, None, Light),
        '\u{2578}' => (Heavy, None, None, None),
        '\u{2579}' => (None, None, Heavy, None),
        '\u{257A}' => (None, Heavy, None, None),
        '\u{257B}' => (None, None, None, Heavy),
        '\u{257C}' => (Light, Heavy, None, None),
        '\u{257D}' => (None, None, Light, Heavy),
        '\u{257E}' => (Heavy, Light, None, None),
        '\u{257F}' => (None, None, Heavy, Light),

        _ => return false,
    };

    let (left, right, up, down) = strokes;

    // Direct optimization for straight full-width lines
    if left == right && up == None && down == None {
        match left {
            Light => {
                push_solid_quad(
                    vertices,
                    [x, mid_y - thick / 2.0, x_right, mid_y + thick / 2.0],
                    color,
                );
                return true;
            }
            Heavy => {
                push_solid_quad(
                    vertices,
                    [x, mid_y - heavy / 2.0, x_right, mid_y + heavy / 2.0],
                    color,
                );
                return true;
            }
            Double => {
                push_solid_quad(
                    vertices,
                    [x, mid_y - gap - thick, x_right, mid_y - gap],
                    color,
                );
                push_solid_quad(
                    vertices,
                    [x, mid_y + gap, x_right, mid_y + gap + thick],
                    color,
                );
                return true;
            }
            None => return false,
        }
    }

    // Direct optimization for straight full-height lines
    if up == down && left == None && right == None {
        match up {
            Light => {
                push_solid_quad(
                    vertices,
                    [mid_x - thick / 2.0, y, mid_x + thick / 2.0, y_bot],
                    color,
                );
                return true;
            }
            Heavy => {
                push_solid_quad(
                    vertices,
                    [mid_x - heavy / 2.0, y, mid_x + heavy / 2.0, y_bot],
                    color,
                );
                return true;
            }
            Double => {
                push_solid_quad(
                    vertices,
                    [mid_x - gap - thick, y, mid_x - gap, y_bot],
                    color,
                );
                push_solid_quad(
                    vertices,
                    [mid_x + gap, y, mid_x + gap + thick, y_bot],
                    color,
                );
                return true;
            }
            None => return false,
        }
    }

    // Arm thickness resolution helper
    let stroke_thickness = |s: Stroke| match s {
        None => 0.0,
        Light => thick,
        Heavy => heavy,
        Double => gap * 2.0 + thick * 2.0,
    };

    let max_h = stroke_thickness(left).max(stroke_thickness(right));
    let max_v = stroke_thickness(up).max(stroke_thickness(down));
    let center_half_x = (max_v / 2.0).max(thick / 2.0);
    let center_half_y = (max_h / 2.0).max(thick / 2.0);

    // Left arm
    match left {
        Light => push_solid_quad(
            vertices,
            [
                x,
                mid_y - thick / 2.0,
                mid_x + center_half_x,
                mid_y + thick / 2.0,
            ],
            color,
        ),
        Heavy => push_solid_quad(
            vertices,
            [
                x,
                mid_y - heavy / 2.0,
                mid_x + center_half_x,
                mid_y + heavy / 2.0,
            ],
            color,
        ),
        Double => {
            push_solid_quad(
                vertices,
                [x, mid_y - gap - thick, mid_x + center_half_x, mid_y - gap],
                color,
            );
            push_solid_quad(
                vertices,
                [x, mid_y + gap, mid_x + center_half_x, mid_y + gap + thick],
                color,
            );
        }
        None => {}
    }

    // Right arm
    match right {
        Light => push_solid_quad(
            vertices,
            [
                mid_x - center_half_x,
                mid_y - thick / 2.0,
                x_right,
                mid_y + thick / 2.0,
            ],
            color,
        ),
        Heavy => push_solid_quad(
            vertices,
            [
                mid_x - center_half_x,
                mid_y - heavy / 2.0,
                x_right,
                mid_y + heavy / 2.0,
            ],
            color,
        ),
        Double => {
            push_solid_quad(
                vertices,
                [
                    mid_x - center_half_x,
                    mid_y - gap - thick,
                    x_right,
                    mid_y - gap,
                ],
                color,
            );
            push_solid_quad(
                vertices,
                [
                    mid_x - center_half_x,
                    mid_y + gap,
                    x_right,
                    mid_y + gap + thick,
                ],
                color,
            );
        }
        None => {}
    }

    // Up arm
    match up {
        Light => push_solid_quad(
            vertices,
            [
                mid_x - thick / 2.0,
                y,
                mid_x + thick / 2.0,
                mid_y + center_half_y,
            ],
            color,
        ),
        Heavy => push_solid_quad(
            vertices,
            [
                mid_x - heavy / 2.0,
                y,
                mid_x + heavy / 2.0,
                mid_y + center_half_y,
            ],
            color,
        ),
        Double => {
            push_solid_quad(
                vertices,
                [mid_x - gap - thick, y, mid_x - gap, mid_y + center_half_y],
                color,
            );
            push_solid_quad(
                vertices,
                [mid_x + gap, y, mid_x + gap + thick, mid_y + center_half_y],
                color,
            );
        }
        None => {}
    }

    // Down arm
    match down {
        Light => push_solid_quad(
            vertices,
            [
                mid_x - thick / 2.0,
                mid_y - center_half_y,
                mid_x + thick / 2.0,
                y_bot,
            ],
            color,
        ),
        Heavy => push_solid_quad(
            vertices,
            [
                mid_x - heavy / 2.0,
                mid_y - center_half_y,
                mid_x + heavy / 2.0,
                y_bot,
            ],
            color,
        ),
        Double => {
            push_solid_quad(
                vertices,
                [
                    mid_x - gap - thick,
                    mid_y - center_half_y,
                    mid_x - gap,
                    y_bot,
                ],
                color,
            );
            push_solid_quad(
                vertices,
                [
                    mid_x + gap,
                    mid_y - center_half_y,
                    mid_x + gap + thick,
                    y_bot,
                ],
                color,
            );
        }
        None => {}
    }

    true
}

/// Main entrypoint: renders either a Box Drawing or Block Element glyph.
pub fn render_procedural_glyph(
    vertices: &mut Vec<f32>,
    c: char,
    x: f32,
    y: f32,
    width: f32,
    ch: f32,
    color: [f32; 4],
) -> bool {
    if render_block_element(vertices, c, x, y, width, ch, color) {
        return true;
    }
    if render_box_drawing(vertices, c, x, y, width, ch, color) {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_full_block_covers_entire_cell() {
        let mut vertices = Vec::new();
        let color = [1.0, 0.0, 0.0, 1.0];
        let handled = render_procedural_glyph(&mut vertices, '█', 10.0, 20.0, 8.0, 16.0, color);
        assert!(handled);
        assert_eq!(vertices.len(), 48); // 1 quad = 6 vertices * 8 floats

        // Verify vertex boundaries touch cell edges: x0=10, y0=20, x1=18, y1=36
        assert_eq!(vertices[0], 10.0);
        assert_eq!(vertices[1], 20.0);
        assert_eq!(vertices[8], 18.0);
        assert_eq!(vertices[9], 20.0);
        assert_eq!(vertices[32], 18.0);
        assert_eq!(vertices[33], 36.0);
    }

    #[test]
    fn test_adjacent_blocks_have_zero_gap() {
        let mut vertices = Vec::new();
        let color = [0.0, 0.0, 1.0, 1.0];
        // Cell 1: x = 0..10
        render_procedural_glyph(&mut vertices, '█', 0.0, 0.0, 10.0, 20.0, color);
        // Cell 2: x = 10..20
        render_procedural_glyph(&mut vertices, '█', 10.0, 0.0, 10.0, 20.0, color);

        assert_eq!(vertices.len(), 96);
        assert_eq!(vertices[8], 10.0);
        assert_eq!(vertices[48], 10.0);
    }

    #[test]
    fn test_box_drawing_horizontal_line_seamless() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // Cell 1: ─ from 0..10
        assert!(render_procedural_glyph(
            &mut vertices,
            '─',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        // Cell 2: ─ from 10..20
        assert!(render_procedural_glyph(
            &mut vertices,
            '─',
            10.0,
            0.0,
            10.0,
            20.0,
            color
        ));

        assert_eq!(vertices.len(), 96);
        // Cell 1 right edge touches cell 2 left edge exactly at 10.0
        assert_eq!(vertices[8], 10.0);
        assert_eq!(vertices[48], 10.0);
    }

    #[test]
    fn test_box_drawing_vertical_line_seamless() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // Cell 1: │ from y=0..20
        assert!(render_procedural_glyph(
            &mut vertices,
            '│',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        // Cell 2: │ from y=20..40
        assert!(render_procedural_glyph(
            &mut vertices,
            '│',
            0.0,
            20.0,
            10.0,
            20.0,
            color
        ));

        assert_eq!(vertices.len(), 96);
        // Cell 1 bottom edge touches cell 2 top edge exactly at 20.0
        assert_eq!(vertices[33], 20.0);
        assert_eq!(vertices[48 + 1], 20.0);
    }

    #[test]
    fn test_fractional_blocks() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // Lower half block ▄
        assert!(render_procedural_glyph(
            &mut vertices,
            '▄',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices[1], 10.0);
        assert_eq!(vertices[33], 20.0);

        vertices.clear();
        // Left half block ▌
        assert!(render_procedural_glyph(
            &mut vertices,
            '▌',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices[0], 0.0);
        assert_eq!(vertices[8], 5.0);
    }

    #[test]
    fn test_corners_and_junctions() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // Corner ┌
        assert!(render_procedural_glyph(
            &mut vertices,
            '┌',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert!(!vertices.is_empty());

        vertices.clear();
        // Cross ┼
        assert!(render_procedural_glyph(
            &mut vertices,
            '┼',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert!(!vertices.is_empty());

        vertices.clear();
        // Double corner ╔
        assert!(render_procedural_glyph(
            &mut vertices,
            '╔',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert!(!vertices.is_empty());
    }

    #[test]
    fn test_rounded_corners_segmented_arc_not_solid_block() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // ╭ arc down & right: 2 stems (2 quads) + 6 ring segments (6 quads) = 8 quads = 8 * 48 floats = 384
        assert!(render_procedural_glyph(
            &mut vertices,
            '╭',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 8 * 48);

        // Verify all 4 rounded corners render
        for c in ['╭', '╮', '╯', '╰'] {
            vertices.clear();
            assert!(render_procedural_glyph(
                &mut vertices,
                c,
                0.0,
                0.0,
                10.0,
                20.0,
                color
            ));
            assert_eq!(vertices.len(), 8 * 48);
        }
    }
}
