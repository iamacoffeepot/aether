#!/usr/bin/env bash
# Move issues to the right column on the release board, driven by the issue's
# phase:* label. The label is the source of truth; the board card follows it.
#
# An issue carries a phase:<name> label (set by humans in the UI, or by the
# scope/approve/implement/bounce skills). The board Phase field should always
# show that same column. It drifts because the field is only writable through
# the project API, so a label slapped on in the UI never moves the card. This
# syncs the card to the label.
#
# Rules, in order:
#   issue CLOSED                -> Phase = Done   (Done carries no label)
#   issue OPEN, phase:<x> label -> Phase = <X>    (matched to the board option)
#   issue OPEN, no phase label  -> Phase = Backlog (the resting/default column)
#
# Runs from `.github/workflows/reconcile-board.yml` on issue label/state events
# (one issue) and is runnable by hand for a whole-board backlog pass:
#
#   scripts/reconcile-board.sh --all                # dry-run: print the plan
#   scripts/reconcile-board.sh --all --apply        # actually move the cards
#   scripts/reconcile-board.sh --issue 984 --apply  # one issue
#
# Default is DRY-RUN — nothing moves without --apply. The workflow passes
# --apply (an unattended run has no human to confirm a printed plan).
#
# Owner + project number come from .claude/release-state.json when present
# (local runs), else from $BOARD_OWNER / $BOARD_PROJECT (the workflow sets
# these — they are public, not secret). The project node id and the Phase
# field/option ids are always resolved live from the API, so nothing here goes
# stale and the gitignored release-state.json is not required in CI.
#
# Projects v2 (user-level) writes need a token with project scope; the default
# Actions GITHUB_TOKEN cannot write a user project, so the workflow sets GH_TOKEN
# to a PROJECTS_TOKEN secret. If a write fails for lack of scope the script
# warns once and keeps going rather than aborting.

set -euo pipefail

MODE=""
TARGET=""
APPLY=0
VERBOSE=0

while [[ $# -gt 0 ]]; do
    case "$1" in
        --all) MODE="all"; shift ;;
        --issue) MODE="issue"; TARGET="$2"; shift 2 ;;
        --apply) APPLY=1; shift ;;
        --verbose|-v) VERBOSE=1; shift ;;
        *) echo "unknown arg: $1" >&2; exit 2 ;;
    esac
done

if [[ -z "$MODE" ]]; then
    echo "usage: reconcile-board.sh (--all | --issue N) [--apply] [--verbose]" >&2
    exit 2
fi

log() { [[ $VERBOSE -eq 1 ]] && echo "  · $*" >&2 || true; }

# Owner + project number: local cache first, env fallback for CI.
ROOT="$(git rev-parse --show-toplevel 2>/dev/null || echo .)"
STATE="$ROOT/.claude/release-state.json"
if [[ -f "$STATE" ]]; then
    OWNER=$(jq -r '.owner' "$STATE")
    PROJECT=$(jq -r '.active_project' "$STATE")
else
    OWNER="${BOARD_OWNER:-}"
    PROJECT="${BOARD_PROJECT:-}"
fi
if [[ -z "$OWNER" || -z "$PROJECT" ]]; then
    echo "no owner/project — need .claude/release-state.json or \$BOARD_OWNER + \$BOARD_PROJECT" >&2
    exit 1
fi

# Phase field + options, resolved live (never stale). Address the project as
# "@me" (the authenticated user) rather than by owner login: `gh project
# --owner <login>` resolves the owner by querying both user(login) and
# organization(login), and on some gh versions the org half's error for a User
# login poisons the response into "unknown owner type". "@me" routes through
# `viewer`, skipping that lookup. (Assumes the board is owned by the token's
# user — true for this release board; an org-owned board would use its login.)
# Surface gh's own error rather than dying mutely under set -e on failure.
if ! proj_json=$(gh project view "$PROJECT" --owner "@me" --format json 2>&1); then
    echo "cannot read project $PROJECT as @me — token lacks project scope, or @me is not the board owner ($OWNER)." >&2
    echo "  gh: $proj_json" >&2
    exit 1
fi
PROJECT_NODE=$(echo "$proj_json" | jq -r '.id // empty')
if ! FIELDS_JSON=$(gh project field-list "$PROJECT" --owner "@me" --format json 2>&1); then
    echo "cannot list project $PROJECT fields — token scope? gh: $FIELDS_JSON" >&2
    exit 1
