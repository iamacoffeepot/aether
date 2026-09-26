#!/usr/bin/env bash
# Build the Bloomery environment images and publish them to a loopback
# registry, so the `aether.workspace` actor can import them by digest
# (ADR-0237 decision 3).
#
#   publish.sh          start the registry if needed, build, push, and print
#                       base=<repository>@sha256:<digest>
#                       toolchain=<repository>@sha256:<digest>
#   publish.sh --stop   remove the registry container; its volume stays
#
# The registry and the push live in `../registry.sh`. Rerunning is
# idempotent: it reuses the running registry, the build is a cache hit when
# nothing changed, and the same references come back. Only the two reference
# lines go to stdout; build and push output goes to stderr.
#
# AETHER_ENV_REGISTRY_PORT picks the loopback port (default 5000).

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../../.." && pwd)

# shellcheck source=../registry.sh
source "$here/../registry.sh"

if [[ "${1:-}" == "--stop" ]]; then
  stop_registry
  exit 0
elif [[ $# -ne 0 ]]; then
  echo "usage: $0 [--stop]" >&2
  exit 2
fi

ensure_registry

scratch=$(mktemp -d)
trap 'rm -rf "$scratch"' EXIT
mkdir "$scratch/base" "$scratch/toolchain"
cp "$repo/rust-toolchain.toml" "$scratch/toolchain/"

base=$(publish_image "$here/base.Dockerfile" "$scratch/base" aether-env/base)
toolchain=$(publish_image "$here/toolchain.Dockerfile" "$scratch/toolchain" aether-env/toolchain)

echo "base=${base}"
echo "toolchain=${toolchain}"
