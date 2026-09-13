# Agent Instructions & Project Guidelines for ftty

`ftty` is an ultra-lightweight, Suckless Wayland terminal emulator with native Kitty graphics protocol support, written in Rust.

---

## 1. Supreme Architecture Principles (最高原则)

1. **Unix Philosophy in Terminal Emulation**:
   - **Do One Thing Well**: `ftty` is strictly a PTY terminal surface. No built-in tabs, no split panes, no multiplexing. Tiling belongs to the window manager (`xwm` / `xrwm`), session multiplexing belongs to `herdr` or `tmux`.
   - **Text as the Universal Interface**: Configuration is simple declarative TOML with `include` support.
2. **Suckless Frugality**:
   - **No Heavy GUI Frameworks**: No GPUI, no GTK, no Qt. Direct native Wayland client (`wayland-client` / EGL).
   - **Resource Constrained**: Stripped binary < 3 MB, idle memory < 15 MB, cold launch < 3 ms.
   - **Cognitive Maintainability**: Keep code small and readable. Mechanism over policy.
3. **Core Protocol First-Class Support**:
   - **Native Kitty Graphics Protocol**: APC `\x1b_G` parser, direct/shm memory mapping for zero-copy image transfers (for `yazi`, `image.nvim`).
   - **Fcitx5 IME First-Class Citizen**: Wayland `text-input-v3` integration for precise candidate box cursor tracking.
   - **Theme Hot-Reloading**: Signal-driven (`SIGUSR1`) dynamic palette switching with zero flicker.

---

## 2. Project Layout

| Goal                         | Target File / Directory                       |
| ---------------------------- | --------------------------------------------- |
| Package specs & dependencies | `Cargo.toml`                                  |
| Developer tooling & tasks    | `mise.toml`                                   |
| Quality gates & git hooks    | `hk.pkl`                                      |
| Code quality automation      | `mise run check:plan`, `check:changed`, `fix` |
| Main application entrypoint  | `src/main.rs`                                 |

---

## 3. Code Quality & hk Quality Gates

This repository uses **hk** (`hk.pkl`) for git hooks and code quality checks:

- **Rust formatting**: `rustfmt`
- **Rust linting**: `cargo clippy --all-targets -- -D warnings`
- **TOML**: `taplo` (with `--no-schema`)
- **Shell / Markdown**: `shellcheck`, `shfmt`, `prettier`

Run quality commands during development:

```bash
mise run check:plan     # preview execution plan for modified files
mise run check:changed  # run hk checks across modified/staged/untracked files
mise run fix            # auto-format modified files
mise run build          # compile project
```

---

## 4. Strict Chronological Development Workflow (Issue + Draft PR)

All coding agents must strictly adhere to the SOP:

1. `gh issue view <id>` -> checkout branch -> empty commit -> push -> `gh pr create --draft` (all tasks unchecked `- [ ]`).
2. Single-Item Focused Loop -> Local Quality Gate (`check:plan`, `check:changed`, `fix`) -> Local Atomic Commit.
3. Unified Push, Checks & Merge (`git push`, `gh pr edit`, `gh pr checks`, `gh pr ready`, `gh pr merge --squash --delete-branch`).
