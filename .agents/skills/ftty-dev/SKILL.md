---
name: ftty-dev
description: >
  REQUIRED for developing, maintaining, and testing ftty (Fan TTY) terminal emulator.
  Use whenever editing Cargo.toml, mise.toml, hk.pkl, or Rust sources in src/.
  Trigger on: 开发ftty, 编写终端代码, 提PR, 运行质量门禁, 代码体检.
---

# ftty: Developer & Engineering Guide

This skill governs the development workflows and quality gates for `ftty`.

## Commands

```bash
mise run check:plan     # preview checks
mise run check:changed  # run quality checks on changed files
mise run fix            # auto-format with rustfmt
mise run build          # cargo build debug
mise run build:release  # cargo build release
mise run test           # cargo test
```

## Issue + Draft PR Workflow

Always create an issue first, open a draft PR with unchecked tasks `- [ ]`, develop one task at a time, verify quality gates, make local atomic commits, and push/merge cleanly upon completion.
