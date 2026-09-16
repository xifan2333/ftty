# Agent Instructions & Project Guidelines for ftty

`ftty` is an ultra-lightweight, minimalist Wayland terminal emulator with native Kitty graphics protocol support, written in Rust.

---

## 1. Supreme Architecture Principles (最高原则)

1. **Unix Philosophy in Terminal Emulation**:
   - **Do One Thing Well**: `ftty` is strictly a PTY terminal surface. No built-in tabs, no split panes, no multiplexing. Tiling belongs to the window manager (`xwm` / `xrwm`), session multiplexing belongs to `herdr` or `tmux`.
   - **Text as the Universal Interface**: Configuration is simple declarative TOML with `include` support.
2. **Minimalist & Bloat-Free**:
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
- **Commit message validation**: hk built-in `check-conventional-commit`

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

### Phase 1: Issue Discovery & Branch Initialization

1. Inspect issue: `gh issue view <id>`
2. Checkout feature branch: `git checkout -b <branch_name>`
3. Empty commit and push:
   ```bash
   git commit --allow-empty -m "<type>(<scope>): start <task_summary> (#<issue_id>)"
   git push -u origin <branch_name>
   ```
4. Create Draft PR with all checklist items unchecked (`- [ ]`):
   ```bash
   gh pr create --draft --title "<title>" --body "..."
   ```

### Phase 2: Single-Item Focused Implementation Loop

For each sub-task in the issue checklist (in strict sequential order, maintaining minimal granularity):

1. **Targeted Implementation**: Implement code changes targeted strictly to that single task.
2. **Quality Gates Preview & Execution**:
   - Preview checks: `mise run check:plan`
   - Auto-format and lint: `mise run fix && mise run check:changed`
   - Run unit tests: `mise run test`
3. **Local Atomic Commit**: Create an atomic commit following Conventional Commits format:
   ```bash
   git add <modified_files>
   git commit -m "<type>(<scope>): <concise summary> (#<issue_id>)"
   ```
4. **Immediate PR Progress Sync**: Update the Draft PR description immediately to check off the completed item (`- [x]`):
   ```bash
   gh pr edit --body "..."
   ```
5. **Incremental Push**: Push the commit to the remote branch immediately to guarantee transparent progress:
   ```bash
   git push origin <branch_name>
   ```

### Phase 3: PR Readiness & Review Activation

Once all checklist items are completed, checked off, and pushed:

1. Mark PR ready for review (this activates review bots: Qodo, CodeRabbit, Greptile):
   ```bash
   gh pr ready
   ```

### Phase 4: Automated Review Triage & Fix Loop (Post-Ready)

Once the PR is marked ready, CI gates and review bots automatically analyze the changes:

1. **Poll Check Status & Feedback**:
   - Verify CI status: `gh pr checks`
   - Inspect PR comments: `gh pr view <pr_id> --comments`
   - Inspect line-level review comments: `gh api repos/:owner/:repo/pulls/<pr_id>/comments`
2. **Review Bot Feedback Ingestion**:
   - **CodeRabbit**: Extract `> Prompt for AI Agents` structured blocks when available.
   - **Greptile**: Inspect cross-file architecture consistency alerts (`greptile.json`).
3. **Defensive Fix & Verification**:
   - Treat all bot feedback as review suggestions; verify against actual code logic.
   - Run `mise run check:changed` and `mise run test` locally.
   - Commit atomic fix: `git commit -m "fix(review): address review feedback (#<issue_id>)"` and push.

### Phase 5: Final Squash-Merge

1. Confirm all CI checks and bot checks pass (`gh pr checks`).
2. Perform squash-merge and delete the remote branch:
   ```bash
   gh pr merge --squash --delete-branch
   ```
