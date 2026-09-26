#!/usr/bin/env bash
# Prove the base image's package list is complete: run
# `cargo check --workspace --locked` over the repository in the base userland
# with only the toolchain directory added, the way an environment layers them
# (ADR-0237 decision 3).
#
#   check.sh
#
# It runs `publish.sh` (idempotent) and checks the two references it prints,
# so the check covers exactly the images an import will take. Then:
#
#   1. `cargo fetch --locked` runs in the toolchain image, into a throwaway
#      volume. Building needs neither the network nor the fetch, so the check
#      below runs offline; the base carries CA certificates only because
#      `vendor.cargo` fetches inside the environment.
#   2. A throwaway image is built FROM the base, with the toolchain directory
#      (`rustc --print sysroot` in the toolchain image) copied in at the same
#      path and nothing else.
#   3. The check runs in that image with no network, the repository
#      bind-mounted read-only, the fetched crates as CARGO_HOME, and the target
#      directory on a tmpfs. A missing `-dev` package fails here.
#
# The throwaway image and volume are removed on exit. AETHER_ENV_CHECK_TMPFS
# sizes the target tmpfs (default 32g).

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../../.." && pwd)
tmpfs_size=${AETHER_ENV_CHECK_TMPFS:-32g}

refs=$("$here/publish.sh")
base=$(sed -n 's/^base=//p' <<<"$refs")
toolchain=$(sed -n 's/^toolchain=//p' <<<"$refs")
echo "base=${base}" >&2
echo "toolchain=${toolchain}" >&2

check_image=aether-env-check:$$
cargo_volume=aether-env-check-$$
scratch=$(mktemp -d)
cleanup() {
  docker image rm -f "$check_image" >/dev/null 2>&1 || true
  docker volume rm -f "$cargo_volume" >/dev/null 2>&1 || true
  rm -rf "$scratch"
}
trap cleanup EXIT

sysroot=$(docker run --rm --entrypoint rustc "$toolchain" --print sysroot)
echo "toolchain directory: ${sysroot}" >&2

docker volume create "$cargo_volume" >/dev/null
docker run --rm \
  -v "$repo:/src:ro" -v "$cargo_volume:/cargo" \
  -e CARGO_HOME=/cargo -w /src \
  "$toolchain" cargo fetch --locked

docker build --provenance=false --sbom=false -t "$check_image" -f - "$scratch" <<EOF
FROM ${base}
COPY --from=${toolchain} ${sysroot} ${sysroot}
ENV PATH=${sysroot}/bin:/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin
EOF

docker run --rm --network none \
  -v "$repo:/src:ro" -v "$cargo_volume:/cargo" \
  --tmpfs "/target:rw,exec,size=${tmpfs_size}" \
  -e CARGO_HOME=/cargo -e CARGO_TARGET_DIR=/target -w /src \
  "$check_image" cargo check --workspace --locked --offline
