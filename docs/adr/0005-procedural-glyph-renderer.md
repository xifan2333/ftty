# ADR 0005: Extract Procedural Box Drawing and Block Elements to Procedural Module

## Context
`src/render.rs` contained over 1070 lines of pure mathematical box-drawing, corner arc approximation, shaded block generator algorithms (U+2500..U+257F and U+2580..U+259F), and 200 lines of inline geometry tests. These algorithms operate entirely in vertex space (`&mut Vec<f32>`) and have zero dependencies on OpenGL, EGL, or Wayland, yet cluttered the primary GPU rendering engine and obscured high-level frame presentation pipelines.

## Decision
We extract all Box Drawing, Block Elements, and segmented arc calculations into a dedicated deep module `src/render/procedural.rs`. It exposes a narrow two-function interface (`is_procedural_glyph` and `render_procedural_glyph`), while re-exporting `Stroke`, `render_block_element`, and `render_box_drawing` under `render::procedural` and `render::box_drawing` for compatibility.

## Consequences
- `src/render.rs` immediately sheds 1070+ lines of non-GL code, refocusing on GPU state management and frame rendering.
- Procedural glyph math and line joint continuity tests reside directly within `procedural.rs` under test-locality principles.
- Future procedural glyph extensions (e.g. Braille patterns, Powerline glyphs) can be developed in isolation without modifying the main GPU rendering engine.
