#!/usr/bin/env bash
# Pre-flight gate for Claude-driven `git push` / `gh pr create` Bash calls.
#
# The git pre-push hook (.githooks/pre-push) does the same gating for any
# pusher (CLI, IDE, Claude). What this Claude-side hook adds: earlier
# failure — the check fires before `git push` starts uploading, so a stale
# tree fails in milliseconds instead of after a slow push + pre-push
# pre-flight cycle. (Qodana is no longer a separate nudge: it runs inside
# `scripts/preflight.sh --qodana`, which the implement-agent push path
# passes; see CLAUDE.md § "Qodana pre-flight".)
#
# Reads the Bash tool-call JSON from stdin (Claude Code PreToolUse hook
# protocol). Exits 0 to allow, 2 to block (stdout body returns to Claude).

set -u

input=$(cat)
command=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')

# Match `git push` / `gh pr create` only in command position — at the
# start of a line or right after a separator (`;`, `&`, `|`, and thus
# `&&` / `||`). A real invocation is always in command position; a
# heredoc or quoted body that merely *mentions* the command name (e.g.
# an issue describing `gh pr create`) is not, so it no longer
# false-positives a `gh issue create` whose body quotes the name.
if ! printf '%s' "$command" \
    | grep -qE '(^|[;&|])[[:space:]]*(git[[:space:]]+push|gh[[:space:]]+pr[[:space:]]+create)'; then
    exit 0
fi

# User-elected bypass. The git pre-push hook will also see --no-verify.
case "$command" in
    *"--no-verify"*) exit 0 ;;
esac

if ! command -v git >/dev/null 2>&1; then
    exit 0
fi

# iamacoffeepot/aether#1199: the gated command may target a worktree via a
# leading `cd <path> &&` — the /implement skill pushes from
# `.claude/worktrees/<slug>` / `/tmp/aether-*`. Evaluate git state in that
# directory rather than the hook's own cwd (the main checkout); otherwise
# HEAD and the stamp gitdir resolve against the wrong tree and a clean
# in-worktree preflight is false-blocked. No `cd` prefix (the main-checkout
# push) leaves cwd untouched, so that path is unchanged.
cd_prefix_re='^[[:space:]]*cd[[:space:]]+([^[:space:];&|]+)'
if [[ "$command" =~ $cd_prefix_re ]]; then
    target_dir="${BASH_REMATCH[1]}"
    if [[ -n "$target_dir" && -d "$target_dir" ]]; then
        cd "$target_dir" || true
    fi
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

# No matching stamp — tell Claude to run the pre-flight.
{
    echo "[claude pre-push] no pre-flight stamp for HEAD ($head_sha)."
    echo
    echo "Before pushing, run the local pre-flight:"
    echo
    echo "    scripts/preflight.sh"
    echo
    echo "On the implement-agent push path (about to open a CI-checked PR),"
    echo "pass --qodana so the pre-flight also runs the same qodana scan CI"
    echo "gates on:"
    echo
    echo "    scripts/preflight.sh --qodana"
    echo
    echo "Once preflight.sh exits 0 the stamp updates and the push proceeds."
    echo "To bypass deliberately (e.g. emergency docs push, or a known"
    echo "Qodana-for-Rust EAP tooling flake), re-run the push with --no-verify."
} >&2

exit 2
