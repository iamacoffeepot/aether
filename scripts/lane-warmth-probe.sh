#!/usr/bin/env bash
#
# Prove — on the host that runs the lanes — what makes a seeded lane slot cold.
#
# The lane slots hand every dispatch a warm cargo target directory, and the
# dispatches observably recompile the workspace anyway. Two candidate causes are
# separable, and this probe separates them on a three-crate synthetic workspace
# so the answer costs seconds instead of a lane:
#
#   1. cargo's own freshness is decided by mtime. A checkout materialized into a
#      directory git has not written before stamps every source file with the
#      current time, which is newer than the artifact built from it, so cargo
#      rebuilds the workspace even though the bytes are identical. Moving the
#      same tree to a different path with its mtimes intact does NOT rebuild —
#      cargo hashes a workspace member's identity relative to the workspace
#      root, so the artifact filenames are path-independent.
#
#   2. sccache's key is path-dependent. It hashes the paths named on the rustc
#      invocation, so the same source compiled from a different directory, or
#      into a different target directory, misses. Registry dependencies live at
#      a path that never moves and keep hitting, which is why a lane can report
#      a partial hit rate while recompiling every workspace crate.
#
# Together they say a per-dispatch checkout path cannot be warm at all: cargo
# recompiles it and sccache cannot serve the recompile. That is the measurement
# behind pinning a mechanical lane to its slot's own checkout.
#
# Usage: scripts/lane-warmth-probe.sh [workdir]
# Leaves nothing behind outside `workdir` (default: a fresh mktemp -d), and
# never touches the host's sccache server, cache directory, or any repo target.

set -euo pipefail

WORK="${1:-$(mktemp -d)}"
mkdir -p "$WORK"
WORK="$(cd "$WORK" && pwd)"

say() { printf '\n== %s\n' "$*"; }
verdict() { printf '%-58s %s\n' "$1" "$2"; }

seed_workspace() {
  local root="$1"
  mkdir -p "$root/crates/leaf/src" "$root/crates/mid/src" "$root/crates/app/src"
  cat >"$root/Cargo.toml" <<'EOF'
[workspace]
members = ["crates/leaf", "crates/mid", "crates/app"]
resolver = "2"
EOF
  cat >"$root/crates/leaf/Cargo.toml" <<'EOF'
[package]
name = "leaf"
version = "0.1.0"
edition = "2021"

[dependencies]
cfg-if = "1"
EOF
  cat >"$root/crates/mid/Cargo.toml" <<'EOF'
[package]
name = "mid"
version = "0.1.0"
edition = "2021"

[dependencies]
leaf = { path = "../leaf" }
EOF
  cat >"$root/crates/app/Cargo.toml" <<'EOF'
[package]
name = "app"
version = "0.1.0"
edition = "2021"

[dependencies]
mid = { path = "../mid" }
EOF
  echo 'pub fn leaf() -> u32 { cfg_if::cfg_if! { if #[cfg(unix)] { 1 } else { 2 } } }' >"$root/crates/leaf/src/lib.rs"
  echo 'pub fn mid() -> u32 { leaf::leaf() + 1 }' >"$root/crates/mid/src/lib.rs"
  echo 'pub fn app() -> u32 { mid::mid() + 1 }' >"$root/crates/app/src/lib.rs"
}

# How many workspace crates cargo chose to compile for this invocation.
compiled_count() {
  local manifest="$1" target="$2" log="$3"
  CARGO_TARGET_DIR="$target" cargo build --manifest-path "$manifest" >"$log" 2>&1 || {
    cat "$log"
    exit 1
  }
  grep -c '^ *Compiling' "$log" || true
}

say "cargo freshness: does the path alone invalidate, or the mtimes?"
seed_workspace "$WORK/ws"
cp -a "$WORK/ws" "$WORK/path-a"
first="$(compiled_count "$WORK/path-a/Cargo.toml" "$WORK/target-shared" "$WORK/build-a.log")"

cp -a "$WORK/ws" "$WORK/path-b"
moved="$(compiled_count "$WORK/path-b/Cargo.toml" "$WORK/target-shared" "$WORK/build-b.log")"

cp -R "$WORK/ws" "$WORK/path-c"
find "$WORK/path-c" -exec touch {} +
restamped="$(compiled_count "$WORK/path-c/Cargo.toml" "$WORK/target-shared" "$WORK/build-c.log")"

verdict "cold build at path A compiled" "$first crates"
verdict "same tree at path B, mtimes preserved" "$moved crates"
verdict "same tree at path C, mtimes restamped" "$restamped crates"
if [ "$moved" -eq 0 ] && [ "$restamped" -gt 0 ]; then
  verdict "hypothesis 1 (mtime, not path, invalidates cargo)" "CONFIRMED"
else
  verdict "hypothesis 1 (mtime, not path, invalidates cargo)" "REFUTED — re-read before acting on it"
fi

if ! command -v sccache >/dev/null 2>&1; then
  say "sccache is not on PATH; skipping the cache-key half of the probe"
  echo "workdir: $WORK"
  exit 0
fi

say "sccache key: is it sensitive to the source path and the target path?"
export SCCACHE_DIR="$WORK/sccache"
export SCCACHE_CACHE_SIZE="2G"
export SCCACHE_SERVER_PORT="${SCCACHE_SERVER_PORT:-14226}"
export RUSTC_WRAPPER=sccache
export CARGO_INCREMENTAL=0
trap 'sccache --stop-server >/dev/null 2>&1 || true' EXIT
sccache --stop-server >/dev/null 2>&1 || true
sccache --start-server >/dev/null 2>&1

# One field out of `sccache -s`, by exact leading label.
stat_of() { sccache -s | awk -v want="$1" '$0 ~ "^"want" " { print $NF }' | head -1; }

populate() {
  local manifest="$1" target="$2" log="$3"
  rm -rf "$target"
  sccache --zero-stats >/dev/null 2>&1
  CARGO_TARGET_DIR="$target" cargo build --manifest-path "$manifest" >"$log" 2>&1 || {
    cat "$log"
    exit 1
  }
  printf '%s/%s' "$(stat_of 'Cache hits')" "$(stat_of 'Cache misses')"
}

cold="$(populate "$WORK/path-a/Cargo.toml" "$WORK/target-keyed" "$WORK/cache-cold.log")"
same_path="$(populate "$WORK/path-a/Cargo.toml" "$WORK/target-keyed" "$WORK/cache-same.log")"
other_path="$(populate "$WORK/path-b/Cargo.toml" "$WORK/target-keyed" "$WORK/cache-other.log")"
other_target="$(populate "$WORK/path-a/Cargo.toml" "$WORK/target-elsewhere" "$WORK/cache-target.log")"

verdict "cold cache, source A, target T (hits/misses)" "$cold"
verdict "wiped target, same source A, same target T" "$same_path"
verdict "wiped target, source B, same target T" "$other_path"
verdict "wiped target, source A, target U" "$other_target"
echo
echo "Read it as: the same-path rebuild should be all hits, the different-source-path"
echo "rebuild should hit only the registry dependency, and the different-target-path"
echo "rebuild should miss everything. Every miss there is a full rustc run a lane pays."
echo
echo "workdir: $WORK"
