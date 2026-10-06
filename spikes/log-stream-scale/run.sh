#!/usr/bin/env bash
# Sweep the log-stream-scale spike. Run from the repository root on the
# benchmark host, after `cargo build --release -p log-stream-scale` and a
# second build with `--features no-hook` copied to `<bin>-nohook`.
#
#   spikes/log-stream-scale/run.sh <stage> <out-dir> [first-rep] [last-rep]
#
# Stages: closed, stock, grid10, grid100, grid1000, extra, flood, backfill,
# overload4, overload1000, all. Each appends rows to a CSV in <out-dir>; a
# stage is safe to re-run for further reps. stderr (the engine's own log) is
# discarded. `stock` runs the repository's own dispatch comparison
# (aether-perf-compare) and needs LSS_BASE_TRIAL, an aether-perf-trial binary
# built from unmodified main.
#
# Modes: off (tap closed), mail (one unrooted mail per logging handler),
# mailroot (the same mail on a fresh causal chain), buffer (one bounded
# buffer under one lock), shard (the bounded buffer split over 16 locks).
set -uo pipefail

stage=${1:?stage}
out=${2:?out dir}
first=${3:-1}
last=${4:-5}
bin=${LSS_BIN:-${CARGO_TARGET_DIR:-target}/release/log-stream-scale}
nohook=${LSS_NOHOOK_BIN:-$bin-nohook}
mkdir -p "$out"

run() {
    local file=$1 binary=$2
    shift 2
    timeout "${LSS_TIMEOUT:-600}" "$binary" "$@" 2>/dev/null >> "$out/$file" || echo "exit=$? $*" >> "$out/failures.txt"
}

header() {
    local file=$1 scenario=$2
    [ -s "$out/$file" ] || "$bin" scenario="$scenario" header=1 > "$out/$file"
}

echo "$stage reps $first..$last: $(uptime)" >> "$out/host-load.txt"

case "$stage" in
closed)
    # Item 1: the closed tap against the same dispatch loop with the gather
    # compiled out. Frames run back to back; every handler is quiet.
    header closed.csv steady
    for rep in $(seq "$first" "$last"); do
        for actors in 100 1000; do
            run closed.csv "$nohook" scenario=steady label=nohook mode=off actors=$actors handlers=0 quiet=100000 pace=0 frames=200
            run closed.csv "$bin" scenario=steady mode=off actors=$actors handlers=0 quiet=100000 pace=0 frames=200
        done
        run closed.csv "$nohook" scenario=steady label=nohook mode=off actors=100 handlers=100000 pace=0 frames=200
        run closed.csv "$bin" scenario=steady mode=off actors=100 handlers=100000 pace=0 frames=200
        # The open tap on handlers that log nothing.
        for mode in mail buffer shard; do
            run closed.csv "$bin" scenario=steady mode=$mode actors=100 handlers=0 quiet=100000 pace=0 frames=200
        done
    done
    ;;
stock)
    # Item 1 by the repository's own tool: K interleaved trials of the
    # unmodified-main sweep binary against this branch's.
    dir=$(dirname "$bin")
    base=${LSS_BASE_TRIAL:?aether-perf-trial built from unmodified main}
    export AETHER_PERF_WORKERS=max,2 AETHER_PERF_FRAMES=200 AETHER_PERF_TOPOS=ci
    AETHER_PERF_DRIVE=latency AETHER_PERF_TIER=light "$dir/aether-perf-compare" --base "$base" \
        --cand "$dir/aether-perf-trial" -k 12 --out "$out/stock-latency.json" \
        --title "latency: spike branch (tap closed) vs unmodified main" --subtitle "12 trials per side, interleaved" \
        > "$out/stock-latency.md" 2> "$out/stock-latency.log"
    AETHER_PERF_DRIVE=saturate AETHER_PERF_TIER=light AETHER_PERF_BACKLOG=512 "$dir/aether-perf-compare" --base "$base" \
        --cand "$dir/aether-perf-trial" -k 12 --out "$out/stock-saturate.json" \
        --title "throughput: spike branch (tap closed) vs unmodified main" --subtitle "12 trials per side, interleaved" \
        > "$out/stock-saturate.md" 2> "$out/stock-saturate.log"
    ;;
