//! Procedural geometric vertex generation for Unicode Box Drawing and Block Elements.

pub const SOLID_UV: [[f32; 2]; 2] = [[-1.0, -1.0]; 2];

/// Pushes a solid quad with `SOLID_UV` to the vertex buffer.
#[inline]
pub fn push_solid_quad(vertices: &mut Vec<f32>, [x0, y0, x1, y1]: [f32; 4], color: [f32; 4]) {
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let [u, v] = [SOLID_UV[0][0], SOLID_UV[0][1]];
    let [r, g, b, a] = color;
    vertices.extend_from_slice(&[
        x0, y0, u, v, r, g, b, a, x1, y0, u, v, r, g, b, a, x0, y1, u, v, r, g, b, a, x1, y0, u, v,
        r, g, b, a, x1, y1, u, v, r, g, b, a, x0, y1, u, v, r, g, b, a,
    ]);
}

/// Pushes an arbitrary solid convex quadrilateral (two triangles) with `SOLID_UV`.
#[inline]
pub fn push_solid_poly_quad(
    vertices: &mut Vec<f32>,
    p0: [f32; 2],
    p1: [f32; 2],
    p2: [f32; 2],
    p3: [f32; 2],
    color: [f32; 4],
) {
    let [u, v] = [SOLID_UV[0][0], SOLID_UV[0][1]];
    let [r, g, b, a] = color;
    vertices.extend_from_slice(&[
        p0[0], p0[1], u, v, r, g, b, a, p1[0], p1[1], u, v, r, g, b, a, p2[0], p2[1], u, v, r, g,
        b, a, p1[0], p1[1], u, v, r, g, b, a, p3[0], p3[1], u, v, r, g, b, a, p2[0], p2[1], u, v,
        r, g, b, a,
    ]);
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
/// One solid rect inside a block element, expressed as cell-length fractions
/// (0.0 = cell start edge, 1.0 = cell end edge) with an alpha multiplier for
/// the shade glyphs. Interior boundaries are rounded, cell edges stay raw.
#[derive(Copy, Clone)]
struct BlockRect {
    x0: f32,
    y0: f32,
    x1: f32,
    y1: f32,
    alpha: f32,
}

const fn br(x0: f32, y0: f32, x1: f32, y1: f32, alpha: f32) -> BlockRect {
    BlockRect {
        x0,
        y0,
        x1,
        y1,
        alpha,
    }
}

/// Resolves a cell fraction to a pixel coordinate: cell-start edges and
/// cell-end edges stay raw while interior boundaries are rounded to avoid
/// bleeding into neighbouring cells.
fn block_boundary(base: f32, end: f32, f: f32) -> f32 {
    if f <= 0.0 {
        base
    } else if f >= 1.0 {
        end
    } else {
        (base + f * (end - base)).round()
    }
}

/// Maps every block element character (U+2580..U+259F) to its component rects.
static BLOCK_RECT_TABLE: [Option<&'static [BlockRect]>; 32] = [
    Some(&[br(0.0, 0.0, 1.0, 0.5, 1.0)]),
    Some(&[br(0.0, 0.875, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.75, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.625, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.5, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.375, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.25, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.125, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.875, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.75, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.625, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.5, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.375, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.25, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.125, 1.0, 1.0)]),
    Some(&[br(0.5, 0.0, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 1.0, 1.0, 0.25)]),
    Some(&[br(0.0, 0.0, 1.0, 1.0, 0.50)]),
    Some(&[br(0.0, 0.0, 1.0, 1.0, 0.75)]),
    Some(&[br(0.0, 0.0, 1.0, 0.125, 1.0)]),
    Some(&[br(0.875, 0.0, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.5, 0.5, 1.0, 1.0)]),
    Some(&[br(0.5, 0.5, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.5, 0.5, 1.0)]),
    Some(&[br(0.0, 0.0, 0.5, 1.0, 1.0), br(0.5, 0.5, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 0.5, 0.5, 1.0), br(0.5, 0.5, 1.0, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 1.0, 0.5, 1.0), br(0.0, 0.5, 0.5, 1.0, 1.0)]),
    Some(&[br(0.0, 0.0, 1.0, 0.5, 1.0), br(0.5, 0.5, 1.0, 1.0, 1.0)]),
    Some(&[br(0.5, 0.0, 1.0, 0.5, 1.0)]),
    Some(&[br(0.5, 0.0, 1.0, 0.5, 1.0), br(0.0, 0.5, 0.5, 1.0, 1.0)]),
    Some(&[br(0.0, 0.5, 1.0, 1.0, 1.0), br(0.5, 0.0, 1.0, 0.5, 1.0)]),
];

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
    let idx = c as usize;
    if !(0x2580..0x25A0).contains(&idx) {
        return false;
    }
    let Some(rects) = BLOCK_RECT_TABLE[idx - 0x2580] else {
        return false;
    };

    let x_right = x + width;
    let y_bot = y + ch;
    for r in rects {
        let mut quad_color = color;
        quad_color[3] *= r.alpha;
        push_solid_quad(
            vertices,
            [
                block_boundary(x, x_right, r.x0),
                block_boundary(y, y_bot, r.y0),
                block_boundary(x, x_right, r.x1),
                block_boundary(y, y_bot, r.y1),
            ],
            quad_color,
        );
    }
    true
}

