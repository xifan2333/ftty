#!/usr/bin/env bash
# Audits AST anti-patterns across Rust files changed relative to the upstream
# merge-base, or explicit file arguments.
#
# Only `deep-if-branching` and `no-dbg` are hard gates (`--error=<rule>`).
# `nested-loops` stays a warning: nested iteration over 2-D grid data (rows x
# cells) and test scaffolding is legitimate linear traversal, not O(N^2)
# algorithmic degradation, and should not force code-rewrites to dodge the
# detector.
#
# Usage:
#   scripts/check-ast-changed.sh           # Audit files changed vs origin/main merge-base
#   scripts/check-ast-changed.sh <file>…   # Audit specific files
set -euo pipefail

FILES=("$@")
if [ "${#FILES[@]}" -eq 0 ]; then
  BASE_REF="$(git merge-base HEAD origin/main 2>/dev/null || echo "origin/main")"
  mapfile -t FILES < <(
    git diff --name-only "${BASE_REF}" 2>/dev/null
    git diff --cached --name-only
    git ls-files --others --exclude-standard
  ) || true
fi

mapfile -t RUST_FILES < <(printf '%s\n' "${FILES[@]}" | grep -E '^src/.*\.rs$' || true)
if [ "${#RUST_FILES[@]}" -eq 0 ]; then
  echo "[check:ast:changed] no touched Rust files"
  exit 0
fi

ast-grep scan --error=deep-if-branching --error=no-dbg "${RUST_FILES[@]}"
