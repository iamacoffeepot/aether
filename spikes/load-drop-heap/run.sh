#!/usr/bin/env bash
# SPIKE (issue 7414): run the load and drop cells. Linux, from the checkout
# root, after
#   cargo xtask build-wasm
#   cargo build --release --target wasm32-unknown-unknown -p spike-load-drop-bundle
#   CARGO_PROFILE_RELEASE_DEBUG=line-tables-only cargo build --release \
#     -p aether-harness-perf --features heap-profile --bin aether-perf-load-drop
#
#   spikes/load-drop-heap/run.sh <out-dir> [jobs]
#
# One process is one cell at one count in one mode:
#   dhat    dhat heap profile (<label>-<count>.dhat.json) and the sample series
#   plain   no profiler, glibc defaults: the resident-set reading
#   arena1  no profiler, MALLOC_ARENA_MAX=1: every malloc is in [heap] or an
#           mmapped chunk, so mallinfo2 covers the whole heap
set -euo pipefail
out=${1:?out dir}; jobs=${2:-10}
bin=${CARGO_TARGET_DIR:-target}/release/aether-perf-load-drop
mkdir -p "$out"

wasm_root=${CARGO_TARGET_DIR:-target}/wasm32-unknown-unknown
spike="--wasm $wasm_root/release/spike_load_drop_bundle.wasm"
fixture="--wasm $wasm_root/debug/aether_test_fixtures_bundle.wasm --export test.spike.asset_probe --counter-keys"
# The shape issue 7414's figures were measured with (runite#30's churn): a
# content-addressed bundle, 64 assets, 1 MiB of payload, 100 instances live.
ref="--addressed --assets 64 --payload 1048576"

# label | counts | driver args
cells=(
  "same-ref|1000 2000 4000|same $spike $ref --live 100"
  "same-ref-live0|1000 2000 4000|same $spike $ref"
  "same-ref-watch|1000 2000 4000|same $spike $ref --live 100 --watch"
  "same-bare|1000 2000 4000|same $spike --addressed"
  "same-fixture|1000 2000 4000|same $fixture"
  "distinct-ref|500 1000 2000|distinct $spike $ref --live 100"
  "distinct-ref-live0|500 1000 2000|distinct $spike $ref"
  "distinct-bare|500 1000 2000|distinct $spike --addressed"
  "distinct-unaddressed|500 1000 2000|distinct $spike --assets 64 --payload 1048576"
)

plan=$(mktemp)
for cell in "${cells[@]}"; do
  IFS='|' read -r label counts args <<<"$cell"
  for count in $counts; do
    for mode in dhat plain arena1; do
      echo "$label $count $mode $args" >>"$plan"
    done
  done
done

run_one() {
  local out=$1 bin=$2 label=$3 count=$4 mode=$5; shift 5
  local cell=$1; shift
  local stem="$out/$label-$count.$mode"
  case $mode in
    dhat)   "$bin" "$cell" "$count" "$@" --dhat "$stem.json" >"$stem.series.json" 2>"$stem.err" ;;
    plain)  "$bin" "$cell" "$count" "$@" >"$stem.series.json" 2>"$stem.err" ;;
    arena1) MALLOC_ARENA_MAX=1 "$bin" "$cell" "$count" "$@" >"$stem.series.json" 2>"$stem.err" ;;
  esac
  echo "done $label $count $mode"
}
export -f run_one

xargs -P "$jobs" -L 1 bash -c 'run_one "$0" "$1" "${@:2}"' "$out" "$bin" <"$plan"
rm -f "$plan"
