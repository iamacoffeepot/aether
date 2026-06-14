#!/usr/bin/env bash
# Status line command — render the session's bound role as a pastel pill badge.
#
# Claude Code pipes a JSON object to stdin on each status refresh; this script
# extracts the session_id field, reads the session-keyed role marker the
# SessionStart hook writes, and prints a single ANSI badge naming the role
# (ADR-0110 § "Status line").
#
# Four roles, four pastel fills (truecolor, Catppuccin Mocha) with dark ink:
#   dreamer       — blue   (137,180,250)
#   scoper        — yellow (249,226,175)
#   orchestrator  — green  (166,227,161)
#   everything    — mauve  (203,166,247)
#
# The badge is a rectangular fill: the role name in dark text on the pastel
# fill, no end-caps, so it renders in any font. Set
# AETHER_STATUSLINE_BADGE=rounded for a pill with powerline half-circle end-caps
# (U+E0B6 / U+E0B4) instead — those glyphs need a Nerd Font.
#
# Stays bash 3.2 compatible (the macOS system bash): printf octal escapes only,
# no \u unicode escapes (unsupported there), so the cap glyphs are spelled as
# their UTF-8 bytes.
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

print_none() {
    printf "${DIM}no role${RESET}\n"
}

if [[ -z "$session_id" ]]; then
    print_none
    exit 0
fi

marker_file="$project_dir/.claude/roles/$session_id"

if [[ ! -f "$marker_file" ]]; then
    print_none
    exit 0
fi

role=$(tr -d '[:space:]' < "$marker_file")

if [[ -z "$role" ]]; then
    print_none
    exit 0
fi

# Pastel fill per role, as a truecolor "R;G;B" triple.
case "$role" in
    dreamer)
        pastel='137;180;250'
        ;;
    scoper)
        pastel='249;226;175'
        ;;
    orchestrator)
        pastel='166;227;161'
        ;;
    everything)
        pastel='203;166;247'
        ;;
    *)
        print_none
        exit 0
        ;;
esac

ink='30;30;46'  # dark text on the pastel fill (Catppuccin base)

fg="\033[38;2;${pastel}m"
bg="\033[48;2;${pastel}m"
ink_fg="\033[38;2;${ink}m"

# Default: a rectangular fill, no end-caps — renders in any font.
# AETHER_STATUSLINE_BADGE=rounded adds powerline half-circle caps (a Nerd Font
# only): \356\202\266 = U+E0B6 (left), \356\202\264 = U+E0B4 (right).
if [[ "${AETHER_STATUSLINE_BADGE:-block}" == "rounded" ]]; then
    printf "${fg}\356\202\266${bg}${ink_fg} %s ${RESET}${fg}\356\202\264${RESET}\n" "$role"
else
    printf "${bg}${ink_fg} %s ${RESET}\n" "$role"
fi
