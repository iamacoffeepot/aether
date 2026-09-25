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
# The registry is a digest-pinned `registry:2` container bound to 127.0.0.1,
# which the daemon trusts without TLS. Its blobs live in a named volume, so a
# stopped and restarted registry still serves what was pushed. Rerunning is
# idempotent: it reuses the running registry, the build is a cache hit when
# nothing changed, and the same references come back. Only the two reference
# lines go to stdout; build and push output goes to stderr.
#
# AETHER_ENV_REGISTRY_PORT picks the loopback port (default 5000).

set -euo pipefail

here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../../.." && pwd)

REGISTRY_IMAGE=registry:2@sha256:a3d8aaa63ed8681a604f1dea0aa03f100d5895b6a58ace528858a7b332415373
REGISTRY_CONTAINER=aether-env-registry
REGISTRY_VOLUME=aether-env-registry
REGISTRY_PORT=${AETHER_ENV_REGISTRY_PORT:-5000}
REGISTRY=localhost:${REGISTRY_PORT}

stop_registry() {
  if docker container inspect "$REGISTRY_CONTAINER" >/dev/null 2>&1; then
    docker rm -f "$REGISTRY_CONTAINER" >/dev/null
    echo "removed $REGISTRY_CONTAINER (volume $REGISTRY_VOLUME kept)" >&2
  else
    echo "$REGISTRY_CONTAINER is not present" >&2
  fi
}

# Reuse a running registry, restart a stopped one, or start a new one, then
# wait (bounded) for its API to answer.
ensure_registry() {
  local state
  state=$(docker container inspect --format '{{.State.Running}}' "$REGISTRY_CONTAINER" 2>/dev/null || true)
  case "$state" in
    true) ;;
    false) docker start "$REGISTRY_CONTAINER" >/dev/null ;;
    *)
      docker run -d --name "$REGISTRY_CONTAINER" \
        --label aether.env=registry \
        -p "127.0.0.1:${REGISTRY_PORT}:5000" \
        -v "${REGISTRY_VOLUME}:/var/lib/registry" \
        "$REGISTRY_IMAGE" >/dev/null
      ;;
  esac

  local _
  for _ in $(seq 1 50); do
    if curl -fsS "http://127.0.0.1:${REGISTRY_PORT}/v2/" >/dev/null 2>&1; then
      return 0
    fi
    sleep 0.2
  done
  echo "the registry on 127.0.0.1:${REGISTRY_PORT} did not answer" >&2
  exit 1
}

# Build one image from `$1`'s Dockerfile over context `$2`, push it as
# `$REGISTRY/aether-env/$3`, and print its digest-pinned reference. Provenance
# and SBOM attestations are off: they carry build times, so a cache-hit rebuild
# would push a new digest.
publish_image() {
  local dockerfile=$1 context=$2 name=$3
  local repository=${REGISTRY}/aether-env/${name}
  docker build --provenance=false --sbom=false \
    -f "$dockerfile" -t "${repository}:latest" "$context" >&2
  local digest
  digest=$(docker push "${repository}:latest" | tee /dev/stderr \
    | sed -n 's/^.*digest: \(sha256:[0-9a-f]\{64\}\).*$/\1/p' | tail -n 1)
  if [[ -z "$digest" ]]; then
    echo "docker push ${repository}:latest reported no digest" >&2
    exit 1
  fi
  echo "${repository}@${digest}"
}

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

base=$(publish_image "$here/base.Dockerfile" "$scratch/base" base)
toolchain=$(publish_image "$here/toolchain.Dockerfile" "$scratch/toolchain" toolchain)

echo "base=${base}"
echo "toolchain=${toolchain}"
