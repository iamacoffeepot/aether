#!/usr/bin/env bash
# Pre-flight check for `gh pr create` / `gh pr edit` / `gh issue
# create` / `gh issue edit` body and title content. Catches the four
# recurring failure modes documented in the user's auto-memory file
# `feedback_heredoc_no_backtick_escape.md`.
#
# Reads the Bash tool call JSON from stdin (Claude Code PreToolUse
# hook protocol), filters to only `gh` commands that publish text,
# runs the regex check, exits 2 (block) on match. Body extraction
# also handles `--body-file PATH` by reading the file directly.
#
# To deliberately submit a body that looks like one of the patterns
# (rare — usually the pattern means the body really is broken),
# include `# pr-body-ok: <reason>` somewhere in the command and the
# hook will yield.

set -u

input=$(cat)
command=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')

case "$command" in
    *"gh pr create"*|*"gh pr edit"*|*"gh issue create"*|*"gh issue edit"*) ;;
    *) exit 0 ;;
esac

if printf '%s' "$command" | grep -qE 'pr-body-ok:'; then
    exit 0
fi

# Body-file content joins the search corpus alongside the command
# itself so heredoc + --body-file paths are both covered.
body_file=$(printf '%s' "$command" | grep -oE -- '--body-file[ =]+[^ ]+' | sed -E 's/^--body-file[ =]+//' | tr -d '"' | tr -d "'")
body_content=""
if [[ -n "$body_file" && -f "$body_file" ]]; then
    body_content=$(cat "$body_file")
fi
search_text="$command"$'\n'"$body_content"

issues=()

# Pattern A: \` or \$ — escaped backticks/dollars are literal in
# quoted heredocs and render as broken text on GitHub. Drop the
# backslash.
if printf '%s' "$search_text" | grep -qE '\\[`$]'; then
    issues+=("Pattern A: backslash-escaped backtick or dollar — drop the backslash; quoted heredocs (<<'EOF') pass them through literally")
fi

# Pattern B: bare #NNN auto-link. GitHub renders any standalone
# `#<digits>` as a cross-ref. Allow `owner/repo#NNN` (preceded by
# `/`); allow ADR-0045-style refs (preceded by an alphanum or `-`).
# Allow occurrences inside obvious URL paths (preceded by digits via
# the `[A-Za-z0-9_/-]` exclusion).
if printf '%s' "$search_text" | grep -qE '(^|[^A-Za-z0-9_/-])#[0-9]+'; then
    issues+=("Pattern B: bare #NNN auto-links — write 'PR 235' instead of '#235', or 'owner/repo#NNN' for cross-repo refs")
fi

# Pattern D: $...$ inline math span. GitHub treats `$foo$` as LaTeX.
# Exclude `$(...)` (shell expansion) and `$ ` (variable form) by
# requiring the byte after the opening `$` to be neither space, paren,
# nor digit.
if printf '%s' "$search_text" | grep -qE '\$[^ \(0-9][^$]*\$'; then
    issues+=("Pattern D: \$...\$ renders as LaTeX math on GitHub — switch inline code to backticks")
fi

# Pattern C: PR title subject must start lowercase. CI runs
# amannn/action-semantic-pull-request with subjectPattern
# `^(?![A-Z]).+$`. Extract --title argument; works for both single
# and double-quoted forms.
title_match=$(printf '%s' "$command" | grep -oE -- "--title[ =]+(\"[^\"]*\"|'[^']*')" || true)
if [[ -n "$title_match" ]]; then
    title=${title_match#*--title}
    title=${title# }
    title=${title#=}
    title=${title%\'}
    title=${title#\'}
    title=${title%\"}
    title=${title#\"}
    if [[ "$title" == *:* ]]; then
        subject=${title#*:}
        subject=${subject# }
        first=${subject:0:1}
        if [[ "$first" =~ [A-Z] ]]; then
            issues+=("Pattern C: PR title subject starts uppercase ('$first') — CI rejects, rephrase ('adr-0045 ...' not 'ADR-0045 ...')")
        fi
    fi
fi

if (( ${#issues[@]} )); then
    {
        printf 'PR/issue text pre-flight failed:\n'
        for i in "${issues[@]}"; do
            printf '  - %s\n' "$i"
        done
        printf '\nSee feedback_heredoc_no_backtick_escape.md (auto-memory) for context.\n'
        printf 'To override deliberately, include `# pr-body-ok: <reason>` in the command.\n'
    } >&2
    exit 2
fi
exit 0
