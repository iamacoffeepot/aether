#!/usr/bin/env bash
# PreToolUse hook for GitHub PR/issue text published from Bash.
#
# Refuses a `gh pr|issue create|edit` before it runs when its text is of a kind
# GitHub mangles or CI rejects. Each check has a letter:
#
#   A  a backslash before a backtick or a dollar in the body
#   C  a title subject that starts with a capital letter (`Lint title` rejects it)
#   D  a dollar-delimited span in the body, which GitHub renders as math
#   E  an issue title outside `{type}({scope}): subject`, or with an unknown scope
#
# `<!-- pr-body-ok: <letters> - <reason> -->` anywhere in the text waives the
# checks it names.

set -u

input=$(cat)

command=""
if command -v jq >/dev/null 2>&1; then
    command=$(
        printf '%s' "$input" | jq -r '.tool_input.command // .tool_input.cmd // .input.command // .input.cmd // .arguments.command // .arguments.cmd // .params.command // .params.cmd // .command // .cmd // empty' 2>/dev/null || true
    )
fi

case "$command" in
    *"gh pr create"*|*"gh pr edit"*) publishes=pr ;;
    *"gh issue create"*|*"gh issue edit"*) publishes=issue ;;
    *) exit 0 ;;
esac

# The quoted value of the first `--<flag>` in the command, without its quotes.
flag_value() {
    local flag="$1" value
    value=$(printf '%s' "$command" | grep -oE -- "--$flag[ =]+(\"[^\"]*\"|'[^']*')" | head -1 || true)
    value=${value#*--"$flag"}
    value=${value# }
    value=${value#=}
    value=${value# }
    value=${value%\'}
    value=${value#\'}
    value=${value%\"}
    value=${value#\"}
    printf '%s' "$value"
}

waived=""
override_line=$(printf '%s' "$command" | grep -oE 'pr-body-ok:[^\n]*' | head -1 || true)
if [[ -n "$override_line" ]]; then
    letters=${override_line#pr-body-ok:}
    letters=${letters# }
    letters=${letters%%[[:space:]-]*}
    waived=$(printf '%s' "$letters" | tr '[:upper:]' '[:lower:]' | grep -oE '[acde]' | tr -d '\n' || true)
    if [[ -z "$waived" ]]; then
        printf 'pr-body-ok override needs at least one pattern letter (a/c/d/e), e.g. `<!-- pr-body-ok: d - reason -->`\n' >&2
        exit 2
    fi
fi

checks() {
    [[ "$waived" != *"$1"* ]]
}

# The body is every place the command can carry one: a `--body-file` on disk,
# an inline `--body`, and the lines of an `EOF` heredoc.
body_file=$(printf '%s' "$command" | grep -oE -- '--body-file[ =]+[^ ]+' | sed -E 's/^--body-file[ =]+//' | tr -d '"' | tr -d "'")
body_from_file=""
if [[ -n "$body_file" && -f "$body_file" ]]; then
    body_from_file=$(cat "$body_file")
fi
body_heredoc=$(printf '%s' "$command" | awk '/<<.*EOF/ { grab=1; next } grab && /^[[:space:]]*EOF[[:space:]]*$/ { grab=0; next } grab { print }')
body=$(printf '%s\n%s\n%s' "$body_from_file" "$(flag_value body)" "$body_heredoc")

title=$(flag_value title)

issues=()

if checks a && printf '%s' "$body" | grep -qE '\\[`$]'; then
    issues+=("Pattern A: backslash-escaped backtick or dollar - drop the backslash; quoted heredocs pass them through literally")
fi

if checks d && printf '%s' "$body" | grep -qE '\$[^ \(0-9][^$]*\$'; then
    issues+=("Pattern D: dollar-delimited text renders as LaTeX math on GitHub - use backticks for inline code")
fi

if checks c && [[ "$title" == *:* ]]; then
    subject=${title#*:}
    subject=${subject# }
    first=${subject:0:1}
    if [[ "$first" =~ [A-Z] ]]; then
        issues+=("Pattern C: PR/issue title subject starts uppercase ('$first') - CI rejects it")
    fi
fi

valid_scope() {
    local scope="$1"
    local root
    root=$(git rev-parse --show-toplevel 2>/dev/null || pwd)

    case " ci docs adr repo release workflow guide " in
        *" $scope "*) return 0 ;;
    esac

    "$root/scripts/issue-title-scopes.sh" --check "$scope"
}

# The scope list is aether's own crates, so an issue filed on another repository
# (`-R` / `--repo` naming it, as for a nested research repo) keeps the title-shape
# check but skips scope membership.
target_repo=$(printf '%s' "$command" | grep -oE -- '(-R|--repo)[ =]+[^ ]+' | head -1 | sed -E 's/^(-R|--repo)[ =]+//' | tr -d "\"'")
targets_aether=1
if [[ -n "$target_repo" && "$target_repo" != "iamacoffeepot/aether" ]]; then
    targets_aether=0
fi

if checks e && [[ "$publishes" == issue && -n "$title" ]]; then
    title_re='^(feat|fix|chore|docs|perf|refactor|flake)\(([a-z0-9-]+)(/[a-z0-9-]+)?\):[[:space:]].+$'
    if [[ "$title" =~ $title_re ]]; then
        scope="${BASH_REMATCH[2]}"
        scope_status=0
        if (( targets_aether == 1 )); then
            valid_scope "$scope"
            scope_status=$?
        fi
        case "$scope_status" in
            0) ;;
            1) issues+=("Pattern E: issue title scope '$scope' is not a known crate or meta-scope") ;;
            *) issues+=("Pattern E: unable to validate issue title scope '$scope' from Cargo metadata") ;;
        esac
    else
        issues+=("Pattern E: issue title must match {type}({crate}): subject; allowed types: feat, fix, chore, docs, perf, refactor, flake")
    fi
fi

if (( ${#issues[@]} )); then
    {
        printf 'PR/issue text pre-flight failed:\n'
        for issue in "${issues[@]}"; do
            printf '  - %s\n' "$issue"
        done
        printf '\nRules reference:\n'
        printf '  - Issue title: {type}({scope}): <subject>. Types: feat fix chore docs perf refactor flake.\n'
        printf '  - PR title: conventional-commit shape with a lowercase subject.\n'
        printf '  - Scope is a crate name OR a meta-scope: ci docs adr repo release workflow guide.\n'
        printf '  - Body: no backslash before a backtick/dollar; no dollar-delimited math span.\n'
        printf '\nTo override deliberately, include `<!-- pr-body-ok: <letters> - <reason> -->` (letters: a/c/d/e).\n'
    } >&2
    exit 2
fi

exit 0
