#!/usr/bin/env bash
# Regenerate the committed graphify knowledge graph deterministically.
#
# graphify (https://github.com/safishamsi/graphify) extracts a knowledge graph
# from the repo. Code/structural extraction is deterministic and offline (AST via
# tree-sitter, no LLM/API calls), so the graph can be committed and verified in
# CI. graphify's bash extractor encodes the absolute checkout path into a handful
# of node ids, so we run scripts/graphify-normalize.py afterward to make the
# output byte-reproducible across machines (see that script's docstring).
#
#   scripts/graphify-graph.sh build    # (re)generate graphify-out/graph.json
#   scripts/graphify-graph.sh check    # regenerate into a temp dir, diff vs committed
#
# Pin the graphify version so a format change in a new release can't silently
# break the committed graph; bump GRAPHIFY_VERSION and rebuild together.
set -euo pipefail

GRAPHIFY_VERSION="${GRAPHIFY_VERSION:-0.8.33}"
REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
GRAPH_REL="graphify-out/graph.json"
NORMALIZE="$REPO_ROOT/scripts/graphify-normalize.py"

run_graphify() {
  # Prefer an already-installed graphify; otherwise run the pinned version via
  # uvx (CI installs uv). Either way extraction is AST-only — no API key needed.
  if command -v graphify >/dev/null 2>&1; then
    graphify "$@"
  else
    uvx --from "graphifyy==${GRAPHIFY_VERSION}" graphify "$@"
  fi
}

python_bin() {
  if command -v python3 >/dev/null 2>&1; then echo python3;
  elif command -v uv >/dev/null 2>&1; then echo "uv run --no-project python3";
  else echo "python3"; fi
}

generate() {
  # Target dir to generate into (defaults to the repo root).
  local target="${1:-$REPO_ROOT}"
  # Start from a clean slate: `graphify update` merges into any existing
  # graph.json, so a stale one would make the result history-dependent.
  rm -rf "$target/graphify-out"
  ( cd "$target" && run_graphify update . --no-cluster >/dev/null )
  # shellcheck disable=SC2046
  $(python_bin) "$NORMALIZE" "$target/$GRAPH_REL" >/dev/null
}

case "${1:-build}" in
  build)
    generate "$REPO_ROOT"
    echo "Wrote $REPO_ROOT/$GRAPH_REL"
    ;;
  check)
    tmp="$(mktemp -d)"
    trap 'rm -rf "$tmp"' EXIT
    # Extract a clean copy of the tracked tree so untracked files (the existing
    # graphify-out, build artifacts) can't perturb the regenerated graph.
    git -C "$REPO_ROOT" archive --format=tar HEAD | tar -xf - -C "$tmp"
    generate "$tmp"
    if ! diff -q "$REPO_ROOT/$GRAPH_REL" "$tmp/$GRAPH_REL" >/dev/null; then
      echo "ERROR: $GRAPH_REL is out of date." >&2
      echo "Regenerate it with: scripts/graphify-graph.sh build" >&2
      echo "--- first differing lines ---" >&2
      diff "$REPO_ROOT/$GRAPH_REL" "$tmp/$GRAPH_REL" | head -40 >&2 || true
      exit 1
    fi
    echo "$GRAPH_REL is up to date."
    ;;
  *)
    echo "usage: $0 {build|check}" >&2
    exit 2
    ;;
esac