/// Shared geometry metrics for one box drawing cell.
///
/// All thicknesses and midpoints derive from the cell rect once, so the
/// per-stroke emitters stay free of repeated arithmetic.
struct StrokeSpecs {
    x: f32,
    y: f32,
    width: f32,
    ch: f32,
    thick: f32,
    heavy: f32,
    gap: f32,
    mid_x: f32,
    mid_y: f32,
    x_right: f32,
    y_bot: f32,
}

impl StrokeSpecs {
    fn new(x: f32, y: f32, width: f32, ch: f32) -> Self {
        let thick = (ch * 0.08).round().max(1.0);
        Self {
            x,
            y,
            width,
            ch,
            thick,
            heavy: (thick * 2.0).max(thick + 1.0),
            gap: thick.max(1.0),
            mid_x: (x + width * 0.5).floor(),
            mid_y: (y + ch * 0.5).floor(),
            x_right: x + width,
            y_bot: y + ch,
        }
    }

    /// Total band thickness an arm of this stroke occupies across the center line.
    fn thickness(&self, stroke: Stroke) -> f32 {
        match stroke {
            Stroke::None => 0.0,
            Stroke::Light => self.thick,
            Stroke::Heavy => self.heavy,
            Stroke::Double => self.gap * 2.0 + self.thick * 2.0,
        }
    }
}

/// Which axis an arm of a box glyph spans.
#[derive(Copy, Clone)]
enum ArmAxis {
    Horizontal,
    Vertical,
}

/// Pushes the quads for a single arm of a box glyph.
///
/// The arm occupies the longitudinal range `a0..a1` along `axis`; its stroke
/// band is centered on the perpendicular midline (`mid_y` for horizontal arms,
/// `mid_x` for vertical arms).
fn push_arm(
    vertices: &mut Vec<f32>,
    specs: &StrokeSpecs,
    stroke: Stroke,
    axis: ArmAxis,
    a0: f32,
    a1: f32,
    color: [f32; 4],
) {
    let horizontal = matches!(axis, ArmAxis::Horizontal);
    let midpoint = if horizontal { specs.mid_y } else { specs.mid_x };
    let band = |t: f32, offset: f32| -> [f32; 4] {
        if horizontal {
            [a0, midpoint + offset, a1, midpoint + offset + t]
        } else {
            [midpoint + offset, a0, midpoint + offset + t, a1]
        }
    };
    match stroke {
        Stroke::None => {}
        Stroke::Light => push_solid_quad(vertices, band(specs.thick, -specs.thick / 2.0), color),
        Stroke::Heavy => push_solid_quad(vertices, band(specs.heavy, -specs.heavy / 2.0), color),
        Stroke::Double => {
            push_solid_quad(vertices, band(specs.thick, -specs.gap - specs.thick), color);
            push_solid_quad(vertices, band(specs.thick, specs.gap), color);
        }
    }
}

