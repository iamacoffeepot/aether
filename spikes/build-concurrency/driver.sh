#!/usr/bin/env bash
# Spike: concurrent-build capacity on eve (32 cores / 31 GiB).
# Workload: the aether workspace itself (what the bloom lanes build).
# Cells, safest first:
#   B  one cold build, -j8            -> per-build peak RSS + target size (disk gate)
#   C  N cold builds concurrent, -j8  -> the claimed capacity number
#   D  N warm builds concurrent, -j8  -> warm-traffic capacity
#   A  one cold build, uncapped -j32  -> the rule-of-thumb check (guarded, runs LAST)
# Every cell runs inside a systemd scope with MemoryMax=26G, MemorySwapMax=0 so an
# overrun OOM-kills the cell, never the coordinator. sccache disabled throughout.
set -uo pipefail
export PATH="$HOME/.cargo/bin:$PATH"
export RUSTC_WRAPPER=
SRC=/mnt/dev/workspace/aether
OUT=/mnt/dev/tmp/build-spike
BUILD_ARGS="--workspace --all-targets --locked"
mkdir -p "$OUT"
cd "$SRC"

log() { echo "$(date -u +%H:%M:%S) $*" >> "$OUT/driver.log"; }
mark() { echo "$(date +%s),MARK,$1,," >> "$OUT/mem.csv"; log "== $1 =="; }

echo "epoch,kind,mem_avail_kb,swap_free_kb," > "$OUT/mem.csv"
( while :; do
    awk '/MemAvailable/{a=$2} /SwapFree/{s=$2} END{printf "%s,SAMPLE,%s,%s,\n", strftime("%s"), a, s}' /proc/meminfo >> "$OUT/mem.csv"
    sleep 2
  done ) &
SAMPLER=$!
trap 'kill $SAMPLER 2>/dev/null' EXIT

scoped() { # scoped <cell-name> <command...>
  local name=$1; shift
  systemd-run --user --scope --collect -p MemoryMax=26G -p MemorySwapMax=0 "$@"
}

mark cellB-start
scoped cellB env CARGO_TARGET_DIR="$OUT/c1" /usr/bin/time -v cargo build $BUILD_ARGS -j8 > "$OUT/cellB.log" 2>&1
log "cellB exit=$?"
mark cellB-end

SZ_GB=$(du -s --block-size=1G "$OUT/c1" | cut -f1)
FREE_GB=$(df --block-size=1G --output=avail /mnt/dev | tail -1 | tr -d ' ')
N=4
if [ $((SZ_GB * 4 + 40)) -gt "$FREE_GB" ]; then N=3; fi
if [ $((SZ_GB * 3 + 40)) -gt "$FREE_GB" ]; then N=2; fi
log "target size ${SZ_GB}G, free ${FREE_GB}G -> cold concurrency N=$N"

rm -rf "$OUT/c1"
mark cellC-start
scoped cellC bash -c '
  out=$1; n=$2; args=$3; shift 3
  for i in $(seq 1 "$n"); do
    CARGO_TARGET_DIR="$out/c$i" /usr/bin/time -v cargo build $args -j8 > "$out/cellC-$i.log" 2>&1 &
  done
  wait' cellC "$OUT" "$N" "$BUILD_ARGS"
log "cellC exit=$?"
mark cellC-end

touch -c crates/aether-kinds/src/lib.rs
mark cellD-start
scoped cellD bash -c '
  out=$1; n=$2; args=$3; shift 3
  for i in $(seq 1 "$n"); do
    CARGO_TARGET_DIR="$out/c$i" /usr/bin/time -v cargo build $args -j8 > "$out/cellD-$i.log" 2>&1 &
  done
  wait' cellD "$OUT" "$N" "$BUILD_ARGS"
log "cellD exit=$?"
mark cellD-end

rm -rf "$OUT/c1"
mark cellA-start
scoped cellA env CARGO_TARGET_DIR="$OUT/c1" /usr/bin/time -v cargo build $BUILD_ARGS -j32 > "$OUT/cellA.log" 2>&1
log "cellA exit=$? (137/oom = the uncapped build does not fit under 26G)"
mark cellA-end

du -s --block-size=1G "$OUT"/c[0-9] >> "$OUT/driver.log" 2>/dev/null
df -h /mnt/dev | tail -1 >> "$OUT/driver.log"
log DONE
