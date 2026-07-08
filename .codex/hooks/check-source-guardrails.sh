#!/usr/bin/env bash
# PostToolUse hook for source-level guardrails that can be read from git diff.

set -u

root=$(git rev-parse --show-toplevel 2>/dev/null || true)
[[ -n "$root" ]] || exit 0

tmp=$(mktemp "${TMPDIR:-/tmp}/aether-codex-guardrails.XXXXXX")
cleanup() { rm -f "$tmp"; }
trap cleanup EXIT

issues=()

source_file() {
    case "$1" in
        *.rs|*.ts|*.tsx|*.js|*.py|*.sh|*.go|*.c|*.cpp|*.h) return 0 ;;
        *) return 1 ;;
    esac
}

check_untracked_file() {
    local file="$1"
    local path="$root/$file"

    source_file "$file" || return 0
    [[ -f "$path" ]] || return 0

    if awk '
        /^[[:space:]]*(\/\/|#)[[:space:]]*[-=*]{3,}/ && !/(\/\/|#) ?DIVIDER_OK:/ {
            found = 1
        }
        END { exit found ? 0 : 1 }
    ' "$path"; then
        issues+=("no-divider-comments: untracked file $file contains section-divider comment lines; use normal comments or split the file")
    fi

    if [[ "$file" == */host_fns.rs || "$file" == "host_fns.rs" ]]; then
        if awk '
            /HOST_FN_OK:/ { override = 1 }
            /linker\.func_wrap\(/ { added = 1 }
            END { exit (added && !override) ? 0 : 1 }
        ' "$path"; then
            issues+=("host_fns.rs: untracked file $file adds linker.func_wrap without HOST_FN_OK; most new capabilities should land as mail sinks")
        fi
    fi
}

check_diff() {
    local diff="$1"
    [[ -n "$diff" ]] || return 0

    printf '%s' "$diff" > "$tmp"

    if awk '
        /^\+\+\+ b\// {
            file = substr($0, 7)
            next
        }
        /^\+[^+]/ {
            line = substr($0, 2)
            if (file ~ /\.(rs|ts|tsx|js|py|sh|go|c|cpp|h)$/ && line ~ /^[[:space:]]*(\/\/|#)[[:space:]]*[-=*]{3,}/ && line !~ /(\/\/|#) ?DIVIDER_OK:/) {
                found = 1
            }
        }
        END { exit found ? 0 : 1 }
    ' "$tmp"; then
        issues+=("no-divider-comments: this diff adds section-divider comment lines; use normal comments or split the file")
    fi

    while IFS= read -r file; do
        source_file "$file" || continue
        [[ "$file" == */host_fns.rs || "$file" == "host_fns.rs" ]] || continue
        if awk -v target="$file" '
            /^\+\+\+ b\// {
                file = substr($0, 7)
                in_file = (file == target)
                next
            }
            in_file && /^\+[^+]/ {
                line = substr($0, 2)
                if (line ~ /HOST_FN_OK:/) {
                    override = 1
                }
                if (line ~ /linker\.func_wrap\(/) {
                    added = 1
                }
            }
            END { exit (added && !override) ? 0 : 1 }
        ' "$tmp"; then
            issues+=("host_fns.rs: this diff adds linker.func_wrap without HOST_FN_OK; most new capabilities should land as mail sinks")
        fi
    done < <(awk '/^\+\+\+ b\// { print substr($0, 7) }' "$tmp" | sort -u)
}

unstaged=$(git -C "$root" diff --no-ext-diff --unified=0 -- . 2>/dev/null || true)
staged=$(git -C "$root" diff --cached --no-ext-diff --unified=0 -- . 2>/dev/null || true)
check_diff "$(printf '%s\n%s\n' "$unstaged" "$staged")"

while IFS= read -r file; do
    check_untracked_file "$file"
done < <(git -C "$root" ls-files --others --exclude-standard 2>/dev/null || true)

if (( ${#issues[@]} )); then
    {
        printf 'Source guardrail check failed:\n'
        for issue in "${issues[@]}"; do
            printf '  - %s\n' "$issue"
        done
    } >&2
    exit 2
fi

exit 0
