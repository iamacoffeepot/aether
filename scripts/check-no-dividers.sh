#!/usr/bin/env bash
# CI lint: fail on section-divider banner comments in Rust source
# (issue 1039). The convention lives in CLAUDE.md: no `// ---- label ----`
# banners; split into modules if visual structure is needed.
#
# A banner is a comment whose content is a run of dashes/equals directly
# after the comment marker, optionally wrapping a short label. ASCII
# diagrams are NOT banners — they carry structure (digits, arrows,
# parentheses, pipes, plus-corners), so a candidate line is flagged only
# when it contains none of those. The negative cases this must keep
# passing: the state-machine diagram in
# crates/aether-substrate/src/scheduler/slot.rs and the coordinate
# sketch in crates/aether-mesh/src/tessellate/cdt/triangulate.rs.
# (A banner whose label contains a digit slips the filter — accepted:
# the lint is a tripwire for the common class, not a parser.)
set -euo pipefail
cd "$(git rev-parse --show-toplevel)"

# One anchored expression: the comment content must be a dash/equals run
# to end of line, allowing only label-ish characters (letters, spaces,
# more dashes) after it — any diagram character ends the match. The
# filter can't be a second grep over the match list: the `file:line:`
# prefix always contains digits.
matches=$(git ls-files '*.rs' \
  | xargs grep -nHE '^[[:space:]]*//[/!]?[[:space:]]*[-=]{4,}[^()<>0-9|+]*$' 2>/dev/null || true)

if [[ -n "$matches" ]]; then
  echo "section-divider banner comments are banned (CLAUDE.md conventions);" >&2
  echo "use plain comments or split into modules:" >&2
  echo "$matches" >&2
  exit 1
fi
echo "check-no-dividers: clean."