/// Emits a dashed horizontal line split into `count` dashes of stroke width `t`.
fn draw_dashed_h(
    vertices: &mut Vec<f32>,
    specs: &StrokeSpecs,
    count: usize,
    t: f32,
    color: [f32; 4],
) {
    let total_units = (count * 2 - 1) as f32;
    let dash_w = (specs.width / total_units).max(1.0);
    let top = specs.mid_y - t / 2.0;
    let bot = top + t;
    for k in 0..count {
        let x0 = specs.x + (k * 2) as f32 * dash_w;
        let x1 = (x0 + dash_w).min(specs.x_right);
        push_solid_quad(vertices, [x0, top, x1, bot], color);
    }
}

/// Emits a dashed vertical line split into `count` dashes of stroke width `t`.
fn draw_dashed_v(
    vertices: &mut Vec<f32>,
    specs: &StrokeSpecs,
    count: usize,
    t: f32,
    color: [f32; 4],
) {
    let total_units = (count * 2 - 1) as f32;
    let dash_h = (specs.ch / total_units).max(1.0);
    let left = specs.mid_x - t / 2.0;
    let right = left + t;
    for k in 0..count {
        let y0 = specs.y + (k * 2) as f32 * dash_h;
        let y1 = (y0 + dash_h).min(specs.y_bot);
        push_solid_quad(vertices, [left, y0, right, y1], color);
    }
}

/// Emits a rounded corner: two stems meeting at a ring arc in the selected quadrant.
fn draw_arc(
    vertices: &mut Vec<f32>,
    specs: &StrokeSpecs,
    down: bool,
    right: bool,
    color: [f32; 4],
) {
    let r = (specs.width.min(specs.ch) * 0.4).round().max(2.0);
    let t = specs.thick;

    // Stems run from the arc center outward to the cell edges.
    let (h0, h1) = if right {
        (specs.mid_x + r, specs.x_right)
    } else {
        (specs.x, specs.mid_x - r)
    };
    push_solid_quad(
        vertices,
        [h0, specs.mid_y - t / 2.0, h1, specs.mid_y + t / 2.0],
        color,
    );

    let (v0, v1) = if down {
        (specs.mid_y + r, specs.y_bot)
    } else {
        (specs.y, specs.mid_y - r)
    };
    push_solid_quad(
        vertices,
        [specs.mid_x - t / 2.0, v0, specs.mid_x + t / 2.0, v1],
        color,
    );

    // Ring center sits `r` in from the selected corner; sweep starts at the stem side.
    let cx = if right {
        specs.mid_x + r
    } else {
        specs.mid_x - r
    };
    let cy = if down {
        specs.mid_y + r
    } else {
        specs.mid_y - r
    };
    let start = match (down, right) {
        (true, true) => std::f32::consts::PI,
        (true, false) => 1.5 * std::f32::consts::PI,
        (false, false) => 0.0,
        (false, true) => std::f32::consts::FRAC_PI_2,
    };
    push_arc_ring(vertices, cx, cy, r, t, start, color);
}

/// Emits a diagonal stroke stepped across the cell as short quads.
fn draw_diagonal(vertices: &mut Vec<f32>, specs: &StrokeSpecs, forward: bool, color: [f32; 4]) {
    let steps = (specs.width.min(specs.ch) as usize).max(4);
    let step_w = specs.width / steps as f32;
    let step_h = specs.ch / steps as f32;
    let t = specs.thick;
    for s in 0..steps {
        let (x0, x1) = if forward {
            (
                specs.x + s as f32 * step_w,
                specs.x + (s + 1) as f32 * step_w,
            )
        } else {
            (
                specs.x + specs.width - (s + 1) as f32 * step_w,
                specs.x + specs.width - s as f32 * step_w,
            )
        };
        let y0 = specs.y + s as f32 * step_h;
        let y1 = (y0 + step_h + t).min(specs.y_bot);
        push_solid_quad(vertices, [x0, y0, x1, y1], color);
    }
}

