#!/usr/bin/env bash
# Pre-flight gate for Claude-driven `git push` / `gh pr create` Bash calls.
#
# The git pre-push hook (.githooks/pre-push) does the same gating for any
# pusher (CLI, IDE, Claude). What this Claude-side hook adds:
#
#   1. Earlier failure. The check fires before `git push` starts uploading,
#      so a stale tree fails in milliseconds instead of after a slow push +
#      pre-push pre-flight cycle.
#
#   2. The RustRover MCP nudge. The git hook can only run shell-callable
#      checks (fmt / clippy / doc / nextest / wasm32). The qodana-equivalent
#      surface lives in RustRover's IDE inspector, callable as the
#      `mcp__rustrover__get_file_problems` MCP tool from Claude. This hook
#      reminds Claude to run that tool over the diff before pushing.
#
# Reads the Bash tool-call JSON from stdin (Claude Code PreToolUse hook
# protocol). Exits 0 to allow, 2 to block (stdout body returns to Claude).

set -u

input=$(cat)
command=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')

case "$command" in
    *"git push"*|*"gh pr create"*) ;;
    *) exit 0 ;;
esac

# User-elected bypass. The git pre-push hook will also see --no-verify.
case "$command" in
    *"--no-verify"*) exit 0 ;;
esac

if ! command -v git >/dev/null 2>&1; then
    exit 0
fi

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    exit 0
fi

head_sha=$(git rev-parse HEAD 2>/dev/null || true)
[[ -z "$head_sha" ]] && exit 0

stamp_file="$(git rev-parse --git-dir)/aether-preflight-passed"
if [[ -f "$stamp_file" ]]; then
    stamped_sha=$(awk '{print $1}' "$stamp_file" 2>/dev/null || echo)
    if [[ "$stamped_sha" == "$head_sha" ]]; then
        exit 0
    fi
fi

# No matching stamp. Compute the diff vs origin/main so the message tells
# Claude exactly which files to inspect.
if git rev-parse --verify origin/main >/dev/null 2>&1; then
    base=$(git merge-base HEAD origin/main 2>/dev/null \
        || git rev-parse origin/main)
else
    base=$(git rev-parse HEAD~1 2>/dev/null || echo "$head_sha")
fi
rust_files=()
while IFS= read -r f; do
    [[ -n "$f" ]] || continue
    [[ "$f" =~ \.rs$ ]] && rust_files+=("$f")
done < <(git diff --name-only "$base..HEAD" 2>/dev/null || true)

{
    echo "[claude pre-push] no pre-flight stamp for HEAD ($head_sha)."
    echo
    echo "Before pushing, run the local pre-flight:"
    echo
    echo "    scripts/preflight.sh"
    echo
    if [[ ${#rust_files[@]} -gt 0 ]]; then
        echo "Then run \`mcp__rustrover__get_file_problems\` over each changed .rs"
        echo "file (the qodana-equivalent that preflight.sh can't run locally)."
        echo "IMPORTANT: pass \`errorsOnly: false\` — the default is errors-only"
        echo "and Qodana CI flags NOTICE-level findings (Duplicated code"
        echo "fragment, Unnecessary path prefix) that get missed otherwise:"
        echo
        for f in "${rust_files[@]}"; do
            echo "    - $f"
        done
        echo
    fi
    echo "Once preflight.sh exits 0 the stamp updates and the push proceeds."
    echo "To bypass deliberately (e.g. emergency docs push), re-run the push"
    echo "with --no-verify."
} >&2

exit 2
