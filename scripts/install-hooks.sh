#!/usr/bin/env bash
# Install the project's git pre-push hook.
#
# Pre-push runs `make pre-push` — a fast `cargo fmt --check` + `tsc --noEmit`.
# It does NOT run clippy/test/build (those need libclang+pkg-config+gstreamer
# dev headers installed, which contributors on a fresh box may not have).
# GitHub Actions runs the full `make ci` on every PR — that's the canonical
# correctness gate.
#
# Usage:
#   make install-hooks
#   # or directly:
#   bash scripts/install-hooks.sh
#
# Re-runs are idempotent: an existing hook is overwritten.

set -euo pipefail

REPO_ROOT="$(git rev-parse --show-toplevel)"
HOOK_PATH="$REPO_ROOT/.git/hooks/pre-push"

if [ ! -d "$REPO_ROOT/.git" ]; then
    echo "error: not inside a git repository" >&2
    exit 1
fi

cat > "$HOOK_PATH" <<'HOOK'
#!/bin/sh
#
# Pre-push hook: fast checks only (fmt + type-check, no link).
# Installed by scripts/install-hooks.sh — re-run that to update.
#
# To skip in an emergency: git push --no-verify

echo "Running pre-push checks (make pre-push)..."
echo ""

if ! make pre-push; then
    echo ""
    echo "Pre-push checks FAILED. Push aborted."
    echo "Fix the issues above, then try again."
    echo "To skip this hook: git push --no-verify"
    exit 1
fi
HOOK

chmod +x "$HOOK_PATH"
echo "Installed pre-push hook at $HOOK_PATH"
echo "It runs 'make pre-push' (fast: fmt + tsc --noEmit)."
echo "GitHub Actions runs the full 'make ci' on every PR."
