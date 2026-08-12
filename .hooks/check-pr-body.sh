#!/usr/bin/env bash
# PreToolUse hook for GitHub PR/issue text published from Bash.

set -u

input=$(cat)

jq_value() {
    local filter="$1"
    if command -v jq >/dev/null 2>&1; then
        printf '%s' "$input" | jq -r "$filter // empty" 2>/dev/null || true
    fi
}

command=$(
    jq_value '.tool_input.command // .tool_input.cmd // .input.command // .input.cmd // .arguments.command // .arguments.cmd // .params.command // .params.cmd // .command // .cmd'
)

case "$command" in
    *"gh pr create"*|*"gh pr edit"*|*"gh issue create"*|*"gh issue edit"*) ;;
    *) exit 0 ;;
esac

is_pr_cmd=0
is_issue_cmd=0
case "$command" in
    *"gh pr create"*|*"gh pr edit"*) is_pr_cmd=1 ;;
esac
if (( is_pr_cmd == 0 )); then
    case "$command" in
        *"gh issue create"*|*"gh issue edit"*) is_issue_cmd=1 ;;
    esac
fi

override_line=$(printf '%s' "$command" | grep -oE 'pr-body-ok:[^\n]*' | head -1 || true)
allowed=""
if [[ -n "$override_line" ]]; then
    rest=${override_line#pr-body-ok:}
    rest=${rest# }
    prefix=${rest%%[[:space:]-]*}
    allowed=$(printf '%s' "$prefix" | tr '[:upper:]' '[:lower:]' | grep -oE '[a-e]' | tr '\n' ',' | sed 's/,$//' || true)
    if [[ -z "$allowed" ]]; then
        printf 'pr-body-ok override needs at least one pattern letter (a/b/c/d/e), e.g. `<!-- pr-body-ok: d - reason -->`\n' >&2
        exit 2
    fi
fi

body_file=$(printf '%s' "$command" | grep -oE -- '--body-file[ =]+[^ ]+' | sed -E 's/^--body-file[ =]+//' | tr -d '"' | tr -d "'")
body_content=""
if [[ -n "$body_file" && -f "$body_file" ]]; then
    body_content=$(cat "$body_file")
fi

body_inline=$(printf '%s' "$command" | grep -oE -- "--body[ =]+(\"[^\"]*\"|'[^']*')" | head -1 || true)
if [[ -n "$body_inline" ]]; then
    body_inline=${body_inline#*--body}
    body_inline=${body_inline# }
    body_inline=${body_inline#=}
    body_inline=${body_inline# }
    body_inline=${body_inline%\'}
    body_inline=${body_inline#\'}
    body_inline=${body_inline%\"}
    body_inline=${body_inline#\"}
fi

body_heredoc=$(printf '%s' "$command" | awk '/<<.*EOF/ { grab=1; next } grab && /^[[:space:]]*EOF[[:space:]]*$/ { grab=0; next } grab { print }')
body_corpus=$(printf '%s\n%s\n%s' "$body_content" "$body_inline" "$body_heredoc")

issues=()

if [[ ",$allowed," != *",a,"* ]] && printf '%s' "$body_corpus" | grep -qE '\\[`$]'; then
    issues+=("Pattern A: backslash-escaped backtick or dollar - drop the backslash; quoted heredocs pass them through literally")
fi

if [[ ",$allowed," != *",d,"* ]] && printf '%s' "$body_corpus" | grep -qE '\$[^ \(0-9][^$]*\$'; then
    issues+=("Pattern D: dollar-delimited text renders as LaTeX math on GitHub - use backticks for inline code")
fi

title_match=$(printf '%s' "$command" | grep -oE -- "--title[ =]+(\"[^\"]*\"|'[^']*')" || true)
title=""
if [[ -n "$title_match" ]]; then
    title=${title_match#*--title}
    title=${title# }
    title=${title#=}
    title=${title# }
    title=${title%\'}
    title=${title#\'}
    title=${title%\"}
    title=${title#\"}
fi

if [[ ",$allowed," != *",c,"* ]] && [[ -n "$title" && "$title" == *:* ]]; then
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

if [[ ",$allowed," != *",e,"* ]] && (( is_issue_cmd == 1 )) && [[ -n "$title" ]]; then
    title_re='^(feat|fix|chore|docs|perf|refactor|flake)\(([a-z0-9-]+)(/[a-z0-9-]+)?\):[[:space:]].+$'
    if [[ "$title" =~ $title_re ]]; then
        scope="${BASH_REMATCH[2]}"
        valid_scope "$scope"
        scope_status=$?
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
