#!/usr/bin/env bash
# Pre-flight check for `gh pr create` / `gh pr edit` / `gh issue
# create` / `gh issue edit` body and title content. Catches the four
# recurring failure modes documented in the user's auto-memory file
# `feedback_heredoc_no_backtick_escape.md`. (Pattern B — bare #NNN in
# PR bodies — was retired: it guarded against a body-stripping hook
# that does not exist; bare #NNN auto-links fine on GitHub.)
#
# Reads the Bash tool call JSON from stdin (Claude Code PreToolUse
# hook protocol), filters to only `gh` commands that publish text,
# runs the regex check, exits 2 (block) on match. Body extraction
# also handles `--body-file PATH` by reading the file directly.
#
# To deliberately submit a body that looks like one of the patterns
# (rare — usually the pattern means the body really is broken),
# include `<!-- pr-body-ok: <letters> — <reason> -->` in the command.
# The letter list is one or more of a/c/d/e (comma-separated, e.g.
# `<!-- pr-body-ok: a,d — reason -->`); only listed patterns are
# skipped, unlisted ones still fire. A bare `pr-body-ok:` with no
# letter list is rejected to force the author to think about which
# check they're overriding. The legacy `# pr-body-ok: ...` form still
# matches (the regex grabs `pr-body-ok:[^\n]*` regardless of leading
# punctuation), but prefer the HTML-comment form because a `#` at line
# start renders as an H1 heading on GitHub.

set -u

input=$(cat)
command=$(printf '%s' "$input" | jq -r '.tool_input.command // ""')

case "$command" in
    *"gh pr create"*|*"gh pr edit"*|*"gh issue create"*|*"gh issue edit"*) ;;
    *) exit 0 ;;
esac

# Distinguish PR vs issue commands. Body content (heredoc text,
# commit messages, examples) often mentions both `gh pr create` and
# `gh issue create` literally, so substring matching can find both
# in a single Bash invocation. Heuristic: if the command contains
# any `gh pr (create|edit)`, treat it as a PR command — Pattern E
# (issue-title check) only fires when NO PR call is present. The
# rare hybrid (one Bash that publishes both a PR and an issue) is
# treated as PR; intentionally publishing an issue from inside a
# PR-creation flow can override via `<!-- pr-body-ok: e — ... -->`.
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

