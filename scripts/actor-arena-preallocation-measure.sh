#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <artifact-directory>" >&2
    exit 2
fi

artifact_root=$1
workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
matrix="$workspace_root/target/release/aether-actor-arena-preallocation-matrix"

cargo build --release -p aether-harness-actor-arena --bins --manifest-path "$workspace_root/Cargo.toml"

common=(
    --actors 65536
    --page-slots 64
    --state-bytes 64
    --burst-actors 4096
    --seed 6840227784451616781
)

"$matrix" --artifact-dir "$artifact_root/primary" --campaign all --samples 7 --sweeps 80 "${common[@]}"

"$matrix" --artifact-dir "$artifact_root/diagnostic-allocations" --campaign diagnostic --samples 3 --sweeps 20 \
    --instrument-allocations "${common[@]}"

"$matrix" --artifact-dir "$artifact_root/diagnostic-touched" --campaign diagnostic --samples 3 --sweeps 20 \
    --touch-reserved "${common[@]}"