/// Pushes a straight full-width horizontal line. Returns `false` only for the
/// degenerate `None` arm, which cannot occur from the stroke table.
fn push_straight_h(
    vertices: &mut Vec<f32>,
    specs: &StrokeSpecs,
    stroke: Stroke,
    color: [f32; 4],
) -> bool {
    if stroke == Stroke::None {
        return false;
    }
    push_arm(
        vertices,
        specs,
        stroke,
        ArmAxis::Horizontal,
        specs.x,
        specs.x_right,
        color,
    );
    true
}

/// Pushes a straight full-height vertical line. Returns `false` only for the
/// degenerate `None` arm, which cannot occur from the stroke table.
fn push_straight_v(
    vertices: &mut Vec<f32>,
    specs: &StrokeSpecs,
    stroke: Stroke,
    color: [f32; 4],
) -> bool {
    if stroke == Stroke::None {
        return false;
    }
    push_arm(
        vertices,
        specs,
        stroke,
        ArmAxis::Vertical,
        specs.y,
        specs.y_bot,
        color,
    );
    true
}

/// The box drawing operation derived from a character's code point, resolved
/// through a dense table indexed by `U+2500..=U+257F` offset for O(1) lookup.
#[derive(Copy, Clone)]
enum BoxGlyph {
    /// Standard four-arm crossing / corner drawn from stroke widths.
    Strokes(Stroke, Stroke, Stroke, Stroke),
    /// Horizontal dashes: `(count, heavy)`.
    DashedH(usize, bool),
    /// Vertical dashes: `(count, heavy)`.
    DashedV(usize, bool),
    /// Rounded corner arc: `(down, right)`.
    Arc(bool, bool),
    /// Diagonal stroke `forward`.
    Diagonal(bool),
    /// Both diagonals crossed (╳).
    DiagonalBoth,
}

