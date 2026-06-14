#!/usr/bin/env bash
# SessionStart hook — bind each Claude session to a role and its own worktree
# (ADR-0110 § "Session binding").
#
# On every session start this hook:
#   1. ensures the session's worktree exists at .claude/worktrees/<session-id>
#      (idempotent: `git worktree add` when absent, no-op when already present);
#   2. reads the session-keyed role marker at .claude/roles/<session-id>:
#        - hit  -> injects the matching role directive
#                  (.claude/roles/directives/<role>.md) as session context;
#        - miss -> injects an instruction to ask the user which role applies
#                  and write the marker.
#
# Reads the SessionStart hook payload JSON from stdin (Claude Code hook
# protocol) and emits added context via the hookSpecificOutput.additionalContext
# form. It never blocks — it primes the conversation, it does not gate it. The
# hook cannot change the running session's cwd (fixed at launch), so the session
# stays repo-rooted; the worktree boundary is enforced by a separate PreToolUse
# hook (ADR-0110 § "Hook-enforced guardrails").

set -u

input=$(cat)

# Project root: CLAUDE_PROJECT_DIR is exported for hooks; fall back to the hook
# script's own location (two levels up from .claude/hooks/) when it is unset.
project_dir="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$project_dir" ]]; then
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    project_dir=$(cd "$script_dir/../.." && pwd)
fi

session_id=$(printf '%s' "$input" | jq -r '.session_id // ""')

# Without a session id there is nothing to key the binding on — stay silent.
if [[ -z "$session_id" ]]; then
    exit 0
fi

roles_dir="$project_dir/.claude/roles"
directives_dir="$roles_dir/directives"
marker_file="$roles_dir/$session_id"
worktree_dir="$project_dir/.claude/worktrees/$session_id"

# (1) Ensure the per-session worktree exists. Idempotent: skip when the path is
# already present, otherwise add it. Never fatal — a failure here must not stop
# the session from starting.
if command -v git >/dev/null 2>&1 \
    && git -C "$project_dir" rev-parse --git-dir >/dev/null 2>&1; then
    if [[ ! -e "$worktree_dir" ]]; then
        git -C "$project_dir" worktree add "$worktree_dir" >/dev/null 2>&1 || true
    fi
fi

# Emit the SessionStart added-context JSON for the text in $1, built safely with
# jq so the directive body is escaped correctly.
emit_context() {
    jq -n --arg ctx "$1" \
        '{hookSpecificOutput: {hookEventName: "SessionStart", additionalContext: $ctx}}'
}

# (2) Read the role marker. On a hit inject the matching directive; on a miss
# inject the ask-the-user-and-write-the-marker instruction.
if [[ -f "$marker_file" ]]; then
    role=$(tr -d '[:space:]' < "$marker_file")
    directive_file="$directives_dir/$role.md"
    if [[ -n "$role" && -f "$directive_file" ]]; then
        directive=$(cat "$directive_file")
        emit_context "$(printf 'This session is bound to the %s role (ADR-0110). Its directive follows.\n\n%s' "$role" "$directive")"
        exit 0
    fi
    # Marker present but empty or naming an unknown role — fall through to the
    # prompt so the user can re-declare it.
fi

prompt=$(cat <<EOF
This session has no role marker yet (ADR-0110 § "Session binding").

Ask the user which of the four roles this session is for, then write the choice
to the session-keyed marker so the binding is durable across restarts:

    printf '%s\n' <role> > "$marker_file"

Roles:
  - dreamer       turn a felt absence into scoped issues
  - scoper        scope-only: walk a Backlog issue Define -> Design -> Plan
  - orchestrator  scoped issues -> merged PRs end-to-end (shardable)
  - everything    no directive, every skill, the ad-hoc escape hatch

Once the marker is written, the matching role directive is injected at the next
session start.
EOF
)

emit_context "$prompt"
exit 0
