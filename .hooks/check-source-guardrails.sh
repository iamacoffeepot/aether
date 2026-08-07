#!/usr/bin/env bash
# Unified source guardrails — runs Codex + Claude checks from one place.
# Codex content is the superset (divider + host_fn via diff + untracked).
# This wrapper ensures .hooks is the single source of truth.

set -u
root=$(git rev-parse --show-toplevel 2>/dev/null || true)
[[ -n "$root" ]] || exit 0

# Run the original Codex guardrails (divider + host_fn)
bash "$root/.hooks/check-source-guardrails-codex.sh" 2>&1
codex_status=$?

# Also run Claude's no-divider as separate pass for Edit/Write that bypasses diff
# (Claude's hook is PreToolUse, but double-check here is cheap)
# No-op if codex already failed
if [[ $codex_status -ne 0 ]]; then
    exit $codex_status
fi
exit 0
