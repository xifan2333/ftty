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
mise run check:changed  # run hk checks (rustfmt + clippy) across modified files (fast, < 2s)
mise run fix            # auto-format modified files
cargo test <owning_module_or_test_name> # run targeted unit tests (verify tests run > 0, fast, < 1s)
```

### Hardware Liberation Principle (解放本地硬件原则)

> ⚡ **CRITICAL DIRECTIVE**: Local developer hardware must be liberated!
> - **DO NOT run heavy release builds (`cargo build --release`) locally** unless the user explicitly requests benchmarking or actual manual testing.
> - **DO NOT run full test suites (`mise run test` / `cargo test`) locally** across micro-tasks.
> - Rely primarily on **GitHub Actions CI** for heavy compilation and full-suite testing.
> - Keep local commands strictly to fast gates: `mise run fix && mise run check:changed` (< 2s) and targeted single-function unit tests when needed (< 1s).

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
   - Fast targeted test: `cargo test <owning_test_name>` (verify executed test count > 0, fast < 2s).
   - *Note*: Rely on GitHub Actions CI for full-suite verification; do not run full `mise run test` locally on every item.
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

> ⚠️ **CRITICAL MERGE PREVENTION RULE**:
> `gh pr checks` ONLY reflects GitHub Actions CI and CodeRabbit status. **Qodo DOES NOT create a Check Run — Qodo reports ONLY via PR comments.**
> A green `gh pr checks` is **NOT** sufficient to merge! You MUST wait for Qodo to complete and verify `Bugs (0)`.

1. **Watch CI with Native Interval**:
   - Do NOT use manual `sleep` scripts. Use the native `--watch` flag:
     ```bash
     gh pr checks <pr_id> --watch --interval 10
     ```
   - Let GitHub Actions CI execute the full 170+ test suite and compilation in the cloud.

2. **Poll Review Bot Comments (Single-PR Resolution & Head Match)**:
   - Fetch the current pull-request HEAD commit SHA:
     ```bash
     HEAD_SHA=$(git rev-parse HEAD)
     ```
   - Check Qodo review status and ensure it evaluates the current `HEAD_SHA`:
     ```bash
     gh pr view <pr_id> --json comments --jq '.comments[] | select(.author.login=="qodo-code-review") | .body' | grep -F "$HEAD_SHA"
     ```
   - If Qodo has not evaluated `HEAD_SHA` yet or shows `Qodo is busy working`: **MERGING IS STRICTLY FORBIDDEN**. Wait and poll again.
   - Once the review for `HEAD_SHA` is complete, inspect `Bugs (N)`:
     - If `Bugs > 0`:
       - Inspect the comment cards in detail (`gh pr view <pr_id> --comments`).
       - **ALL bugs MUST be resolved within the SAME PR before merging.** Never merge a buggy PR to fix in a subsequent PR.

3. **Defensive Fix & Verification**:
   - Implement targeted fix and add unit regression tests.
   - Run local fast-gates: `mise run fix && mise run check:changed && cargo test <module>::tests`.
   - Commit atomic fix:
     ```bash
     git commit -m "fix(review): address review feedback (#<issue_id>)"
     git push origin <branch_name>
     ```
   - Loop back to Step 1: watch CI, wait for Qodo re-review, and verify it updates to **`Bugs (0)`** and **`✓ Resolved`**.

### Phase 5: Final Squash-Merge

**Pre-Merge Hard Checklist** (All 5 conditions MUST be satisfied):
1. [x] `gh pr checks <pr_id>` is 100% green (`pass`).
2. [x] Qodo has evaluated the current PR `HEAD_SHA` (`gh pr view <pr_id> --json comments ... | grep "$HEAD_SHA"`).
3. [x] Qodo status is `Code Review by Qodo` (no `Qodo is busy working`).
4. [x] Qodo reports **`Bugs (0)`** on the current HEAD commit and all previously flagged items show `[✓ Resolved]`.
5. [x] CodeRabbit and Greptile have no unresolved blocking feedback.

Once all 4 conditions are met, perform squash-merge and branch cleanup:
```bash
gh pr merge <pr_id> --squash --delete-branch
```
