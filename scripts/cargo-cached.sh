#!/usr/bin/env sh

set -eu

if ! worktree_root=$(git rev-parse --show-toplevel 2>/dev/null); then
    echo "scripts/cargo-cached.sh must run inside a Git worktree" >&2
    exit 2
fi

if ! sccache_path=$(command -v sccache); then
    echo "scripts/cargo-cached.sh requires sccache on PATH" >&2
    exit 2
fi

case "$sccache_path" in
    /*) ;;
    *) sccache_path="$(cd -P "$(dirname "$sccache_path")" && pwd)/$(basename "$sccache_path")" ;;
esac

exec env \
    CARGO_TARGET_DIR="$worktree_root/target" \
    RUSTC_WRAPPER="$sccache_path" \
    CARGO_INCREMENTAL=0 \
    cargo "$@"
