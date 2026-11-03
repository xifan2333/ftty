# ADR 0003: Encapsulate Terminal Emulation and PTY Orchestration in TerminalSession

## Context
The Calloop event loop (`src/event_loop.rs`) and `AppState` previously coordinated PTY reading, Kitty APC chunk demuxing, VT parser byte advancement, PTY response writebacks, OSC 52 clipboard syncing, title/palette dirty flags, and Mode 2026 synchronized output timeout timers directly. This scattered core terminal emulation mechanics across the platform event loop and forced `AppState` to retain internal parser and timer states.

## Decision
We introduce `TerminalSession` under `src/session.rs` as a deep module that unifies `Terminal`, `Pty`, `VtParser`, `KittyParser`, and synchronized output timers. The event loop delegates incoming PTY byte buffers and user keystrokes directly to `TerminalSession`, which handles internal demuxing, automatic response writebacks, and emits a declarative `SessionOutcome` struct notifying the platform layer of title, clipboard, or redraw updates.

## Consequences
- `AppState` eliminates internal parser instances, PTY file descriptors, and synchronization timers, reducing to a clean platform coordinator.
- Core terminal emulation and escape protocol responses in `Terminal` can be verified in pure memory, and `TerminalSession` orchestrates full PTY I/O and parser demuxing without initializing Wayland event loops.
- `event_loop.rs` transforms into a concise, low-maintenance event pump.
