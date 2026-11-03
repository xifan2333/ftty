# ADR 0001: Encapsulate Pointer Interaction and Terminal Mouse Semantics in PointerController

## Context
Wayland pointer event dispatch (`src/wayland.rs`) previously accessed over 15 fine-grained fields on `AppState` directly and contained terminal-specific interaction logic, including SGR 1006 mouse protocol reporting, multi-click word/line selection gestures, drag tracking, and hyperlink hit-testing. This tightly coupled the platform windowing layer to terminal emulation semantics and made pointer interaction logic impossible to test without mocking Wayland compositors.

## Decision
We introduce `PointerController` under `src/input/pointer.rs` as a deep module owning all pointer state, text selection lifecycles, and gesture timing. The platform layer (`src/wayland.rs`) passes transient layout and terminal metrics via `PointerContext` into explicit event handlers (`handle_motion`, `handle_button`, etc.) and receives a declarative, platform-agnostic `PointerOutcome` struct.

## Consequences
- `AppState` eliminates 15 loose pointer and selection fields in favor of a single `PointerController` instance.
- Pointer gestures, drag selection, and SGR 1006 encoding become testable in pure unit tests without Wayland dependencies.
- URL spawning (`xdg-open`) is represented as an explicit outcome field (`open_url`) rather than an implicit background thread spawn inside event callbacks.
