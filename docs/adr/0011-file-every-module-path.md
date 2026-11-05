# ADR 0011: File Every Module Path Instead of Re-Exporting It

## Context

ADR 0003, 0009, and 0010 each split a large file and left behind a `pub mod X { pub use super::{...}; }` block for the sections they had not moved yet. By the time the split stopped, twelve files carried nineteen of these blocks:

```rust
// src/render.rs, before
pub mod text {
    pub use super::{
        KITTY_PLACEHOLDER, RenderContext, SELECTION_BG, build_dynamic_overlays,
        build_row_backgrounds, build_row_foregrounds, build_vertices, cell_colors, cursor_cell,
        prepare_atlas, rgba,
    };
}
```

The blocks were not cosmetic. Tests and callers addressed their items through them —
roughly fifty call sites used `render::text::*`, `input::ime::*`, `kitty::model::*` and
friends — so the shims named the intended module layout while the code stayed inline in
`render.rs`, `input.rs`, and `kitty.rs`. Half the crate's modules were real files
(`input/pointer.rs`, `kitty/store.rs`, `render/pipeline.rs`); the other half were aliases
in a 1800-line file. Fixing a bug in `input::ime` therefore meant opening a file whose
name said `input`, scrolling past the keyboard handler, and editing code under a shim.

Five shims had no callers at all: `event_loop::{geometry,actions,kitty}`, `grid::resize`,
and `config::include`. Nothing removed them because nothing depended on them.

Three properties made this expensive rather than untidy:

- **Two paths per item.** `render::build_vertices` and `render::text::build_vertices` both
  resolved. An agent adding a vertex helper finds one path and updates one list.
- **Hand-synced lists.** Every shim restated an item list that had to be edited whenever
  the inline module gained an item. The empty `pub mod actions {}` is what that looks like
  after nobody has needed an item for a while.
- **Duplicated constants.** `KITTY_PLACEHOLDER` was defined four times — in `width`,
  `kitty::store`, `grid`, and `render` — because each split copied the constant into the
  file it was cutting.

## Decision

File every shim. The section a shim re-exported moves into a real module file, the shim is
deleted, and the parent module re-exports only what has no better home.

The rule applied throughout:

> A module is `pub` exactly when callers or tests address it by name. Otherwise it is
> private, and its public items are re-exported once at the owning level.

That rule decides each case concretely:

| Module | Visibility | Why |
| --- | --- | --- |
| `render::text`, `render::image`, `render::shader`, `render::egl` | `pub` | the integration suite addresses them by name |
| `render::colors`, `render::pipeline`, `render::egl::EglContext` | private + re-export | no caller says `colors::`; `render::ColorScheme` is the one path |
| `grid::diacritics` | `pub` | tests call `grid::diacritics::diacritic_to_index` |
| `grid::cell`, `grid::cursor`, `grid::row` | private + re-export | `grid::Cell` is the one path |
| `input::ime`, `input::mouse`, `input::selection` | `pub` | callers and tests address them by name |
| `input::keys`, `input::keyboard` | private + re-export | `input::KeyAction` is the one path |
| `kitty::model`, `kitty::command`, `kitty::payload` | `pub` | tests address them by name |
| `kitty::stream` | private + re-export | `kitty::KittyParser` is the one path |
| `font::face`, `font::fallback`, `color::palette` | private + re-export | no caller names the file |

Content moved with the names:

- `render.rs` (1825) → `render/{mod 346, text 626, image 488, mod-kept Renderer, shader 141, egl 154, colors 74}`.
  `render::box_drawing`, a `pub use super::procedural::*` alias, is gone; `render::procedural`
  is the path.
- `grid.rs` (2078) → `grid/{mod 1422, row 209, cell 102, diacritics 321, cursor 31}`.
- `input.rs` (1392) → `input/{mod 23, keys 172, keyboard 584, ime 175, mouse 206, selection 239}`.
- `kitty.rs` (1338) → `kitty/{mod 15, model 221, command 327, payload 329, stream 444}`.
- `font/mod.rs` (1179) → `font/{mod 412, face 537, fallback 236}`.
- `color.rs` (189) → `color/{mod 145, palette 47}`.

`KITTY_PLACEHOLDER` now has one definition, in `width` — the leaf module that already
needed it to report a placeholder cell's width. `kitty`, `grid`, and `render` import it.

## Consequences

- Every item has exactly one path, and that path matches the file it is written in.
- Adding an item to a module can no longer desynchronise a re-export list, because the
  list that had to be synchronised is gone.
- The five dead shims are deleted, along with `wayland::seat`: URL and mouse-button
  helpers had been reachable as `wayland::seat::find_url_in_grid` and
  `wayland::find_url_in_grid`, neither of which names the module that defines them.
- The change is a pure move. `cargo test` passes 344 tests unchanged except for six import
  lines that pointed at a deleted path.
- Two mixed files remain: `grid/mod.rs` at 1422 lines (the `Grid` state machine and its
  resize seam) and `parser.rs` at 1378 (the VT state machine). They were left alone
  deliberately — both are single cohesive state machines with no second concern inside.
