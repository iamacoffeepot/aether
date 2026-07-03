#!/usr/bin/env bash
# CI lint: fail on inline `mod runtime { ... }` blocks in aether-capabilities
# (issue 2479). The convention lives in CLAUDE.md / the struct-hosted
# actor-macro migration: a cap's runtime half is a sibling `runtime.rs`
# (or `runtime/` directory) declared with a bare `mod runtime;` — never an
# inline `mod runtime { ... }` block. The compiler is indifferent to the
# difference, so nothing but this check stops the inline form from
# regressing back in.
#
# The anchored regex matches only the inline block opener (`mod runtime {`)
# and leaves the sanctioned bare declaration (`mod runtime;`) untouched,
# since the sanctioned form ends in `;` not `{`. The leading-whitespace
# anchor also excludes prose mentions of `mod runtime` inside comments and
# doc lines.
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

matches=$(git ls-files 'crates/aether-capabilities/src/*.rs' \
  | xargs grep -nHE '^[[:space:]]*mod runtime[[:space:]]*\{' 2>/dev/null || true)

if [[ -n "$matches" ]]; then
  echo "inline \`mod runtime\` blocks are banned in aether-capabilities" >&2
  echo "(CLAUDE.md conventions); use a \`runtime.rs\`/\`runtime/\` sibling behind a bare \`mod runtime;\`:" >&2
  echo "$matches" >&2
  exit 1
fi
echo "check-no-inline-runtime-mod: clean."