# Parse the override line. `# pr-body-ok: <letters> — <reason>` skips
# only the listed patterns; bare `# pr-body-ok:` is rejected. Letters
# are extracted individually, so `b,d`, `bd`, and `b d` all parse to
# the same allow-list.
override_line=$(printf '%s' "$command" | grep -oE 'pr-body-ok:[^\n]*' | head -1 || true)
allowed=""
if [[ -n "$override_line" ]]; then
    rest=${override_line#pr-body-ok:}
    rest=${rest# }
    prefix=${rest%%[[:space:]—]*}
    allowed=$(printf '%s' "$prefix" | tr '[:upper:]' '[:lower:]' | grep -oE '[a-e]' | tr '\n' ',' | sed 's/,$//' || true)
    if [[ -z "$allowed" ]]; then
        printf 'pr-body-ok override needs at least one pattern letter (a/b/c/d/e), e.g. `<!-- pr-body-ok: b — reason -->`\n' >&2
        exit 2
    fi
fi

# Patterns A and D scan only the extracted BODY, never the surrounding
# command. A body assembled from shell plumbing — a $(gh issue view …)
# capture, a piped "$new", a perl $1 backref — carries unrelated
# dollar-expansions that read as a `$…$` math span when the raw command
# is scanned, even though the published body is clean. The body is what
# publishes, so it is the only corpus. Three sources, one per place a
# body can come from:
#   --body-file PATH      → file contents
#   --body '…' / "…"      → the inline literal
#   <<'EOF' … EOF heredoc → the heredoc text (common EOF delimiter)
body_file=$(printf '%s' "$command" | grep -oE -- '--body-file[ =]+[^ ]+' | sed -E 's/^--body-file[ =]+//' | tr -d '"' | tr -d "'")
body_content=""
if [[ -n "$body_file" && -f "$body_file" ]]; then
    body_content=$(cat "$body_file")
fi

# Inline --body literal (single- or double-quoted), extracted the same
# way --title is below. The `--body[ =]` class never matches
# `--body-file` (that is followed by `-`, not a space or `=`).
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

# Heredoc body text between `<<EOF` / `<<'EOF'` and a closing `EOF`.
# Only the common EOF delimiter is recognised — a general delimiter
# parser is not worth the bash, and a non-EOF heredoc simply falls back
# to the file / inline sources rather than being scanned.
body_heredoc=$(printf '%s' "$command" | awk '/<<.*EOF/ { grab=1; next } grab && /^[[:space:]]*EOF[[:space:]]*$/ { grab=0; next } grab { print }')

# The Pattern A/D corpus: the three body sources joined, never the
# command. Title checks (C/E) keep using $command's --title below.
body_corpus=$(printf '%s\n%s\n%s' "$body_content" "$body_inline" "$body_heredoc")

issues=()

# Pattern A: \` or \$ — escaped backticks/dollars are literal in
# quoted heredocs and render as broken text on GitHub. Drop the
# backslash.
if [[ ",$allowed," != *",a,"* ]] && printf '%s' "$body_corpus" | grep -qE '\\[`$]'; then
    issues+=("Pattern A: backslash-escaped backtick or dollar — drop the backslash; quoted heredocs (<<'EOF') pass them through literally")
fi

# Pattern D: $...$ inline math span. GitHub treats `$foo$` as LaTeX.
# Exclude `$(...)` (shell expansion) and `$ ` (variable form) by
# requiring the byte after the opening `$` to be neither space, paren,
# nor digit.
if [[ ",$allowed," != *",d,"* ]] && printf '%s' "$body_corpus" | grep -qE '\$[^ \(0-9][^$]*\$'; then
    issues+=("Pattern D: \$...\$ renders as LaTeX math on GitHub — switch inline code to backticks")
fi

# Extract --title argument once; used by Pattern C (lowercase subject)
# and Pattern E (issue-only title format). Works for both single and
# double-quoted forms.
title_match=$(printf '%s' "$command" | grep -oE -- "--title[ =]+(\"[^\"]*\"|'[^']*')" || true)
title=""
if [[ -n "$title_match" ]]; then
    title=${title_match#*--title}
    title=${title# }
    title=${title#=}
    title=${title%\'}
    title=${title#\'}
    title=${title%\"}
    title=${title#\"}
fi

# Pattern C: PR title subject must start lowercase. CI runs
# amannn/action-semantic-pull-request with subjectPattern
# `^(?![A-Z]).+$`.
if [[ ",$allowed," != *",c,"* ]] && [[ -n "$title" && "$title" == *:* ]]; then
    subject=${title#*:}
    subject=${subject# }
    first=${subject:0:1}
    if [[ "$first" =~ [A-Z] ]]; then
        issues+=("Pattern C: PR title subject starts uppercase ('$first') — CI rejects, rephrase ('adr-0045 ...' not 'ADR-0045 ...')")
    fi
fi

# Pattern E: issue title must match `{type}({crate}): subject` (or
# `{type}({crate}/{subfeat}): subject` for subfeatures). Scope must be
# a registered `crate:*` label OR a meta-scope. Mirrors the server-side
# workflow at `.github/workflows/issue-labels.yml` (keep META_SCOPES in
# sync). Fires only on `gh issue create` / `gh issue edit`.
if [[ ",$allowed," != *",e,"* ]] && (( is_issue_cmd == 1 )) && [[ -n "$title" ]]; then
    title_re='^(feat|fix|chore|docs|perf|refactor|flake)\(([a-z0-9-]+)(/[a-z0-9-]+)?\):[[:space:]].+$'
    # Meta-scopes: cross-cutting work that isn't a single crate. Must
    # match META_SCOPES in .github/workflows/issue-labels.yml — the
    # server accepts these, so the local hook must too or it
    # false-positives on a title the server would pass.
    meta_scopes=" ci docs adr qodana repo release workflow guide "
    if [[ "$title" =~ $title_re ]]; then
        scope="${BASH_REMATCH[2]}"
        if [[ "$meta_scopes" != *" $scope "* ]] \
            && ! gh label list --search "crate:$scope" --json name --jq '.[].name' 2>/dev/null | grep -qx "crate:$scope"; then
            valid_crates=$(gh label list --limit 100 --json name --jq '.[].name | select(startswith("crate:")) | sub("^crate:"; "")' 2>/dev/null | sort | tr '\n' ' ' | sed 's/ $//')
            issues+=("Pattern E: issue title scope '$scope' is not a known crate or meta-scope. Valid crates: $valid_crates. Valid meta-scopes:${meta_scopes}")
        fi
    else
        issues+=("Pattern E: issue title must match {type}({crate}): subject (subfeatures via {type}({crate}/{subfeat}): subject). Allowed types: feat, fix, chore, docs, perf, refactor, flake")
    fi
fi

if (( ${#issues[@]} )); then
    {
        printf 'PR/issue text pre-flight failed:\n'
        for i in "${issues[@]}"; do
            printf '  - %s\n' "$i"
        done
        printf '\nRules reference (so the next attempt is right, not another guess):\n'
        printf '  - Issue title: {type}({scope}): <subject>. Types: feat fix chore docs perf refactor flake.\n'
        printf '  - PR title: same {type}({scope}): <subject> shape; PR types additionally allow test build ci style revert.\n'
        printf '  - Subject (issue + PR) must start lowercase.\n'
        printf '  - Scope is a crate name OR a meta-scope: ci docs adr qodana repo release workflow guide.\n'
        printf '  - Body: no backslash before a backtick/dollar (A); no dollar-delimited math span, use backticks (D).\n'
        printf '\nTo override deliberately, include `<!-- pr-body-ok: <letters> — <reason> -->` (letters: a/c/d/e, comma-separated; only listed patterns are skipped).\n'
        printf 'Context: feedback_heredoc_no_backtick_escape.md (auto-memory).\n'
    } >&2
    exit 2
fi
exit 0
