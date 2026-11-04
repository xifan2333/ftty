# ADR 0004: Encapsulate Vertex Geometry and Row Caching in RenderPipeline

## Context
`src/render.rs` (3060 lines) exposed 10+ vertex and atlas helpers (`build_vertices`, `build_row_backgrounds`, `build_row_foregrounds`, `prepare_atlas`, `cell_colors`, `cursor_cell`, etc.) as public functions solely so unit tests could execute without an EGL context. `tests/render.rs` bypassed the `Renderer` interface entirely and asserted on raw `Vec<f32>` vertex layouts, meaning any vertex-format change invalidated the whole test suite while high-risk logic (incremental row cache invalidation, VBO growth) remained untested through a stable interface.

## Decision
We introduce `RenderPipeline` under `src/render/pipeline.rs` as a deep module that owns incremental per-row vertex caches (`row_bg`, `row_fg`, `row_valid`), geometry generation, dynamic overlay building, and atlas preparation, exposing a single `prepare_frame(...) -> RenderBatch` interface. The EGL/GL `Renderer` consumes `RenderBatch` for GPU upload and drawing. The vertex-format helpers become private implementation details of the pipeline; tests assert high-level batch semantics (primitive counts, layer ordering, dirty-row regeneration) rather than raw float arrays.

## Consequences
- The interface becomes the test surface: unit tests cross the same seam as production rendering without needing Wayland or EGL.
- Vertex layout optimizations (quantized packing, VBO growth) can evolve without breaking the test suite.
- `Renderer`'s diff responsibility shrinks to GPU resource management and draw dispatch.