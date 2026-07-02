#!/usr/bin/env bash
# CI lint: fail on scoped-visibility modifiers in aether-capabilities
# (issue 2471). The convention lives in CLAUDE.md: this crate expresses
# visibility with exactly two forms — `pub`, or no modifier — and lets
# module privacy plus curated re-exports carry reach. The scoped forms
# `pub(crate)`, `pub(super)`, and `pub(in ...)` only restate what the
# module tree already enforces, so they are banned here.
#
# Over-broad visibility resolves by privatizing the item or restructuring
# the module, never by adding a scoped modifier. The `pub(in ...)` form
# additionally hard-codes module paths that silently change meaning when
# files move.
#
# Scope is aether-capabilities only; whether other crates adopt the rule
# (and this path list grows) is a separate decision.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

matches=$(git ls-files 'crates/aether-capabilities/src/*.rs' \
  | xargs grep -nHE 'pub\((crate|super|in [^)]*)\)' 2>/dev/null || true)

if [[ -n "$matches" ]]; then
  echo "scoped-visibility modifiers are banned in aether-capabilities" >&2
  echo "(CLAUDE.md conventions); use plain \`pub\` or privatize/restructure:" >&2
  echo "$matches" >&2
  exit 1
fi
echo "check-no-scoped-visibility: clean."
