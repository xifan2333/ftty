# ftty — Fast & Frugal Wayland Terminal Emulator

<p align="center">
  <a href="https://www.rust-lang.org"><img src="https://img.shields.io/badge/Rust-2024_Edition-black?style=flat-square&logo=rust&logoColor=white" alt="Rust" /></a>
  <a href="https://wayland.freedesktop.org"><img src="https://img.shields.io/badge/Wayland-Native-005A9C?style=flat-square&logo=wayland&logoColor=white" alt="Wayland" /></a>
  <a href="https://kernel.org"><img src="https://img.shields.io/badge/Linux-Platform-FCC624?style=flat-square&logo=linux&logoColor=black" alt="Linux" /></a>
  <a href="https://github.com/xifan2333/ftty/actions/workflows/ci.yml"><img src="https://img.shields.io/github/actions/workflow/status/xifan2333/ftty/ci.yml?branch=main&label=CI&style=flat-square&logo=githubactions&logoColor=white" alt="CI Status" /></a>
  <a href="https://www.gnu.org/licenses/gpl-3.0"><img src="https://img.shields.io/badge/License-GPL--3.0-blue?style=flat-square" alt="License" /></a>
</p>

`ftty` is an ultra-lightweight, minimalist Wayland terminal emulator built with Rust. It provides first-class support for the **Kitty Graphics Protocol** and **Fcitx5 Chinese IME** while remaining strictly obedient to the Unix philosophy of doing one thing well.

---

## 1. Core Principles

- **Minimalist & Bloat-Free**:
  - No tabs, no split panes, no multiplexing. Tiling belongs to the window manager (`xwm` / `xrwm`), session management belongs to `herdr` or `tmux`.
  - No bloated GUI frameworks (no GPUI, no GTK, no Qt). Direct Wayland client communication.
  - Sub-3MB stripped binary, sub-15MB idle memory, and sub-3ms cold boot latency.
- **Native Kitty Graphics Protocol**:
  - Full APC `\x1b_G` transmission parsing.
  - Zero-copy POSIX shared memory (`t=s`) and direct image rendering for seamless `yazi` previews and Neovim image plugins.
- **Fcitx5 IME First-Class Citizen**:
  - Wayland `text-input-v3` implementation ensuring candidate popup boxes track cursor coordinates pixel-accurately.
- **Signal-Driven Theme Hot-Reloading**:
  - Dynamic palette switching via `SIGUSR1` or configuration `include` directives with zero screen flickering.

---

## 2. Configuration Reference

`ftty` loads its configuration from `$XDG_CONFIG_HOME/ftty/ftty.toml` (or `~/.config/ftty/ftty.toml`). All settings support dynamic hot-reloading via `SIGUSR1` or configuration `include` directives.

```toml
# Include modular themes via relative path
# include = ["themes/catppuccin-mocha.toml"]

[font]
# Multi-font fallback chain: primary monospace face, followed by CJK and Emoji fallbacks
family = ["JetBrainsMono Nerd Font", "Noto Sans CJK SC", "Noto Color Emoji"]
size = 14.0

[window]
# Surface padding: [x, y] or uniform scalar N (window dimensions are managed by the WM)
padding = [4, 4]

[clipboard]
# OSC 52 remote clipboard security policies:
# allow_osc52_read: allows CLI applications to query/read the system clipboard.
# Disabled by default (false) to prevent untrusted remote SSH processes from silently exfiltrating credentials.
allow_osc52_read = false

# allow_osc52_write: allows CLI applications (nvim, tmux, yazi) to set or clear the system clipboard.
allow_osc52_write = true

[keybindings]
"Ctrl+Shift+Z" = "prompt_prev"
"Ctrl+Shift+X" = "prompt_next"
"Ctrl+Shift+U" = { pipe_visible = ["urlscan"] }
```

---

## 3. Developer Workflow & Quality Gates

This repository uses **mise** and **hk** with automated quality gates:

```bash
mise run check:plan     # preview linter execution plan
mise run check:changed  # run rustfmt, clippy, taplo on modified files
mise run fix            # auto-format modified files
mise run build          # compile debug binary
mise run build:release  # compile optimized release binary
mise run test           # run test suite
```

---

## 4. License

GPL-3.0-only
