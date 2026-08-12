#!/usr/bin/env bash
# Resolve canonical issue-title crate scopes from workspace package metadata.

set -o pipefail
set -u

usage() {
    printf 'usage: %s [--check <scope> | --self-test]\n' "${0##*/}" >&2
}

repository_root() {
    cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd
}

metadata_command() {
    (
        cd -- "$(repository_root)"
        cargo metadata --no-deps --locked --format-version 1
    )
}

scopes_from_metadata() {
    jq -er '
        .workspace_members as $members
        | .packages[]
        | select(.id as $id | $members | index($id))
        | .name
        | sub("^aether-"; "")
    ' | LC_ALL=C sort -u
}

list_scopes() {
    local metadata

    if ! command -v jq >/dev/null 2>&1; then
        printf 'issue title scopes: jq is required to read cargo metadata\n' >&2
        return 2
    fi

    if ! metadata=$(metadata_command); then
        printf 'issue title scopes: cargo metadata could not be resolved\n' >&2
        return 2
    fi

    if ! printf '%s\n' "$metadata" | scopes_from_metadata; then
        printf 'issue title scopes: cargo metadata did not contain usable workspace package names\n' >&2
        return 2
    fi
}

scope_is_known() {
    local scope="$1"
    local scopes="$2"

    grep -Fqx -- "$scope" <<<"$scopes"
}

check_scope() {
    local scope="$1"
    local scopes

    if ! scopes=$(list_scopes); then
        return 2
    fi

    if scope_is_known "$scope" "$scopes"; then
        return 0
    fi

    return 1
}

assert_equal() {
    local expected="$1"
    local actual="$2"
    local description="$3"

    if [[ "$expected" != "$actual" ]]; then
        printf 'self-test failed: %s\nexpected:\n%s\nactual:\n%s\n' "$description" "$expected" "$actual" >&2
        return 1
    fi
}

self_test() {
    local fixture actual status
    fixture='{"packages":[{"id":"actor","name":"aether-actor"},{"id":"prefixed","name":"aether-aether-actor"},{"id":"xtask","name":"xtask"},{"id":"duplicate","name":"aether-actor"},{"id":"dependency","name":"aether-dependency"}],"workspace_members":["actor","prefixed","xtask","duplicate"]}'

    if ! actual=$(printf '%s\n' "$fixture" | scopes_from_metadata); then
        printf 'self-test failed: fixture metadata could not be parsed\n' >&2
        return 1
    fi
    assert_equal $'actor\naether-actor\nxtask' "$actual" 'normalizes canonical names, keeps xtask, and removes duplicates' || return 1

    scope_is_known 'unknown' "$actual"
    status=$?
    if [[ "$status" -ne 1 ]]; then
        printf 'self-test failed: unknown scope returned %s, expected 1\n' "$status" >&2
        return 1
    fi

    metadata_command() { return 1; }
    list_scopes >/dev/null 2>&1
    status=$?
    if [[ "$status" -ne 2 ]]; then
        printf 'self-test failed: metadata command failure returned %s, expected 2\n' "$status" >&2
        return 1
    fi

    printf 'issue title scopes self-test passed\n'
}

case "${1-}" in
    '') list_scopes ;;
    --check)
        if [[ $# -ne 2 || -z "$2" ]]; then
            usage
            exit 2
        fi
        check_scope "$2"
        ;;
    --self-test)
        if [[ $# -ne 1 ]]; then
            usage
            exit 2
        fi
        self_test
        ;;
    *)
        usage
        exit 2
        ;;
esac
