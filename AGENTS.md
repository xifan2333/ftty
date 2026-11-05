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

1. Mark PR ready for review:
   ```bash
   gh pr ready
   ```

2. Note which review bots this repository actually has. Verified across PRs #282-#288:

   | Bot | State on this repo |
   | --- | --- |
   | Qodo | **not installed** — zero reviews or comments, ever |
   | Greptile | **not installed** — zero reviews or comments, ever |
   | CodeRabbit | intermittent — it reviews some PRs and on others replies "this repository does not receive automatic reviews because it has fewer than 10 stars" and skips |

   Do not wait for a bot that has never posted on this repository. Polling for Qodo is waiting for an event that cannot happen.

### Phase 4: Review Triage

The only reliable gate is GitHub Actions. Handle bot reviews when they exist, and detect their absence instead of waiting it out.

1. **Watch CI natively** — no hand-rolled `sleep` loop:
   ```bash
   gh pr checks <pr_id> --watch --interval 10
   ```

2. **Check whether a bot actually reviewed this PR** rather than assuming one did:
   ```bash
   gh pr view <pr_id> --json reviews,comments \
     --jq '[.reviews[].author.login, .comments[].author.login] | unique | map(select(test("bot|coderabbit|qodo|greptile")))'
   ```
   An empty array means no bot engaged. Proceed to Phase 5; do not poll.

3. **If a bot did review**, triage against the current `HEAD_SHA`:
   - Confirm the review covers the current head, otherwise it is stale:
     ```bash
     HEAD_SHA=$(git rev-parse HEAD)
     gh pr view <pr_id> --json reviews --jq '.reviews[] | .body' | grep -F "$HEAD_SHA"
     ```
   - Fix blocking findings in the same PR, with a regression test. Never merge a known-buggy PR to fix it in a follow-up.
   - Rebut false positives in a reply on the thread instead of silently ignoring them, so the next reader sees why.

4. **Run local fast gates before every push**:
   ```bash
   mise run fix && mise run check:changed
   ```

### Phase 5: Squash-Merge

**Pre-merge checklist:**
1. [ ] `gh pr checks <pr_id>` is 100% `pass` (CodeRabbit showing `skipping` is not a failure).
2. [ ] Every bot review that exists on this PR has its blocking findings fixed, or explicitly rebutted on the thread.
3. [ ] The branch is pushed and matches local `HEAD`.

Then squash-merge and delete the branch:
```bash
gh pr merge <pr_id> --squash --delete-branch
```

