---
name: ftty-dev
description: Complete development, architecture, terminal protocol, code health check, and Issue+PR lifecycle guide for ftty. Use when developing or refactoring Rust terminal emulator code, modifying Wayland client or OpenGL rendering pipelines, adding/extending terminal escape sequences (Kitty Graphics, Kitty Keyboard, SGR mouse, OSC commands), tuning font rasterization or fallback faces, integrating Fcitx5 IME (text-input-v3), running pre-PR clean-up inspections (代码体检), or triaging automated review bot feedback. Trigger also on Chinese requests such as 开发ftty, 编写终端代码, 终端协议扩展, Wayland客户端调试, 字体回退与字形排版, 渲染器优化, 提PR, 运行质量门禁, 代码体检, 机器人审查修复.
---

# ftty: Unified Developer & Engineering Guide

This skill is the master operational manual for developing, maintaining, and testing `ftty` (Fan TTY) — an ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol support, written in Rust.

---

## 1. Quick Repository Routing & Upstream References

Verify component scope before making changes:

| Target | Description | Path / Reference |
| :--- | :--- | :--- |
| **`ftty` (Core)** | Wayland terminal client, PTY, OpenGL renderer, VT parser | `.` / [xifan2333/ftty](https://github.com/xifan2333/ftty) |
| **`wayland-client`** (Upstream) | Native Wayland protocol client & XDG shell | [Smithay/wayland-rs](https://github.com/Smithay/wayland-rs) |
| **`wayland-protocols`** (Upstream) | `zwp_text_input_v3` (IME), `xdg_shell`, `wl_data_device` | [wayland-protocols](https://gitlab.freedesktop.org/wayland/wayland-protocols) |
| **`vte`** (Upstream) | DEC / ANSI terminal state machine parser | [alacritty/vte](https://github.com/alacritty/vte) |
| **`xkbcommon`** (Upstream) | XKB keyboard layouts, keysyms, compose processing | [xkbcommon-rs](https://github.com/Smithay/xkbcommon-rs) |
| **`fontdue`** (Upstream) | High-speed TrueType/OpenType font rasterizer | [slimsag/fontdue](https://github.com/slimsag/fontdue) |
| **`glow`** (Upstream) | Cross-platform OpenGL bindings (EGL backend) | [grovesNL/glow](https://github.com/grovesNL/glow) |
| **Kitty Protocols** (Upstream) | Kitty Graphics Protocol (`\x1b_G`) & Keyboard Protocol (`CSI u`) | [Kitty Protocol Docs](https://sw.kovidgoyal.net/kitty/protocol-extensions/) |

---

## 2. Supreme Architecture Principles (最高原则)

1. **Unix Philosophy in Terminal Emulation**:
   - **Do One Thing Well**: `ftty` is strictly a PTY terminal surface. No built-in tabs, no split panes, no multiplexing. Tiling belongs to the window manager (`xwm` / `xrwm`), session multiplexing belongs to `herdr` or `tmux`.
   - **Text as the Universal Interface**: Configuration is simple declarative TOML with recursive `include` support and hot-reloading (`SIGUSR1`).
2. **Minimalist & Bloat-Free**:
   - **No Heavy GUI Frameworks**: No GPUI, no GTK, no Qt. Direct native Wayland client (`wayland-client` / EGL / `glow`).
   - **Resource Constrained**: Stripped release binary < 3 MB, idle memory < 15 MB, cold launch < 3 ms.
   - **Cognitive Maintainability**: Keep code small and readable. Mechanism over policy.
3. **Core Protocol First-Class Citizens**:
   - **Native Kitty Graphics Protocol**: APC `\x1b_G` parser, direct/shm memory mapping for zero-copy image transfers (for `yazi`, `image.nvim`).
   - **Fcitx5 IME First-Class Citizen**: Wayland `text-input-v3` integration for precise candidate box cursor tracking and preedit text rendering.
   - **Modern Keyboard & Protocol Standard**: Native Kitty Keyboard Protocol progressive enhancement (`CSI u`), Synchronized Output (Mode 2026), OSC 8 hyperlinks, OSC 52 clipboard, and styled undercurl.
4. **Strict Rust Safety**:
   - `unsafe_code = "deny"` across the entire crate. The only exceptions are audited FFI boundaries (`src/render/` for OpenGL/EGL and `src/pty.rs` for POSIX PTY).
   - Clippy denies `unwrap_used` and `expect_used` in production code (explicitly allowed in `#[cfg(test)]` modules).
   - Undocumented unsafe blocks are denied everywhere.

---

## 3. Universal Code Quality Gate (`hk` / `mise`)

Always run quality verification before committing:

```bash
# 1. Preview which linter/formatter steps match modified files
mise run check:plan

# 2. Check changed files safely (rustfmt + cargo clippy) (fast, < 2s)
mise run check:changed

# 3. Automatically fix formatting (rustfmt edition 2024)
mise run fix

# 4. Run targeted unit test for modified module or owning subsystem (verify tests run > 0, fast, < 1s)
cargo test <test_filter>

# 5. Note: Full 170+ test suite regression on Linux default target is offloaded to GitHub Actions CI.
# Avoid running full `mise run test` (> 2 min) locally on every iteration.
```

---

## 4. Progressive Reference Guides (Read as Needed)

Follow progressive disclosure: consult specific reference files depending on your current task:

### Task: Implementing an Issue / Feature / Bugfix
Read **[references/issue-pr-workflow.md](references/issue-pr-workflow.md)**
- The strict 5-phase Issue + Draft PR chronological lifecycle (`gh pr create --draft`).
- Single-item focused implementation loop: atomic commit, immediate PR checklist sync (`- [x]`), and push per item.
- Marking PR ready for review once all checklist tasks are completed.
- **Review Bot Triage & Verification**: Never rush to merge! Qodo reports via PR comments (not Check Runs). Poll until Qodo and CodeRabbit finish analyzing, resolve all reported bugs within the SAME PR, and verify `Bugs (0)` before merging. Use `gh pr checks --watch --interval 10` for native CI monitoring.

### Task: Architecture, Module Boundaries & Safety Invariants
Read **[references/architecture-and-safety.md](references/architecture-and-safety.md)**
- Module-by-module breakdown across all 14 files in `src/`.
- Rust safety standards: audited `unsafe` isolation, zero-unwrap policy, resource bounds (capped caches/pools).
- Non-blocking PTY master scheduling and asynchronous background paste for large payloads (> 4KB).

### Task: Implementing or Debugging Terminal Protocols
Read **[references/terminal-protocols.md](references/terminal-protocols.md)**
- Complete protocol specification and parsing rules (Kitty Graphics, Kitty Keyboard, text-input-v3, SGR mouse, Synchronized Output, OSC 8/52/7/133, Undercurl).
- Protocol-specific edge cases: colon subparameters vs independent semicolons in SGR, alternate-screen state isolation, bitmask narrowing in `u16` space, Wayland seat serial propagation.

### Task: Handling Review Bot Feedback (Qodo, CodeRabbit, Greptile)
Read **[references/review-bots-guide.md](references/review-bots-guide.md)**
- Recognizing bot states (`Qodo is busy working` vs `Code Review by Qodo`).
- Systematic review triage, defensive fixing, regression test addition, and re-review polling.
- Common terminal emulator failure modes flagged by automated reviewers.
