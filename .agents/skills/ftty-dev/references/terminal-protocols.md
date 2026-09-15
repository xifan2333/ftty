# Terminal Protocol Standards & Implementation Guide

This reference documents the terminal escape sequences and protocols supported by `ftty`, their data structures, and edge-case handling guidelines.

---

## 1. Supported Protocols Matrix

| Protocol | Category | Sequence / Interface | Handled In |
| :--- | :--- | :--- | :--- |
| **Kitty Graphics** | Core | APC `\x1b_G ... \x1b\` | `kitty.rs`, `render.rs`, `grid.rs` |
| **Kitty Keyboard** | Keyboard | `CSI ? u`, `CSI =/>/< flags u` | `input.rs`, `parser.rs`, `event_loop.rs` |
| **text-input-v3** | Input | Wayland `zwp_text_input_v3` | `wayland.rs`, `ime.rs`, `render.rs` |
| **SGR Mouse** | Pointer | DECSET `1006` (`\x1b[<btn;col;rowM/m`) | `mouse.rs`, `parser.rs`, `event_loop.rs` |
| **Bracketed Paste** | Input | DECSET `2004` (`\x1b[200~` ... `\x1b[201~`) | `parser.rs`, `event_loop.rs` |
| **Focus Reporting** | Window | DECSET `1004` (`\x1b[I` / `\x1b[O`) | `parser.rs`, `event_loop.rs` |
| **Synchronized Output** | Render | DECSET `2026` (`\x1b[?2026h` / `l`) | `parser.rs`, `event_loop.rs` |
| **Styled Underlines** | Visual | SGR `4:1`..`4:5`, SGR `58;2` / `58;5`, `59` | `grid.rs`, `parser.rs`, `render.rs` |
| **OSC 8 Hyperlinks** | Semantic | `\x1b]8;params;url\x1b\` | `grid.rs`, `parser.rs`, `event_loop.rs` |
| **OSC 52 Clipboard** | IPC | `\x1b]52;Pc;Pd\x1b\` | `parser.rs`, `event_loop.rs` |
| **OSC 7 Working Dir** | Semantic | `\x1b]7;file://hostname/path\x1b\` | `parser.rs` |
| **OSC 133 Shell Int.** | Semantic | `\x1b]133;[A|B|C|D]\x1b\` | `parser.rs` |
| **Mode 2031 Theme** | Visual | DECSET `2031` (`\x1b[?2031;1/2$y`) | `parser.rs` |
| **Mode 2048 Geometry** | Window | DECSET `2048` (`\x1b[4;H;Wt`) | `parser.rs` |
| **OSC 9;4 Progress** | Semantic | `\x1b]9;4;state;progress\x1b\` | `parser.rs` |

---

## 2. Key Protocol Implementation Guidelines

### 1. Kitty Graphics Protocol
- **Specification**: Encapsulated in APC `\x1b_G<control>;<payload>\x1b\`.
- **Transmission Mediums**:
  - `m=0` (Direct inline Base64)
  - `m=1` / `m=2` (File / Temp file)
  - `m=3` (POSIX Shared Memory `/dev/shm`, zero-copy via `shm_open`)
- **Placement & Placeholder**:
  - Anchored to grid coordinates; supports Unicode placeholder `U=1` with diacritic row/column offsets.
  - Image placements must be tracked across vertical screen shrinkage and alternate screen transitions without corruption.

### 2. Kitty Keyboard Protocol (CSI u)
- **Supported Flags**:
  - `1`: `DISAMBIGUATE_ESCAPE_CODES` (Escape, Tab, Enter, Backspace reported via `CSI <key>;<mod>u`)
  - `2`: `REPORT_EVENT_TYPES` (Key release and repeat events reported)
- **Flag Masking**:
  - Always mask incoming raw flags in `u16` space before narrowing to `u8`:
    `let flags = ((raw & SUPPORTED_KITTY_FLAGS) as u8);`
  - Unrecognized high bits must not wrap into supported low flags.
- **Stack Behavior**:
  - Bounded at 64 entries.
  - An emptying pop request (when stack is exhausted) **must reset all flags to 0**.
- **Event Loop Sync**:
  - In `event_loop.rs`, synchronize `KeyboardHandler::kitty_flags` directly from `Terminal::kitty_keyboard_flags` on every PTY read cycle to guarantee strict real-time state alignment.

### 3. Synchronized Output (Mode 2026)
- **Mechanism**:
  - `\x1b[?2026h` disables frame presentation to batch heavy visual updates without tearing.
  - `\x1b[?2026l` reenables rendering and triggers an immediate frame commit.
- **Freeze Prevention Guardrail**:
  - When mode 2026 is active, `calloop.dispatch` must use a bounded timeout (`50ms`) rather than blocking indefinitely (`None`).
  - An application that hangs or dies without sending `?2026l` must be timed out after `150ms`.
  - Every activation increment must track a `sync_output_gen` generation counter so re-enablement resets the timer.

### 4. SGR Attributes & Styled Underlines
- **Structured Subparameters**:
  - `vte::Params` represents parameters as `&[&[u16]]`.
  - Do NOT flatten parameters before checking underline styles:
    - Colon subparameters (`4:3`) denote underline styles (3 = undercurl).
    - Semicolon parameters (`4;1`) denote independent SGR attributes (4 = underline, 1 = bold). Both must be preserved!
- **Rendering Geometry**:
  - Curly underlines (undercurl) are rendered using piecewise horizontal quad segments approximating a sine wave.
  - SGR 58/59 underline color takes precedence over text foreground when set.

### 5. Wayland Clipboard (OSC 52 & Data Device)
- **Seat Serial Requirement**:
  - Wayland `set_selection` rejects requests if the seat serial is invalid or zero.
  - Always store `state.last_serial` on `wl_keyboard::Event::Enter`, `Key`, and `Modifiers`, in addition to pointer clicks.
- **Clearing Selection**:
  - An empty OSC 52 payload represents an explicit clear.
  - Must call `device.set_selection(None, last_serial)`, NOT advertise an empty text buffer.
- **Non-blocking Write Fallback**:
  - Short pastes (<= 4KB) write synchronously with a 100ms readiness timeout.
  - Large pastes (> 4KB) must write from a background thread to prevent freezing the Wayland event loop.

### 6. Hyperlinks (OSC 8)
- **ID Resolution**:
  - 1-based index into an interned URL pool capped at 1,024 entries.
- **Reset Cleanup (RIS)**:
  - On full terminal reset (`\x1bc`), the URL pool must be cleared AND all cells across visible lines, scrollback, and alternate screen (`alt_lines`) must have `hyperlink_id` reset to `None` to prevent dangling references.
