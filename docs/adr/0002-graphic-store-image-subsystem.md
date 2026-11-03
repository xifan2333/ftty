# ADR 0002: Encapsulate Kitty Graphics Protocol Lifecycle in GraphicStore

## Context
Kitty graphics protocol handling was previously split across five modules: `src/kitty.rs` parsed APC chunks; `src/event_loop.rs` computed cell dimensions and cursor advances in `handle_kitty_event`; `src/grid.rs` managed image caches, LRU memory budgeting, and placement lists; and `src/render.rs` traversed `grid.images` to upload textures and mutated CPU buffers (`img.rgba = None`) during frame rendering. This fragmented the image lifecycle, burdened `Grid` with image accounting, and violated the principle of read-only rendering.

## Decision
We introduce `GraphicStore` under `src/kitty/store.rs` as a deep module owned by `Terminal`. It encapsulates image decoding caches, placement spatial indexing, LRU memory eviction, and placement shifting on scroll. `Grid` is stripped of image fields and restored to a pure character grid. `Renderer` queries `GraphicStore` for GPU draw batches and texture upload slices without direct write mutation of image memory.

## Consequences
- `Grid` eliminates 400+ lines of Kitty graphics storage and focus solely on cell matrices and scrollback lines.
- `Renderer` operates as a pure read-only consumer, eliminating in-render state mutation.
- Kitty graphics event handling, placement math, and memory budgeting are testable in isolation without initializing Wayland or EGL rendering pipelines.
