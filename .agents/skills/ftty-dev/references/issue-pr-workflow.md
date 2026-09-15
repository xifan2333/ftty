# Issue + PR Driven Development Workflow (SOP)

Development on `ftty` must strictly follow the **Pre-Code Draft PR -> Single-Item Loop -> Review Triage -> Merge** chronological lifecycle.

---

## 1. Dual-Planning Model

To maintain development velocity without compromising quality gates:

1. **Phase A: Task Planning (Pre-development - BEFORE CODING)**:
   - Defining *what* to build: Issue analysis, module boundaries, and opening the Draft PR with an unchecked `- [ ]` checklist.
2. **Phase B: Quality Gate Pre-check (Post-edit - AFTER EDIT)**:
   - Previewing *which* checks will execute on edited files via `mise run check:plan`.

---

## 2. Chronological Lifecycle Overview

```
+------------------------------------------------------------------------+
| 1. Pre-Code Initialization (MANDATORY BEFORE ANY CODE IS WRITTEN)       |
|    gh issue view <id>                                                  |
|    git checkout -b <type>/<short-description>                          |
|    git commit --allow-empty -m "<type>(<scope>): start <desc> (#<id>)" |
|    git push -u origin <type>/<short-description>                       |
|    gh pr create --draft (ALL checklist items unchecked: - [ ])         |
+-----------------------------------+------------------------------------+
                                    |
                +-------------------v-------------------+
                | 2. Single-Item Focused Development    |
                |    Only implement the topmost - [ ]   |
                +-------------------+-------------------+
                                    |
                +-------------------v-------------------+
                | 3. Local Quality Gate & Pre-check     |
                |    mise run check:plan                |
                |    mise run fix                       |
                |    mise run check:changed             |
                |    mise run test                      |
                +-------------------+-------------------+
                                    |
                +-------------------v-------------------+
                | 4. Local Atomic Commit                |
                |    git add <modified_files>           |
                |    git commit -m "<type>: ..."        |
                |    (Keep commit atomic)               |
                +-------------------+-------------------+
                                    | (Remaining tasks?)
                                    +-------- Yes -------+
                                    | No                 |
+-----------------------------------v-------------------+|
| 5. Unified Push & Mark Ready                          ||
|    git push origin <branch>                           ||
|    gh pr edit --body (check all completed - [x])      ||
|    gh pr ready                                        ||
+-----------------------------------+-------------------+|
                                    |                    |
+-----------------------------------v-------------------+|
| 6. Review Bot Triage & Verification Loop (CRITICAL)   ||
|    - Poll until review bots finish processing         ||
|    - Inspect Qodo, CodeRabbit, and Greptile feedback  ||
|    - Fix ALL reported bugs defensively                ||
|    - Push fixes and WAIT for bot re-review            ||
|    - DO NOT MERGE until ALL bugs are [Resolved]!      ||
+-----------------------------------+-------------------+|
                                    | (All clean?)       |
                                    +--- No: Loop back --+
                                    | Yes
+-----------------------------------v-------------------+
| 7. Final Squash-Merge                                 |
|    gh pr checks (all green)                           |
|    gh pr merge --squash --delete-branch               |
+-------------------------------------------------------+
```

---

## 3. Detailed Execution Steps

### Phase 1: Pre-Code Initialization
```bash
# 1. Inspect issue (or create if not present)
gh issue view <issue_id>

# 2. Create feature branch
git checkout -b <type>/<short-description>

# 3. Initialize branch with empty commit and push
git commit --allow-empty -m "<type>(<scope>): start <task_summary> (#<issue_id>)"
git push -u origin <type>/<short-description>

# 4. Open Draft PR with ALL tasks UNCHECKED (- [ ])
gh pr create --draft \
  --title "<type>(<scope>): <concise description> (#<issue_id>)" \
  --body "Closes #<issue_id>

### Implementation Checklist
- [ ] Task 1 description
- [ ] Task 2 description
- [ ] Task 3 description"
```

### Phase 2: Single-Item Execution Loop
For each unchecked `- [ ]` task in strict sequential order:
1. **Targeted Implementation**: Write code *strictly* targeted to the topmost unchecked task.
2. **Quality Gates Preview & Execution**:
   ```bash
   mise run check:plan     # preview which checks will run
   mise run fix            # auto-format modified files
   mise run check:changed  # run clippy and rustfmt on changed files
   mise run test           # run all unit tests
   ```
3. **Local Atomic Commit**:
   Commit following Conventional Commits format:
   ```bash
   git add <modified_files>
   git commit -m "<type>(<scope>): <concise summary> (#<issue_id>)"
   ```

### Phase 3: Finalize & Unified Push
```bash
# 1. Push all completed atomic commits
git push origin <branch>

# 2. Update Draft PR body to check off all completed tasks (- [x])
gh pr edit --body "..."

# 3. Mark PR ready for review (activates review bots: CodeRabbit, Qodo, Greptile)
gh pr ready
```

---

## 4. Phase 4: Automated Review Triage & Fix Loop (Post-Ready)

> ⚠️ **SUPREME WARNING: NEVER RUSH TO MERGE!**
> Review bots run asynchronously and may take several minutes to generate their complete analysis.
> **You MUST wait for all bots to finish, inspect every comment, resolve all reported bugs, and wait for re-review confirmation before merging.**

### Step 1: Poll Check Status & Comments
```bash
# Check CI status
gh pr checks <pr_id>

# Inspect PR issue comments (Qodo, CodeRabbit summaries)
gh pr view <pr_id> --comments

# Inspect line-level review comments
gh api repos/:owner/:repo/pulls/<pr_id>/comments
```

### Step 2: Understand Review Bot Statuses
- **Qodo Code Review**:
  - `Qodo is busy working` (Anteater gif): **STILL ANALYZING**. Do not proceed; sleep and poll again!
  - `Code Review by Qodo`: **ANALYSIS COMPLETE**. Check `Bugs (N)`:
    - If `Bugs > 0`: Carefully read each finding, understand the root cause (e.g. edge cases, resource bounds, protocol compliance).
    - If `Bugs (0)` and all items are marked `✓ Resolved`: Cleared.
- **CodeRabbit**:
  - Check for `> Prompt for AI Agents` blocks.
  - Review suggestions and defensive sanity checks.

### Step 3: Implement Defensive Fixes
1. Treat every bot finding as an architectural and correctness inspection:
   - Check if an edge case was missed (e.g., alternate screen buffers, unbounded storage, timeouts, non-blocking I/O backpressure).
2. Apply minimal, high-quality Rust fixes.
3. Add corresponding unit regression tests to prevent recurrence.
4. Run local gates:
   ```bash
   mise run fix
   mise run check:changed
   mise run test
   ```
5. Commit and push the fix:
   ```bash
   git add <modified_files>
   git commit -m "fix(review): <specific fix summary> (#<issue_id>)"
   git push origin <branch>
   ```

### Step 4: Await Bot Re-Review Confirmation
After pushing fixes, **repeat Step 1**:
- Poll until Qodo and CodeRabbit finish analyzing the newly pushed commit.
- Verify that Qodo updates its review report and displays **`Bugs (0)`** and **`✓ Resolved`** on the fixed items.
- Only when all checks are green (`pass`) and no unresolved bugs remain, move to Phase 5.

---

## 5. Phase 5: Final Squash-Merge

```bash
# 1. Confirm all CI checks pass
gh pr checks <pr_id>

# 2. Perform squash-merge and delete branch
gh pr merge <pr_id> --squash --delete-branch

# 3. Pull latest main locally
git checkout main
git pull
```