grid10 | grid100 | grid1000)
    # Items 2, 4, 5: the paced grid for one actor count.
    actors=${stage#grid}
    header steady.csv steady
    for rep in $(seq "$first" "$last"); do
        for mode in off mail buffer shard; do
            run steady.csv "$bin" scenario=steady mode=$mode actors=$actors handlers=0 quiet=10000 frames=300
            run steady.csv "$bin" scenario=steady mode=$mode actors=$actors handlers=100 frames=300
            run steady.csv "$bin" scenario=steady mode=$mode actors=$actors handlers=10000 frames=300
            run steady.csv "$bin" scenario=steady mode=$mode actors=$actors handlers=100000 frames=120
        done
    done
    ;;
extra)
    # Ten lines per handler, the native `tracing` log path, each design with
    # the other's batch cap, and the rooted mail.
    header steady.csv steady
    for rep in $(seq "$first" "$last"); do
        for mode in off mail buffer shard; do
            run steady.csv "$bin" scenario=steady mode=$mode actors=100 handlers=10000 lines=10 frames=300
            run steady.csv "$bin" scenario=steady mode=$mode logpath=tracing actors=100 handlers=100 frames=300
            run steady.csv "$bin" scenario=steady mode=$mode logpath=tracing actors=100 handlers=10000 frames=300
        done
        run steady.csv "$bin" scenario=steady mode=buffer label=buffer4k batch_cap=4096 actors=100 handlers=10000 frames=300
        run steady.csv "$bin" scenario=steady mode=buffer label=buffer4k batch_cap=4096 actors=100 handlers=100 frames=300
        run steady.csv "$bin" scenario=steady mode=mail label=mail256 batch_cap=256 actors=100 handlers=10000 frames=300
        run steady.csv "$bin" scenario=steady mode=mailroot actors=100 handlers=100 frames=300
        # Dies on the settlement table (16384 live roots): bounded wait.
        LSS_TIMEOUT=45 run steady.csv "$bin" scenario=steady mode=mailroot actors=100 handlers=10000 frames=300
    done
    ;;
flood)
    # Frames back to back: the added cost per logging handler at full rate.
    header steady.csv steady
    for rep in $(seq "$first" "$last"); do
        for actors in 100 1000; do
            for mode in off mail buffer shard; do
                run steady.csv "$bin" scenario=steady mode=$mode actors=$actors handlers=10000 pace=0 frames=300
            done
            LSS_TIMEOUT=45 run steady.csv "$bin" scenario=steady mode=mailroot actors=$actors handlers=10000 pace=0 frames=300
        done
    done
    ;;
overload4 | overload1000)
    # Items 3, 6: producers faster than the gatherer, off the frame's chain,
    # for 30 s. 4 loopers leave half the 8 workers idle; 1000 saturate them.
    loopers=${stage#overload}
    header "$stage.csv" overload
    for rep in $(seq "$first" "$last"); do
        for mode in off mail buffer shard; do
            run "$stage.csv" "$bin" scenario=overload mode=$mode actors=1000 loopers=$loopers secs=30
        done
        LSS_TIMEOUT=90 run "$stage.csv" "$bin" scenario=overload mode=mailroot actors=1000 loopers=$loopers secs=30
    done
    ;;
backfill)
    # Item 7: opening the tap on 1000 actors holding full rings.
    header backfill.csv backfill
    for rep in $(seq "$first" "$last"); do
        for mode in mail mailroot buffer shard; do
            for backfill in 1 0; do
                run backfill.csv "$bin" scenario=backfill mode=$mode backfill=$backfill actors=1000
            done
        done
    done
    ;;
all)
    # The closed-tap comparison gets twice the reps: its difference is small.
    "$0" closed "$out" $((last + 1)) $((2 * last))
    for each in closed stock grid10 grid100 grid1000 extra flood backfill overload4 overload1000; do
        "$0" "$each" "$out" "$first" "$last"
        echo "$each done $(date -u +%H:%M:%S)" >> "$out/progress.txt"
    done
    ;;
rest)
    # Everything after the paced grid, for a sweep resumed part-way.
    for each in flood backfill overload4 overload1000; do
        "$0" "$each" "$out" "$first" "$last"
        echo "$each done $(date -u +%H:%M:%S)" >> "$out/progress.txt"
    done
    ;;
*)
    echo "unknown stage $stage" >&2
    exit 2
    ;;
esac
