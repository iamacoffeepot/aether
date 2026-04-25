#!/usr/bin/env bash
# Pre-flight check for new host-fn additions. The substrate's host-fn
# surface is privileged (ADR-0002): every new fn becomes reachable by
# every component that links against this surface. Most new substrate
# capabilities should land as a mail sink instead — see
# `crates/aether-substrate-core/src/io.rs` and `net.rs` for the
# precedent: substrate-owned sink + paired request/result kinds in
# `aether-kinds` + SDK wraps via `send_postcard` + `wait_reply`.
#
# This hook fires on Edit/Write to `host_fns.rs` and rejects diffs
# that add a `linker.func_wrap(` call. To deliberately add a host fn
# (e.g. for a wasmtime-specific capability that genuinely needs FFI),
# include `// HOST_FN_OK: <reason>` adjacent to the new
# `linker.func_wrap` and the hook will yield.
#
# Reads the Edit/Write tool call JSON from stdin (Claude Code
# PreToolUse hook protocol), exits 2 (block) on match.

set -u

input=$(cat)
tool_name=$(printf '%s' "$input" | jq -r '.tool_name // ""')
file_path=$(printf '%s' "$input" | jq -r '.tool_input.file_path // ""')

case "$file_path" in
    */host_fns.rs) ;;
    *) exit 0 ;;
esac

count_funcs() {
    printf '%s' "$1" | grep -cE 'linker\.func_wrap\(' || true
}

emit_message() {
    local added=$1
    {
        printf 'host_fns.rs: this change adds %d new linker.func_wrap call(s).\n' "$added"
        printf '\n'
        printf 'Adding a host fn is a deliberate capability decision (ADR-0002).\n'
        printf 'Most new substrate capabilities should land as a mail sink instead.\n'
        printf 'See crates/aether-substrate-core/src/io.rs and net.rs for the\n'
        printf 'precedent: sink owns the resource, paired request/result kinds in\n'
        printf 'aether-kinds, SDK wraps via send_postcard + wait_reply for sync.\n'
        printf '\n'
        printf 'If a host fn is genuinely the right tool, override by including\n'
        printf '`// HOST_FN_OK: <reason>` in the new code.\n'
    } >&2
}

case "$tool_name" in
    Edit)
        old_string=$(printf '%s' "$input" | jq -r '.tool_input.old_string // ""')
        new_string=$(printf '%s' "$input" | jq -r '.tool_input.new_string // ""')
        old_count=$(count_funcs "$old_string")
        new_count=$(count_funcs "$new_string")
        added=$((new_count - old_count))
        if (( added > 0 )); then
            if printf '%s' "$new_string" | grep -qE 'HOST_FN_OK:'; then
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
            old_count=$(count_funcs "$(cat "$file_path")")
        fi
        new_count=$(count_funcs "$content")
        added=$((new_count - old_count))
        if (( added > 0 )); then
            if printf '%s' "$content" | grep -qE 'HOST_FN_OK:'; then
                exit 0
            fi
            emit_message "$added"
            exit 2
        fi
        ;;
esac
exit 0
