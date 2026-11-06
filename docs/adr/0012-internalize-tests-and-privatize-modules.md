# ADR 0012: Internalize Test Suites and Privatize Crate Modules

## Context

While ADR 0011 established an honest module layout by turning fake shim namespaces into real files, the test suites remained in `tests/` as separate compilation units. Because external integration test binaries can only access `pub` items, dozens of internal implementation details—such as `build_vertices`, `wcswidth`, `load_font_bytes`, `find_url_at_col`, and internal pipeline accessors—were forced to be marked `pub #[doc(hidden)]`. 

Furthermore, `src/lib.rs` had accumulated 77 re-exports at the crate root, 68 of which were redundant aliases to items already reachable via their respective modules. The integration tests' need for visibility was artificially driving the crate's public API surface and leaking internal abstractions.

## Decision

We closed the architectural loop by internalizing the test suite and sealing the crate boundary:

1. **Internalize all 14 test suites**: Each `tests/<module>.rs` file was moved into a `#[cfg(test)] mod tests` block at the bottom of its respective module, and the `tests/` directory was deleted. Tests now sit directly beside the implementation they verify and can exercise private types and methods naturally.
2. **Privatize all module declarations in `lib.rs`**: All 16 module declarations in `src/lib.rs` (`mod color;`, `mod config;`, `mod render;`, etc.) are now private.
3. **Collapse the crate prelude**: The public surface in `src/lib.rs` was trimmed to only the 8 operational types and entrypoints required by `src/main.rs` (`AppState`, `Config`, `Pty`, `Terminal`, `TerminalSession`, `FontManager`, `run_event_loop_with_connection`, and `trim_memory`).
4. **Gate test-only helpers behind `#[cfg(test)]`**: Functions and types that exist solely to support testing (such as `build_vertices`, `wcswidth`, `kitty_response`, and `KittyParser` scratch buffers) are gated with `#[cfg(test)]`, eliminating dead code from production release binaries.
5. **Remove obsolete `#[doc(hidden)]` markers**: With internal items no longer exposed across the crate boundary, 69 `#[doc(hidden)]` annotations across the codebase were removed as dead weight.
6. **Purge dead code**: Unused legacy functions like `run_event_loop` and duplicated color defaults (`DEFAULT_FG`/`DEFAULT_BG`) were eliminated.

## Consequences

- The public API surface is minimal, clear, and decoupled from internal refactoring.
- No test scaffolding or internal helper leaks into library consumers.
- All 344 unit tests run in less than 0.7s without compilation unit overhead.
- Production binaries are free of test-only staging buffers and helper logic.