/// Decision table for every box drawing character (U+2500..U+257F).
static BOX_GLYPH_TABLE: [Option<BoxGlyph>; 128] = {
    // Stroke variants shortened in table scope for one-row-per-character data.
    use Stroke::{Double as D, Heavy as H, Light as L, None as N};

    let mut t = [None; 128];
    t[0] = Some(BoxGlyph::Strokes(L, L, N, N));
    t[0x2501 - 0x2500] = Some(BoxGlyph::Strokes(H, H, N, N));
    t[0x2502 - 0x2500] = Some(BoxGlyph::Strokes(N, N, L, L));
    t[0x2503 - 0x2500] = Some(BoxGlyph::Strokes(N, N, H, H));
    t[0x2504 - 0x2500] = Some(BoxGlyph::DashedH(3, false));
    t[0x2505 - 0x2500] = Some(BoxGlyph::DashedH(3, true));
    t[0x2506 - 0x2500] = Some(BoxGlyph::DashedV(3, false));
    t[0x2507 - 0x2500] = Some(BoxGlyph::DashedV(3, true));
    t[0x2508 - 0x2500] = Some(BoxGlyph::DashedH(4, false));
    t[0x2509 - 0x2500] = Some(BoxGlyph::DashedH(4, true));
    t[0x250A - 0x2500] = Some(BoxGlyph::DashedV(4, false));
    t[0x250B - 0x2500] = Some(BoxGlyph::DashedV(4, true));
    t[0x250C - 0x2500] = Some(BoxGlyph::Strokes(N, L, N, L));
    t[0x250D - 0x2500] = Some(BoxGlyph::Strokes(N, H, N, L));
    t[0x250E - 0x2500] = Some(BoxGlyph::Strokes(N, L, N, H));
    t[0x250F - 0x2500] = Some(BoxGlyph::Strokes(N, H, N, H));
    t[0x2510 - 0x2500] = Some(BoxGlyph::Strokes(L, N, N, L));
    t[0x2511 - 0x2500] = Some(BoxGlyph::Strokes(H, N, N, L));
    t[0x2512 - 0x2500] = Some(BoxGlyph::Strokes(L, N, N, H));
    t[0x2513 - 0x2500] = Some(BoxGlyph::Strokes(H, N, N, H));
    t[0x2514 - 0x2500] = Some(BoxGlyph::Strokes(N, L, L, N));
    t[0x2515 - 0x2500] = Some(BoxGlyph::Strokes(N, H, L, N));
    t[0x2516 - 0x2500] = Some(BoxGlyph::Strokes(N, L, H, N));
    t[0x2517 - 0x2500] = Some(BoxGlyph::Strokes(N, H, H, N));
    t[0x2518 - 0x2500] = Some(BoxGlyph::Strokes(L, N, L, N));
    t[0x2519 - 0x2500] = Some(BoxGlyph::Strokes(H, N, L, N));
    t[0x251A - 0x2500] = Some(BoxGlyph::Strokes(L, N, H, N));
    t[0x251B - 0x2500] = Some(BoxGlyph::Strokes(H, N, H, N));
    t[0x251C - 0x2500] = Some(BoxGlyph::Strokes(N, L, L, L));
    t[0x251D - 0x2500] = Some(BoxGlyph::Strokes(N, H, L, L));
    t[0x251E - 0x2500] = Some(BoxGlyph::Strokes(N, L, H, L));
    t[0x251F - 0x2500] = Some(BoxGlyph::Strokes(N, L, L, H));
    t[0x2520 - 0x2500] = Some(BoxGlyph::Strokes(N, L, H, H));
    t[0x2521 - 0x2500] = Some(BoxGlyph::Strokes(N, H, H, L));
    t[0x2522 - 0x2500] = Some(BoxGlyph::Strokes(N, H, L, H));
    t[0x2523 - 0x2500] = Some(BoxGlyph::Strokes(N, H, H, H));
    t[0x2524 - 0x2500] = Some(BoxGlyph::Strokes(L, N, L, L));
    t[0x2525 - 0x2500] = Some(BoxGlyph::Strokes(H, N, L, L));
    t[0x2526 - 0x2500] = Some(BoxGlyph::Strokes(L, N, H, L));
    t[0x2527 - 0x2500] = Some(BoxGlyph::Strokes(L, N, L, H));
    t[0x2528 - 0x2500] = Some(BoxGlyph::Strokes(L, N, H, H));
    t[0x2529 - 0x2500] = Some(BoxGlyph::Strokes(H, N, H, L));
    t[0x252A - 0x2500] = Some(BoxGlyph::Strokes(H, N, L, H));
    t[0x252B - 0x2500] = Some(BoxGlyph::Strokes(H, N, H, H));
    t[0x252C - 0x2500] = Some(BoxGlyph::Strokes(L, L, N, L));
    t[0x252D - 0x2500] = Some(BoxGlyph::Strokes(H, L, N, L));
    t[0x252E - 0x2500] = Some(BoxGlyph::Strokes(L, H, N, L));
    t[0x252F - 0x2500] = Some(BoxGlyph::Strokes(H, H, N, L));
    t[0x2530 - 0x2500] = Some(BoxGlyph::Strokes(L, L, N, H));
    t[0x2531 - 0x2500] = Some(BoxGlyph::Strokes(H, L, N, H));
    t[0x2532 - 0x2500] = Some(BoxGlyph::Strokes(L, H, N, H));
    t[0x2533 - 0x2500] = Some(BoxGlyph::Strokes(H, H, N, H));
    t[0x2534 - 0x2500] = Some(BoxGlyph::Strokes(L, L, L, N));
    t[0x2535 - 0x2500] = Some(BoxGlyph::Strokes(H, L, L, N));
    t[0x2536 - 0x2500] = Some(BoxGlyph::Strokes(L, H, L, N));
    t[0x2537 - 0x2500] = Some(BoxGlyph::Strokes(H, H, L, N));
    t[0x2538 - 0x2500] = Some(BoxGlyph::Strokes(L, L, H, N));
    t[0x2539 - 0x2500] = Some(BoxGlyph::Strokes(H, L, H, N));
    t[0x253A - 0x2500] = Some(BoxGlyph::Strokes(L, H, H, N));
    t[0x253B - 0x2500] = Some(BoxGlyph::Strokes(H, H, H, N));
    t[0x253C - 0x2500] = Some(BoxGlyph::Strokes(L, L, L, L));
    t[0x253D - 0x2500] = Some(BoxGlyph::Strokes(H, L, L, L));
    t[0x253E - 0x2500] = Some(BoxGlyph::Strokes(L, H, L, L));
    t[0x253F - 0x2500] = Some(BoxGlyph::Strokes(H, H, L, L));
    t[0x2540 - 0x2500] = Some(BoxGlyph::Strokes(L, L, H, L));
    t[0x2541 - 0x2500] = Some(BoxGlyph::Strokes(L, L, L, H));
    t[0x2542 - 0x2500] = Some(BoxGlyph::Strokes(L, L, H, H));
    t[0x2543 - 0x2500] = Some(BoxGlyph::Strokes(H, L, H, L));
    t[0x2544 - 0x2500] = Some(BoxGlyph::Strokes(L, H, H, L));
    t[0x2545 - 0x2500] = Some(BoxGlyph::Strokes(H, L, L, H));
    t[0x2546 - 0x2500] = Some(BoxGlyph::Strokes(L, H, L, H));
    t[0x2547 - 0x2500] = Some(BoxGlyph::Strokes(H, H, H, L));
    t[0x2548 - 0x2500] = Some(BoxGlyph::Strokes(H, H, L, H));
    t[0x2549 - 0x2500] = Some(BoxGlyph::Strokes(H, L, H, H));
    t[0x254A - 0x2500] = Some(BoxGlyph::Strokes(L, H, H, H));
    t[0x254B - 0x2500] = Some(BoxGlyph::Strokes(H, H, H, H));
    t[0x254C - 0x2500] = Some(BoxGlyph::DashedH(2, false));
    t[0x254D - 0x2500] = Some(BoxGlyph::DashedH(2, true));
    t[0x254E - 0x2500] = Some(BoxGlyph::DashedV(2, false));
    t[0x254F - 0x2500] = Some(BoxGlyph::DashedV(2, true));
    t[0x2550 - 0x2500] = Some(BoxGlyph::Strokes(D, D, N, N));
    t[0x2551 - 0x2500] = Some(BoxGlyph::Strokes(N, N, D, D));
    t[0x2552 - 0x2500] = Some(BoxGlyph::Strokes(N, D, N, L));
    t[0x2553 - 0x2500] = Some(BoxGlyph::Strokes(N, L, N, D));
    t[0x2554 - 0x2500] = Some(BoxGlyph::Strokes(N, D, N, D));
    t[0x2555 - 0x2500] = Some(BoxGlyph::Strokes(D, N, N, L));
    t[0x2556 - 0x2500] = Some(BoxGlyph::Strokes(L, N, N, D));
    t[0x2557 - 0x2500] = Some(BoxGlyph::Strokes(D, N, N, D));
    t[0x2558 - 0x2500] = Some(BoxGlyph::Strokes(N, D, L, N));
    t[0x2559 - 0x2500] = Some(BoxGlyph::Strokes(N, L, D, N));
    t[0x255A - 0x2500] = Some(BoxGlyph::Strokes(N, D, D, N));
    t[0x255B - 0x2500] = Some(BoxGlyph::Strokes(D, N, L, N));
    t[0x255C - 0x2500] = Some(BoxGlyph::Strokes(L, N, D, N));
    t[0x255D - 0x2500] = Some(BoxGlyph::Strokes(D, N, D, N));
    t[0x255E - 0x2500] = Some(BoxGlyph::Strokes(N, D, L, L));
    t[0x255F - 0x2500] = Some(BoxGlyph::Strokes(N, L, D, D));
    t[0x2560 - 0x2500] = Some(BoxGlyph::Strokes(N, D, D, D));
    t[0x2561 - 0x2500] = Some(BoxGlyph::Strokes(D, N, L, L));
    t[0x2562 - 0x2500] = Some(BoxGlyph::Strokes(L, N, D, D));
    t[0x2563 - 0x2500] = Some(BoxGlyph::Strokes(D, N, D, D));
    t[0x2564 - 0x2500] = Some(BoxGlyph::Strokes(D, D, N, L));
    t[0x2565 - 0x2500] = Some(BoxGlyph::Strokes(L, L, N, D));
    t[0x2566 - 0x2500] = Some(BoxGlyph::Strokes(D, D, N, D));
    t[0x2567 - 0x2500] = Some(BoxGlyph::Strokes(D, D, L, N));
    t[0x2568 - 0x2500] = Some(BoxGlyph::Strokes(L, L, D, N));
    t[0x2569 - 0x2500] = Some(BoxGlyph::Strokes(D, D, D, N));
    t[0x256A - 0x2500] = Some(BoxGlyph::Strokes(D, D, L, L));
    t[0x256B - 0x2500] = Some(BoxGlyph::Strokes(L, L, D, D));
    t[0x256C - 0x2500] = Some(BoxGlyph::Strokes(D, D, D, D));
    t[0x256D - 0x2500] = Some(BoxGlyph::Arc(true, true));
    t[0x256E - 0x2500] = Some(BoxGlyph::Arc(true, false));
    t[0x256F - 0x2500] = Some(BoxGlyph::Arc(false, false));
    t[0x2570 - 0x2500] = Some(BoxGlyph::Arc(false, true));
    t[0x2571 - 0x2500] = Some(BoxGlyph::Diagonal(false));
    t[0x2572 - 0x2500] = Some(BoxGlyph::Diagonal(true));
    t[0x2573 - 0x2500] = Some(BoxGlyph::DiagonalBoth);
    t[0x2574 - 0x2500] = Some(BoxGlyph::Strokes(L, N, N, N));
    t[0x2575 - 0x2500] = Some(BoxGlyph::Strokes(N, N, L, N));
    t[0x2576 - 0x2500] = Some(BoxGlyph::Strokes(N, L, N, N));
    t[0x2577 - 0x2500] = Some(BoxGlyph::Strokes(N, N, N, L));
    t[0x2578 - 0x2500] = Some(BoxGlyph::Strokes(H, N, N, N));
    t[0x2579 - 0x2500] = Some(BoxGlyph::Strokes(N, N, H, N));
    t[0x257A - 0x2500] = Some(BoxGlyph::Strokes(N, H, N, N));
    t[0x257B - 0x2500] = Some(BoxGlyph::Strokes(N, N, N, H));
    t[0x257C - 0x2500] = Some(BoxGlyph::Strokes(L, H, N, N));
    t[0x257D - 0x2500] = Some(BoxGlyph::Strokes(N, N, L, H));
    t[0x257E - 0x2500] = Some(BoxGlyph::Strokes(H, L, N, N));
    t[0x257F - 0x2500] = Some(BoxGlyph::Strokes(N, N, H, L));
    t
};

