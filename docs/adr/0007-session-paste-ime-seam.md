# ADR 0007: Move Paste and IME Encoding Behind the Session Seam

## Context
The Wayland adapter accumulated terminal protocol knowledge that belongs to the emulation core. `paste_clipboard` in `src/wayland.rs` encoded bracketed-paste markers (`\x1b[200~` / `\x1b[201~`) in two separate branches, the `zwp_text_input_v3` `done` handler emitted IME erasure escapes (`\x08`, `\x1b[3~`) inline, and two private poll-readiness write loops (1000 ms readiness slice) duplicated `TerminalSession::write_input_bytes` (20 ms slice). The same PTY write path therefore had three implementations and two divergent timeout policies, so a truncation or stall bug could be fixed in one place while remaining live in another.

## Decision
We move that knowledge behind `TerminalSession`. `session.rs` now owns a single `write_all_to_pty(fd, bytes, budget)` routine used by every producer, plus `encode_paste` / `paste` / `paste_from_fd` / `ime_commit`. IME erasure and commit encoding lives in `input::ime::encode_ime_edits`, next to the rest of the input-side protocol. The Wayland adapter reduces to passing raw bytes and file descriptors.

## Consequences
- The Wayland adapter contains no escape-sequence literals and no write loops; `paste_clipboard` shrinks from ~113 lines to ~20.
- Bracketed-paste wrapping and IME erasure encoding are asserted in pure in-memory tests through `TerminalSession` and `encode_ime_edits`.
- Timeout and truncation behaviour has exactly one implementation, keyed by an explicit budget per producer (100 ms interactive, 5 s paste).
