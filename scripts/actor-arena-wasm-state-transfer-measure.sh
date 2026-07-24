#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <artifact-directory>" >&2
    exit 2
fi

artifact_root=$1
workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
compare="$workspace_root/target/release/aether-actor-arena-compare"
seed=6840227784451616781

cargo build --release -p aether-harness-actor-arena --bins --manifest-path "$workspace_root/Cargo.toml"

measure() {
    local name=$1
    local actors=$2
    local state_bytes=$3
    local sweeps=$4
    local warmup_sweeps=$5
    local pairs=$6

    "$compare" \
        --base wasm-arena \
        --candidate wasm-copy-roundtrip \
        --workload scene-sweep \
        --pairs "$pairs" \
        --artifact-dir "$artifact_root/$name" \
        --actors "$actors" \
        --mails "$((actors * sweeps))" \
        --mails-per-activation 1 \
        --page-slots 64 \
        --state-bytes "$state_bytes" \
        --pattern sequential \
        --seed "$seed" \
        --warmup-mails "$((actors * warmup_sweeps))"
}

# Primary bullet cell: 4 MiB of 64-byte actor state copied in and out for
# each of 80 complete scene updates.
measure primary-65536-state64 65536 64 80 8 9

# Population sensitivity holds total actor updates and transferred bytes close
# to the primary cell while changing the number of Wasm entries/full sweeps.
measure population-4096-state64 4096 64 1280 32 7
measure population-16384-state64 16384 64 320 16 7
measure population-100000-state64 100000 64 50 8 7
measure population-131072-state64 131072 64 40 8 7

# Cold-state sensitivity keeps the bullet update confined to the first 64
# bytes while copying increasingly large complete actor records. Each copied
# arm transfers roughly 1 GiB total across both directions per trial.
measure state-256 65536 256 32 4 7
measure state-1024 32768 1024 16 4 7
measure state-4096 16384 4096 8 2 5
