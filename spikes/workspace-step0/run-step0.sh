#!/usr/bin/env bash
# ADR-0237 step 0 (question 1): build the spike bundle wasm, write a small crate with one clippy finding, and run the
# loop scenario once. Needs Docker and an image with cargo + clippy (SPIKE_IMAGE, see import-env.sh for how the spike
# imported one). Scratch goes under SPIKE_SCRATCH (default: $TMPDIR/spike-ws0).
set -euo pipefail
scratch=${SPIKE_SCRATCH:-${TMPDIR:-/tmp}/spike-ws0}
work=$scratch/q1crate
rm -rf "$work" && mkdir -p "$work/src"
printf '[package]\nname = "hello"\nversion = "0.1.0"\nedition = "2024"\n\n[dependencies]\n' > "$work/Cargo.toml"
printf 'fn main() {\n    let v: Vec<i32> = Vec::new();\n    if v.len() == 0 {\n        println!("empty");\n    }\n}\n' \
  > "$work/src/main.rs"
export SPIKE_WORK_DIR=$work SPIKE_IMAGE=${SPIKE_IMAGE:-spike-ws0-env:v1} SPIKE_USER="$(id -u):$(id -g)"

if ! cargo build -p spike-workspace-step0-program --target wasm32-unknown-unknown > "$scratch/wasm-build.log" 2>&1; then
  tail -40 "$scratch/wasm-build.log"; exit 1
fi
rc=0
cargo test -p spike-workspace-step0-loop --test step0 -- --ignored --nocapture > "$scratch/step0.log" 2>&1 || rc=$?
grep -E '^spike:|test result|panicked|^error' "$scratch/step0.log" | cut -c1-3000 || tail -60 "$scratch/step0.log"
[ "$rc" = 0 ] || tail -30 "$scratch/step0.log"
exit $rc
