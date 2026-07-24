#!/usr/bin/env bash
set -euo pipefail

if [[ $# -ne 1 ]]; then
    echo "usage: $0 <artifact-directory>" >&2
    exit 2
fi

artifact_root=$1
workspace_root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
compare="$workspace_root/target/release/aether-actor-arena-compare"

cargo build --release -p aether-harness-actor-arena --bins --manifest-path "$workspace_root/Cargo.toml"

dispatch_native=(
    --workload dispatch
    --pairs 9
    --actors 4096
    --mails 5000000
    --mails-per-activation 16
    --page-slots 64
    --state-bytes 256
    --pattern random
    --warmup-mails 250000
)

"$compare" --base boxed-current --candidate arena-state \
    --artifact-dir "$artifact_root/native-storage" "${dispatch_native[@]}"
"$compare" --base arena-state --candidate arena-endpoint \
    --artifact-dir "$artifact_root/native-route" "${dispatch_native[@]}"
"$compare" --base arena-endpoint --candidate arena-page \
    --artifact-dir "$artifact_root/native-page" "${dispatch_native[@]}"

"$compare" --base wasm-detached --candidate wasm-arena --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/wasm-detached-arena" --actors 256 --mails 500000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 50000
"$compare" --base wasm-inline --candidate wasm-arena --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/wasm-inline-arena" --actors 1024 --mails 1000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 100000
"$compare" --base wasm-arena --candidate wasm-batch --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/wasm-batch" --actors 1024 --mails 1000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 100000

"$compare" --base boxed-current --candidate arena-state --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/sensitivity-native-state64" --actors 4096 --mails 5000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 64 --pattern random --warmup-mails 250000
"$compare" --base boxed-current --candidate arena-state --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/sensitivity-native-state4096" --actors 4096 --mails 5000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 4096 --pattern random --warmup-mails 250000
"$compare" --base boxed-current --candidate arena-state --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/sensitivity-native-hot-cold" --actors 4096 --mails 5000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern hot-cold --warmup-mails 250000
"$compare" --base boxed-current --candidate arena-state --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/sensitivity-native-sequential" --actors 4096 --mails 5000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern sequential --warmup-mails 250000
"$compare" --base wasm-inline --candidate wasm-arena --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/sensitivity-wasm-state4096" --actors 1024 --mails 1000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 4096 --pattern random --warmup-mails 100000
"$compare" --base wasm-arena --candidate wasm-batch --workload dispatch --pairs 9 \
    --artifact-dir "$artifact_root/sensitivity-wasm-batch-state4096" --actors 1024 --mails 1000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 4096 --pattern random --warmup-mails 100000

for actors in 64 128 512; do
    "$compare" --base wasm-detached --candidate wasm-arena --workload dispatch --pairs 5 \
        --artifact-dir "$artifact_root/memory-wasm-$actors" --actors "$actors" --mails 100000 \
        --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 10000
done

"$compare" --base arena-endpoint --candidate arena-page --workload dispatch --pairs 3 \
    --artifact-dir "$artifact_root/diagnostic-alloc-native-page" --actors 4096 --mails 1000000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 100000 \
    --instrument-allocations
"$compare" --base wasm-arena --candidate wasm-batch --workload dispatch --pairs 3 \
    --artifact-dir "$artifact_root/diagnostic-alloc-wasm-batch" --actors 1024 --mails 250000 \
    --mails-per-activation 16 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 25000 \
    --instrument-allocations

"$compare" --base boxed-current --candidate arena-state --workload lifecycle-churn --pairs 9 \
    --artifact-dir "$artifact_root/lifecycle-churn" --actors 4096 --mails 1000000 \
    --mails-per-activation 1 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 100000
"$compare" --base boxed-current --candidate arena-state --workload lifecycle-churn --pairs 3 \
    --artifact-dir "$artifact_root/diagnostic-alloc-lifecycle-churn" --actors 4096 --mails 250000 \
    --mails-per-activation 1 --page-slots 64 --state-bytes 256 --pattern random --warmup-mails 25000 \
    --instrument-allocations

scene=(
    --workload scene-sweep
    --pairs 9
    --actors 65536
    --mails 5000000
    --mails-per-activation 1
    --page-slots 64
    --state-bytes 64
    --pattern sequential
    --warmup-mails 500000
)

"$compare" --base boxed-current --candidate arena-state \
    --artifact-dir "$artifact_root/scene-storage-65536" "${scene[@]}"
"$compare" --base arena-state --candidate arena-endpoint \
    --artifact-dir "$artifact_root/scene-endpoint-65536" "${scene[@]}"
"$compare" --base arena-endpoint --candidate arena-page \
    --artifact-dir "$artifact_root/scene-page-65536" "${scene[@]}"
"$compare" --base boxed-current --candidate arena-page \
    --artifact-dir "$artifact_root/scene-full-65536" "${scene[@]}"

for actors in 4096 16384; do
    "$compare" --base boxed-current --candidate arena-page --workload scene-sweep --pairs 9 \
        --artifact-dir "$artifact_root/scene-full-$actors" --actors "$actors" --mails 5000000 \
        --mails-per-activation 1 --page-slots 64 --state-bytes 64 --pattern sequential --warmup-mails 500000
done
