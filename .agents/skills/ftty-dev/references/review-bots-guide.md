# Automated Review Bot Triage & Verification Guide

This guide details how coding agents must interact with automated review bots (Qodo, CodeRabbit, Greptile) in `ftty` pull requests.

---

## 1. Supreme Golden Rule

> 🛑 **NEVER RUSH TO MERGE A PR!**
> Marking a PR ready for review triggers asynchronous AI analysis bots.
> **You MUST poll until all bots finish their full analysis, fix every reported issue defensively, and wait for re-review confirmation showing zero unresolved bugs before merging.**

---

## 2. Review Bot Identification & Polling

### 1. Qodo Code Review (`author: qodo-code-review`)
Qodo is a deep semantic analysis bot that detects edge-case defects, resource bounds issues, and protocol inconsistencies.

- **In-Progress State**:
  ```markdown
  <h3>Qodo is busy working</h3>
  Check back in a few minutes. Qodo's code review agents are on it.
  <img src="...anteater-looking-at-ants..." ...>
  ```
  **ACTION**: **WAIT.** Do not merge! Sleep 15-20 seconds and poll with:
  ```bash
  gh pr view <pr_id> --comments
  ```
- **Completed Analysis State**:
  The comment updates to:
  ```markdown
  <h3>Code Review by Qodo</h3>
  Bugs (N)   Rule violations (0)   Skill insights (0)
  ```
  - **Verify Head Commit**: Check the trailing link or commit SHA at the bottom of the comment to ensure it evaluates the current head commit, not an older push.
  - **If `Bugs > 0`**:
    Inspect every bug card. Qodo provides:
    - **Description**: What went wrong.
    - **Evidence**: Specific line references in your code and upstream protocol specs.
    - **Agent prompt**: Recommended remediation instructions.
  - **If `Bugs (0)`**:
    All previous issues have been resolved (`✓ Resolved`). Ready for final approval.

### 2. CodeRabbit (`author: coderabbitai`)
CodeRabbit provides high-level architectural summaries and structured agent prompts.

- Check PR checks: `gh pr checks <pr_id>` (should display `pass`).
- Check PR comments for `> Prompt for AI Agents` blocks. Verify findings independently before applying.

### 3. Greptile (`author: greptile-apps`)
Greptile monitors cross-file consistency and repo-wide patterns.
- Ensure all alerts for the current commit (Confidence $\ge$ 4) are addressed before proceeding to merge. Note that CI checks (`check-and-test`) test only test/build suites, not bot approvals; bot approval must be verified separately via comments.

---

## 3. Systematic Remediation Loop

When a review bot reports issues:

```
+-------------------------------------------------------------+
| 1. Ingest Findings & Understand Root Cause                  |
|    - Avoid superficial bandaids                             |
|    - Identify architectural edge case or protocol mismatch  |
+------------------------------+------------------------------+
                               |
+------------------------------v------------------------------+
| 2. Implement Targeted Defensive Fixes                       |
|    - Address the underlying failure mode                    |
|    - Add corresponding regression tests                     |
+------------------------------+------------------------------+
                               |
+------------------------------v------------------------------+
| 3. Local Verification & Quality Gates                       |
|    - mise run fix                                           |
|    - mise run check:changed                                 |
|    - mise run test                                          |
+------------------------------+------------------------------+
                               |
+------------------------------v------------------------------+
| 4. Atomic Commit & Push                                     |
|    - git commit -m "fix(review): <specific fix> (#<id>)"    |
|    - git push origin <branch>                               |
+------------------------------+------------------------------+
                               |
+------------------------------v------------------------------+
| 5. Wait for Bot Re-Review                                   |
|    - Sleep 15-20s, poll gh pr view <id> --comments          |
|    - Wait for "Qodo is busy working" to finish              |
|    - Verify review applies to current head commit           |
|    - Confirm Bugs count is 0 and items show [✓ Resolved]    |
|    - Confirm CodeRabbit & Greptile have no blockers         |
+------------------------------+------------------------------+
                               | (Bugs remaining?)
                               +--- Yes: Repeat Loop ---------+
                               | No
+------------------------------v------------------------------+
| 6. Proceed to Merge                                         |
|    - gh pr checks (all green)                               |
|    - gh pr merge --squash --delete-branch                   |
+-------------------------------------------------------------+
```

---

## 4. Common Terminal Emulator Pitfalls to Watch For

Based on past review bot findings in `ftty`:
1. **Alternate Screen Leaks**:
   - Operations that reset state (RIS, font reload, geometry resize) must inspect both the active screen (`lines`) and the parked primary screen (`alt_lines`).
2. **Event Loop Starvation / Freezes**:
   - Never use long synchronous timeouts (> 100ms) on the event-loop thread.
   - For synchronized output (Mode 2026), ensure the event loop wakes up periodically (e.g. 50ms) to check expiration deadlines even when PTY/Wayland are quiet.
3. **Unbounded Collections & Memory Exhaustion**:
   - Hyperlink pools, keyboard stacks, image placement caches, and atlas sizes must have defined maximum capacities with deterministic eviction.
4. **Wayland Protocol Serials**:
   - Wayland selection/clipboard requests require a valid, recent seat serial. Ensure `state.last_serial` is updated on keyboard enter, key, and modifier events, not just mouse clicks.
5. **Bitmask Narrowing**:
   - Always apply bitwise masks on the original integer width (`u16`) before downcasting (`u8`) to avoid high-bit truncation aliasing.
