#!/usr/bin/env bash
# Pre-flight check for section-divider comments. The user dislikes
# decorative banners like `// ---- foo ----` in source files (per
# auto-memory `feedback_no_divider_comments.md`): module structure
# should come from items + file splits, not ASCII art.
#
# This hook fires on Edit / Write / MultiEdit against source-extension
# files and rejects diffs that ADD a comment line matching
#   ^\s*(//|#)\s*[-=*]{3,}
# (a `//` or `#` comment opener followed by 3+ marker characters).
# Pre-existing dividers in legacy code don't trigger — only new ones
# introduced by the diff. To deliberately add a banner (rare),
# include `// DIVIDER_OK: <reason>` (or `# DIVIDER_OK: <reason>`) in
# the new content and the hook yields.
#
# Reads the tool call JSON from stdin (Claude Code PreToolUse hook
# protocol), exits 2 (block) on match.

set -u

input=$(cat)
tool_name=$(printf '%s' "$input" | jq -r '.tool_name // ""')
file_path=$(printf '%s' "$input" | jq -r '.tool_input.file_path // ""')

case "$file_path" in
    *.rs|*.ts|*.tsx|*.js|*.py|*.sh|*.go|*.c|*.cpp|*.h) ;;
    *) exit 0 ;;
esac

DIVIDER_RE='^[[:space:]]*(\/\/|#)[[:space:]]*[-=*]{3,}'

count_dividers() {
    printf '%s' "$1" | grep -cE "$DIVIDER_RE" || true
}

has_override() {
    printf '%s' "$1" | grep -qE '(\/\/|#) ?DIVIDER_OK:'
}

emit_message() {
    local added=$1
    {
        printf 'no-divider-comments: this change adds %d new section-divider comment line(s).\n' "$added"
        printf '\n'
        printf 'User feedback (auto-memory `feedback_no_divider_comments.md`):\n'
        printf 'do not write decorative banners like `// ---- foo ----` in source\n'
        printf 'files. The items below the banner already say what they are; if a\n'
        printf 'file genuinely needs visual structure, that is a signal to split it\n'
        printf 'into modules, not to add ASCII art.\n'
        printf '\n'
        printf 'If a banner is genuinely the right tool, override by including\n'
        printf '`// DIVIDER_OK: <reason>` (or `# DIVIDER_OK: <reason>`) in the new code.\n'
    } >&2
}

case "$tool_name" in
    Edit)
        old_string=$(printf '%s' "$input" | jq -r '.tool_input.old_string // ""')
        new_string=$(printf '%s' "$input" | jq -r '.tool_input.new_string // ""')
        old_count=$(count_dividers "$old_string")
        new_count=$(count_dividers "$new_string")
        added=$((new_count - old_count))
        if (( added > 0 )); then
            if has_override "$new_string"; then
                exit 0
            fi
            emit_message "$added"
            exit 2
        fi
        ;;
    Write)
        content=$(printf '%s' "$input" | jq -r '.tool_input.content // ""')
        old_count=0
        if [[ -f "$file_path" ]]; then
            old_count=$(count_dividers "$(cat "$file_path")")
        fi
        new_count=$(count_dividers "$content")
        added=$((new_count - old_count))
        if (( added > 0 )); then
            if has_override "$content"; then
                exit 0
            fi
            emit_message "$added"
            exit 2
        fi
        ;;
    MultiEdit)
        edit_count=$(printf '%s' "$input" | jq -r '.tool_input.edits | length // 0')
        total_added=0
        any_override=0
        for ((i=0; i<edit_count; i++)); do
            old_string=$(printf '%s' "$input" | jq -r ".tool_input.edits[$i].old_string // \"\"")
            new_string=$(printf '%s' "$input" | jq -r ".tool_input.edits[$i].new_string // \"\"")
            old_count=$(count_dividers "$old_string")
            new_count=$(count_dividers "$new_string")
            added=$((new_count - old_count))
            if (( added > 0 )); then
                total_added=$((total_added + added))
                if has_override "$new_string"; then
                    any_override=1
                fi
            fi
        done
        if (( total_added > 0 )); then
            if (( any_override == 1 )); then
                exit 0
            fi
            emit_message "$total_added"
            exit 2
        fi
        ;;
esac
exit 0
