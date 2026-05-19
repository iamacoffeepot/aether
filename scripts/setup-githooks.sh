#!/usr/bin/env bash
# Wires up the repo's git hooks: sets core.hooksPath to .githooks so the
# pre-push gate runs the local CI-equivalent pre-flight before each push.
#
# Idempotent. Re-run safely.

set -euo pipefail

ROOT="$(git rev-parse --show-toplevel)"
cd "$ROOT"

git config core.hooksPath .githooks
chmod +x .githooks/pre-push scripts/preflight.sh

echo "git hooks installed: core.hooksPath -> .githooks"
echo
echo "The pre-push hook runs scripts/preflight.sh against the changed file"
echo "set for the push (fmt + clippy + doc + nextest + wasm32 cross-build)."
echo "Skipped automatically for docs-only and CI-only pushes."
echo
echo "Bypass once with:   git push --no-verify"
echo "Run pre-flight ad-hoc:  scripts/preflight.sh"