fn box_glyph(c: char) -> Option<BoxGlyph> {
    let idx = c as usize;
    if !(0x2500..0x2580).contains(&idx) {
        return None;
    }
    BOX_GLYPH_TABLE[idx - 0x2500]
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
    let specs = StrokeSpecs::new(x, y, width, ch);

    let Some(glyph) = box_glyph(c) else {
        return false;
    };

    match glyph {
        BoxGlyph::DashedH(count, heavy) => draw_dashed_h(
            vertices,
            &specs,
            count,
            if heavy { specs.heavy } else { specs.thick },
            color,
        ),
        BoxGlyph::DashedV(count, heavy) => draw_dashed_v(
            vertices,
            &specs,
            count,
            if heavy { specs.heavy } else { specs.thick },
            color,
        ),
        BoxGlyph::Arc(down, right) => draw_arc(vertices, &specs, down, right, color),
        BoxGlyph::Diagonal(forward) => draw_diagonal(vertices, &specs, forward, color),
        BoxGlyph::DiagonalBoth => {
            draw_diagonal(vertices, &specs, false, color);
            draw_diagonal(vertices, &specs, true, color);
        }
        BoxGlyph::Strokes(left, right, up, down) => {
            // Straight full-width / full-height lines: the arm spans the entire cell.
            if left == right && up == Stroke::None && down == Stroke::None {
                return push_straight_h(vertices, &specs, left, color);
            }
            if up == down && left == Stroke::None && right == Stroke::None {
                return push_straight_v(vertices, &specs, up, color);
            }

            // Generic junction: four arms clipped at the crossing so bands never overlap.
            let max_h = specs.thickness(left).max(specs.thickness(right));
            let max_v = specs.thickness(up).max(specs.thickness(down));
            let center_half_x = (max_v / 2.0).max(specs.thick / 2.0);
            let center_half_y = (max_h / 2.0).max(specs.thick / 2.0);

            push_arm(
                vertices,
                &specs,
                left,
                ArmAxis::Horizontal,
                x,
                specs.mid_x + center_half_x,
                color,
            );
            push_arm(
                vertices,
                &specs,
                right,
                ArmAxis::Horizontal,
                specs.mid_x - center_half_x,
                specs.x_right,
                color,
            );
            push_arm(
                vertices,
                &specs,
                up,
                ArmAxis::Vertical,
                y,
                specs.mid_y + center_half_y,
                color,
            );
            push_arm(
                vertices,
                &specs,
                down,
                ArmAxis::Vertical,
                specs.mid_y - center_half_y,
                specs.y_bot,
                color,
            );
        }
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
    if !is_procedural_glyph(c) {
        return false;
    }
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

    #[test]
    fn test_junction_arm_quad_counts() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // ┼ light cross: four arms, one quad each
        assert!(render_procedural_glyph(
            &mut vertices,
            '┼',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 4 * 48);

        vertices.clear();
        // ┣ heavy tee: three heavy arms
        assert!(render_procedural_glyph(
            &mut vertices,
            '┣',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 3 * 48);

        vertices.clear();
        // ╔ double corner: right and down arms, two quads each
        assert!(render_procedural_glyph(
            &mut vertices,
            '╔',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 4 * 48);

        vertices.clear();
        // ╬ double cross: four arms, two quads each
        assert!(render_procedural_glyph(
            &mut vertices,
            '╬',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 8 * 48);
    }

    #[test]
    fn test_half_ray_clips_at_center() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // ╴ left half ray: single light arm clipped at the crossing center.
        // thick = round(20 * 0.08) = 2; mid_x = floor(10 / 2) = 5; arm ends at 5 + 2/2 = 6.
        assert!(render_procedural_glyph(
            &mut vertices,
            '╴',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 48);
        assert_eq!(vertices[0], 0.0);
        assert_eq!(vertices[8], 6.0);
    }

    #[test]
    fn test_dashed_and_diagonal_quad_counts() {
        let mut vertices = Vec::new();
        let color = [1.0, 1.0, 1.0, 1.0];
        // ┄ 3-dash horizontal line: three dashes
        assert!(render_procedural_glyph(
            &mut vertices,
            '┄',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 3 * 48);

        vertices.clear();
        // ╱ diagonal: min(10, 20) = 10 steps (>= 4)
        assert!(render_procedural_glyph(
            &mut vertices,
            '╱',
            0.0,
            0.0,
            10.0,
            20.0,
            color
        ));
        assert_eq!(vertices.len(), 10 * 48);
    }
}