fi
PHASE_FIELD=$(echo "$FIELDS_JSON" | jq -r '.fields[]? | select(.name=="Phase") | .id' | head -1)
if [[ -z "$PROJECT_NODE" || -z "$PHASE_FIELD" ]]; then
    echo "resolved no project node / Phase field on project $PROJECT (owner $OWNER)" >&2
    exit 1
fi

phase_option_id() {
    echo "$FIELDS_JSON" | jq -r --arg n "$1" \
        '.fields[]? | select(.name=="Phase") | .options[]? | select(.name==$n) | .id' | head -1
}

# Map a phase:<x> label suffix to the canonical board option name, matched
# case-insensitively (phase:ready -> "Ready"). Empty if not a real option.
label_to_phase() {
    echo "$FIELDS_JSON" | jq -r --arg x "$1" \
        '.fields[]? | select(.name=="Phase") | .options[]? | .name
         | select(ascii_downcase == ($x | ascii_downcase))' | head -1
}

# Board item cache as a num<TAB>id<TAB>phase file (bash 3.2 has no associative
# arrays, and the default macOS bash is 3.2).
BOARD_TSV="$(mktemp)"
trap 'rm -f "$BOARD_TSV"' EXIT
load_board() {
    gh project item-list "$PROJECT" --owner "@me" --format json --limit 500 2>/dev/null \
        | jq -r '.items[] | select(.content.number != null)
                 | [.content.number, .id, (.phase // "")] | @tsv' > "$BOARD_TSV" || {
        echo "warning: could not list project $PROJECT items — is GH_TOKEN scoped for projects?" >&2
    }
}
board_item_id() { awk -F'\t' -v n="$1" '$1==n{print $2; exit}' "$BOARD_TSV"; }
board_phase()   { awk -F'\t' -v n="$1" '$1==n{print $3; exit}' "$BOARD_TSV"; }

plan_lines=()
applied=0
warned=0

# Target column for one issue, from its state + phase:* label.
target_phase_for() {
    local num="$1" data st label_suffix
    data=$(gh issue view "$num" --json state,labels 2>/dev/null) || { echo ""; return 0; }
    st=$(echo "$data" | jq -r '.state')
    if [[ "$st" == "CLOSED" ]]; then
        echo "Done"; return 0
    fi
    label_suffix=$(echo "$data" | jq -r '.labels[].name | select(startswith("phase:")) | sub("^phase:"; "")' | head -1)
    if [[ -z "$label_suffix" ]]; then
        echo "Backlog"; return 0
    fi
    label_to_phase "$label_suffix"
}

reconcile_issue() {
    local num="$1" target cur item
    # Only move cards that exist. An issue not on the board is a "should it be
    # tracked" question, not a "move the card" one — out of scope here.
    item="$(board_item_id "$num")"
    if [[ -z "$item" ]]; then
        log "#$num not on board — skipping"
        return 0
    fi
    target=$(target_phase_for "$num")
    cur="$(board_phase "$num")"; cur="${cur:-<none>}"
    if [[ -z "$target" ]]; then
        log "#$num — unrecognized phase label, leaving at $cur"
        return 0
    fi
    log "#$num board=$cur target=$target"
    [[ "$target" == "$cur" ]] && return 0

    plan_lines+=("  #$num  Phase $cur → $target")

    if [[ $APPLY -eq 1 ]]; then
        local opt
        opt=$(phase_option_id "$target")
        if [[ -z "$opt" ]]; then
            echo "warning: no option id for phase '$target'" >&2
            return 0
        fi
        if gh project item-edit --id "$item" --project-id "$PROJECT_NODE" \
             --field-id "$PHASE_FIELD" --single-select-option-id "$opt" >/dev/null 2>&1; then
            applied=$((applied+1))
        elif [[ $warned -eq 0 ]]; then
            echo "warning: project field write failed (token missing 'project' scope?) — cards left unmoved" >&2
            warned=1
        fi
    fi
}

load_board

case "$MODE" in
    issue) reconcile_issue "$TARGET" ;;
    all)
        # Every card on the board, synced to its issue's label/state.
        for n in $(cut -f1 "$BOARD_TSV" | sort -un); do
            [[ -n "$n" ]] && reconcile_issue "$n"
        done
        ;;
esac

echo
if [[ ${#plan_lines[@]} -eq 0 ]]; then
    echo "board-sync: nothing drifted — every card matches its issue's phase label."
elif [[ $APPLY -eq 1 ]]; then
    echo "board-sync: moved ${applied} card(s):"
    printf '%s\n' "${plan_lines[@]}"
else
    echo "board-sync (DRY-RUN — re-run with --apply): ${#plan_lines[@]} card(s) to move:"
    printf '%s\n' "${plan_lines[@]}"
fi
