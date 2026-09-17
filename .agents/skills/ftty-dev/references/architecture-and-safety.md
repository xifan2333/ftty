# ftty Architecture & Rust Safety Standards

`ftty` is an ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol support, written in modern Rust.

---

## 1. Supreme Architecture Principles (最高原则)

1. **Unix Philosophy in Terminal Emulation**:
   - **Do One Thing Well**: `ftty` is strictly a PTY terminal surface. No built-in tabs, no split panes, no multiplexing. Tiling belongs to the window manager (`xwm` / `xrwm`), session multiplexing belongs to `herdr` or `tmux`.
   - **Text as the Universal Interface**: Configuration is simple declarative TOML with recursive `include` support and hot-reload.
2. **Minimalist & Bloat-Free**:
   - **No Heavy GUI Frameworks**: No GPUI, no GTK, no Qt. Direct native Wayland client (`wayland-client` / EGL / `glow`).
   - **Resource Constrained**: Stripped release binary < 3 MB, idle memory < 15 MB, cold launch < 3 ms.
   - **Cognitive Maintainability**: Keep code small and readable. Mechanism over policy.
3. **Core Protocol First-Class Citizens**:
   - **Native Kitty Graphics Protocol**: APC `\x1b_G` parser, direct/shm memory mapping for zero-copy image transfers (for `yazi`, `image.nvim`).
   - **Fcitx5 IME First-Class Citizen**: Wayland `text-input-v3` integration for precise candidate box cursor tracking.
   - **Theme Hot-Reloading**: Signal-driven (`SIGUSR1`) dynamic palette switching with zero flicker.

---

## 2. Module Layout & Responsibilities

| Module | Purpose | Key Responsibilities |
| :--- | :--- | :--- |
| `src/event_loop.rs` | Main event loop & orchestration | Calloop event loop, Wayland dispatch, PTY master reading, paste handling, clipboard synchronization. |
| `src/wayland.rs` | Wayland protocol wrappers | Surface creation, XDG toplevel negotiation, clipboard `wl_data_device`, IME `zwp_text_input_v3`. |
| `src/render.rs` | OpenGL rendering engine | EGL context initialization, GL shaders, vertex batching (`push_quad`), styled underlines, image placements. |
| `src/parser.rs` | VT/ANSI escape sequence parser | VTE state machine, SGR attributes, private modes (DECSET/DECRST), OSC commands (8, 52, 7, 133, 9;4). |
| `src/grid.rs` | Cell grid & scrollback buffer | Terminal screen matrix, alternate screen buffer, cell flags, wide chars, Kitty diacritic placeholders, scrollback ring buffer. |
| `src/kitty.rs` | Kitty Graphics Protocol parser | APC `\x1b_G` sequence decoding, PNG/RGB/RGBA loading, shared memory (`shm_open`) zero-copy, placement tracking. |
| `src/input.rs` | Keyboard input & translation | XKB keymap handling, modifier tracking, functional key escape codes, Kitty keyboard protocol progressive enhancement. |
| `src/mouse.rs` | Mouse tracking & encoding | DECSET 1000/1002/1003 modes, SGR / UTF-8 / X10 mouse report encoders. |
| `src/ime.rs` | Input method state | Preedit string storage, IME cursor rectangle calculation, atomic batch commits on `done`. |
| `src/font.rs` | Font discovery & glyph atlas | Fontconfig resolution, system fallback face chaining (CJK/emoji), fontdue rasterization, dynamic shelf packing atlas. |
| `src/color.rs` | Colors & 256-color palette | RGB channels, hex parsing, 256-color palette definition, dynamic theme luminance calculation. |
| `src/pty.rs` | Pseudo-terminal master/slave | Unix PTY allocation (`posix_openpt`, `forkpty`), window size ioctls, non-blocking I/O. |
| `src/selection.rs` | Mouse text selection | Coordinate normalization, word boundary detection, plain-text extraction. |
| `src/config.rs` | Declarative TOML configuration | Config parsing, circular `include` detection, dynamic theme overrides. |

---

## 3. Strict Rust Safety & Lints Policy

The codebase enforces strict compile-time checks configured in `Cargo.toml`:

```toml
[lints.rust]
unsafe_code = "deny"

[lints.clippy]
unwrap_used = "deny"
expect_used = "deny"
undocumented_unsafe_blocks = "deny"
```

### Safety Rules:
1. **Unsafe Isolation**:
   - `unsafe` blocks are strictly forbidden in all modules except audited FFI boundaries (`src/render.rs` for OpenGL FFI, `src/pty.rs` for POSIX PTY FFI).
   - Every `unsafe` block must be accompanied by an audited safety justification comment (`// SAFETY: ...`).
2. **No Unwraps / Panics in Production**:
   - `unwrap()` and `expect()` are denied by Clippy across production code (explicitly permitted only in `#[cfg(test)]` modules via `src/lib.rs`).
   - All production errors must be handled gracefully: propagate via `?`, fallback to safe defaults, or log and recover.
3. **Non-blocking I/O Guardrails**:
   - The PTY master file descriptor is strictly non-blocking (`O_NONBLOCK`).
   - Never perform unbounded blocking writes on the event-loop thread. Use bounded poll readiness (`PollFd` with <= 100ms timeout) for control sequences and small fallback pastes (<= 4KB). Wayland offers and large fallback pastes (> 4KB) must write from background threads.
4. **Resource Bounds**:
   - Any cache or pool (hyperlink storage, Kitty keyboard stack, glyph atlas, image placements) **must** have a defined maximum capacity with deterministic eviction (FIFO or LRU) to prevent unbounded memory growth.
