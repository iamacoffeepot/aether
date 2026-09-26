#!/usr/bin/env bash
# Pack a source checkout into an image and publish it to the loopback
# registry, so the `aether.workspace` actor can import it by digest
# (ADR-0237 decision 3). The checkout is named here, on the build host, and
# never in mail: only the printed reference crosses into the engine.
#
#   publish.sh             pack the repository holding this script
#   publish.sh <checkout>  pack another checkout directory
#
# It starts the registry if needed, builds `source.Dockerfile` over the
# checkout, pushes it, and prints
#
#   source=<repository>@sha256:<digest>
#
# The context is the allowlist in `source.Dockerfile.dockerignore`, which only
# BuildKit reads, so the build forces BuildKit: the legacy builder would ignore
# the file and pack the whole directory. Repacking an unchanged checkout is a
# cache hit and gives the same reference. Another checkout of the same content
# can give another reference, because the image layer keeps file times, but it
# imports as the same tree: the import drops times. Only the reference line
# goes to stdout; build and push output goes to stderr.
# `../environment/publish.sh --stop` removes the registry both recipes share.
#
# AETHER_ENV_REGISTRY_PORT picks the loopback port (default 5000).

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)

# shellcheck source=../registry.sh
source "$here/../registry.sh"

case $# in
  0) checkout=$(cd "$here/../../.." && pwd) ;;
  1)
    if [[ ! -d "$1" ]]; then
      echo "$1 is not a directory" >&2
      exit 2
    fi
    checkout=$(cd "$1" && pwd)
    ;;
  *)
    echo "usage: $0 [checkout]" >&2
    exit 2
    ;;
esac

ensure_registry

export DOCKER_BUILDKIT=1
reference=$(publish_image "$here/source.Dockerfile" "$checkout" aether-source/checkout)

echo "source=${reference}"
