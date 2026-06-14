#!/usr/bin/env bash
# Status line command — render the session's bound role as a colored label.
#
# Claude Code pipes a JSON object to stdin on each status refresh; this script
# extracts the session_id field, reads the session-keyed role marker the
# SessionStart hook writes, and prints a single ANSI-colored line naming the
# role (ADR-0110 § "Status line").
#
# Four roles, four fixed colors:
#   dreamer      — blue
#   scoper        — yellow
#   orchestrator  — green
#   everything    — magenta
#
# An absent or empty marker prints a dim neutral placeholder rather than
# failing, so a session that has not yet written its marker still renders
# cleanly.

set -u

input=$(cat)

project_dir="${CLAUDE_PROJECT_DIR:-}"
if [[ -z "$project_dir" ]]; then
    script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
    project_dir=$(cd "$script_dir/.." && pwd)
fi

session_id=$(printf '%s' "$input" | jq -r '.session_id // ""')

RESET='\033[0m'
DIM='\033[2m'

if [[ -z "$session_id" ]]; then
    printf "${DIM}no role${RESET}\n"
    exit 0
fi

marker_file="$project_dir/.claude/roles/$session_id"

if [[ ! -f "$marker_file" ]]; then
    printf "${DIM}no role${RESET}\n"
    exit 0
fi

role=$(tr -d '[:space:]' < "$marker_file")

if [[ -z "$role" ]]; then
    printf "${DIM}no role${RESET}\n"
    exit 0
fi

case "$role" in
    dreamer)
        COLOR='\033[34m'
        ;;
    scoper)
        COLOR='\033[33m'
        ;;
    orchestrator)
        COLOR='\033[32m'
        ;;
    everything)
        COLOR='\033[35m'
        ;;
    *)
        printf "${DIM}no role${RESET}\n"
        exit 0
        ;;
esac

printf "${COLOR}${role}${RESET}\n"
