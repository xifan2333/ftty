# ADR 0006: Extract Grid Reflow and Line Rewrapping into Dedicated Module

## Context
`src/grid.rs` (2450 lines) was the largest file in the codebase. Over 540 lines implemented `reflow_primary_screen`, `LogicalLine`, `ReflowCell`, and row shrinking—a complex, self-contained rewrapping algorithm that merges wrapped lines, rewraps them across new column widths, avoids splitting wide characters, preserves semantic prompt markers, and re-maps cursor coordinates. These algorithmic calculations cluttered the core 2D character matrix operations and BCE scroll methods.

## Decision
We extract the logical line rewrapping and resizing algorithm into `src/grid/reflow.rs`. The `Grid::resize` method delegates width changes on the primary screen to `reflow::reflow_primary_screen(self, new_cols, new_rows)`, while retaining `shrink_rows` in `reflow` for row trimming.

## Consequences
- `src/grid.rs` sheds 540+ lines of algorithmic rewrapping code, refocusing on 2D matrix storage, cursor movement, and cell mutations.
- The reflow algorithm is isolated behind a clean seam, enabling focused verification of column wrapping, wide-character safety, and prompt-marker preservation.
- `src/grid.rs` maintains full API backwards compatibility via `grid::resize`.
