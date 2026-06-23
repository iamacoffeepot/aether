#!/usr/bin/env bash
# Pre-flight gate for Claude-driven `git push` / `gh pr create` Bash calls.
#
# The git pre-push hook (.githooks/pre-push) does the same gating for any
# pusher (CLI, IDE, Claude). What this Claude-side hook adds: earlier
# failure — the check fires before `git push` starts uploading, so a stale
# tree fails in milliseconds instead of after a slow push + pre-push
# pre-flight cycle. (Qodana is not a pre-flight step: it is a required CI
# gate that `/land` resolves from the `qodana-report` artifact; see
# CLAUDE.md § "Qodana".)
#
# Reads the Bash tool-call JSON from stdin (Claude Code PreToolUse hook
# protocol). Exits 0 to allow, 2 to block (stdout body returns to Claude).

set -u

input=$(cat)
command=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')
hook_cwd=$(printf '%s' "$input" | jq -r '.cwd // ""')

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

# Worktree resolution — most-specific source wins:
# (1) explicit `cd <path> &&` prefix on the gated command (#1199 behavior, unchanged);
# (2) session cwd from the hook input JSON (.cwd), supplied by Claude Code for
#     worktree-bound agents — closes the bare-push gap without a cd prefix
#     (iamacoffeepot/aether#2200);
# (3) current process cwd (main-checkout fallback when neither source is available).
# Guard both hops with -n/-d before cd so a missing/garbage value fails open.
cd_prefix_re='^[[:space:]]*cd[[:space:]]+([^[:space:];&|]+)'
if [[ "$command" =~ $cd_prefix_re ]]; then
    target_dir="${BASH_REMATCH[1]}"
elif [[ -n "$hook_cwd" && -d "$hook_cwd" ]]; then
    target_dir="$hook_cwd"
else
    target_dir=""
fi
if [[ -n "$target_dir" && -d "$target_dir" ]]; then
    cd "$target_dir" || true
fi

if ! git rev-parse --git-dir >/dev/null 2>&1; then
    exit 0
fi

# Resolve the pushed sha. Parse the command for an explicit refspec: after
# "push", drop flags (-*) and the first bare positional (the remote, e.g.
# "origin"); the next positional is the refspec — take its local side (left
# of ":"). If no refspec positional is found, fall back to the resolved cwd's
# HEAD. For `gh pr create` (no refspec) we also fall through to HEAD.
local_ref=""
if [[ "$command" =~ git[[:space:]]+push(.*) ]]; then
    positional_count=0
    for tok in ${BASH_REMATCH[1]}; do
        case "$tok" in
            -*) ;;
            *)
                positional_count=$((positional_count + 1))
                if [[ $positional_count -eq 2 ]]; then
                    local_ref="${tok%%:*}"
                    break
                fi
                ;;
        esac
    done
fi

if [[ -n "$local_ref" ]]; then
    pushed_sha=$(git rev-parse --verify "${local_ref}^{commit}" 2>/dev/null || true)
else
    pushed_sha=$(git rev-parse --verify HEAD 2>/dev/null || true)
fi

# Fail open when the pushed sha can't be resolved — defer to .githooks/pre-push.
[[ -z "$pushed_sha" ]] && exit 0

# Scan every worktree stamp: the common git dir's own stamp and every linked
# worktree's per-worktree stamp. A match on any allows the push — the stamp
# records the exact attested sha, so a cross-worktree scan cannot false-allow.
common=$(git rev-parse --git-common-dir 2>/dev/null || true)
[[ -z "$common" ]] && exit 0
[[ "$common" = /* ]] || common="$(pwd)/$common"

for stamp_file in "$common/aether-preflight-passed" "$common"/worktrees/*/aether-preflight-passed; do
    if [[ -f "$stamp_file" ]]; then
        stamped_sha=$(awk '{print $1}' "$stamp_file" 2>/dev/null || true)
        if [[ "$stamped_sha" == "$pushed_sha" ]]; then
            exit 0
        fi
    fi
done

# No matching stamp anywhere — tell Claude to run the pre-flight.
{
    echo "[claude pre-push] no pre-flight stamp for HEAD ($pushed_sha)."
    echo
    echo "Before pushing, run the local pre-flight:"
    echo
    echo "    scripts/preflight.sh"
    echo
    echo "Once preflight.sh exits 0 the stamp updates and the push proceeds."
    echo "To bypass deliberately (e.g. an emergency docs push), re-run the"
    echo "push with --no-verify."
} >&2

exit 2
