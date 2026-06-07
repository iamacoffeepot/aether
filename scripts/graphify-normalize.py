#!/usr/bin/env python3
"""Canonicalize a graphify graph.json so it is byte-reproducible across machines.

graphify's bash extractor derives the node id for a script's entrypoint from the
file's *absolute* path (`graphify.extract._make_id(str(path)) + "__entry"`), so the
same repo extracted at `/workspace` vs a CI checkout dir produces different
`*__entry` ids — and the edges that reference them differ too. Everything else
graphify emits is already path-relative and its file walk is sorted, so the
entry ids are the only source of cross-checkout drift.

This script rewrites every `*__entry` id from the node's repo-relative
`source_file` (which graphify already stores relative), remaps the edges that
touch those ids, then re-serializes nodes and edges in a canonical sorted order
with stable key ordering. The result is identical for any checkout path, which
is what lets CI diff a freshly generated graph against the committed one.

Usage: graphify-normalize.py <graph.json>   # rewrites in place
"""
import json
import re
import sys
import unicodedata
from pathlib import Path


def make_id(*parts: str) -> str:
    """Mirror graphify.extract._make_id / build._normalize_id (#811).

    NFKC normalize, replace non-word runs with '_', collapse repeats, strip,
    casefold. Kept in sync with graphify so rebuilt ids match what a from-source
    extraction at a relative path would produce.
    """
    combined = "_".join(p.strip("_.") for p in parts if p)
    s = unicodedata.normalize("NFKC", combined)
    cleaned = re.sub(r"[^\w]+", "_", s, flags=re.UNICODE)
    cleaned = re.sub(r"_+", "_", cleaned)
    return cleaned.strip("_").casefold()


def normalize(graph: dict) -> dict:
    nodes = graph.get("nodes", [])
    # NetworkX <= 3.1 serialized edges under "links"; graphify writes "links".
    edge_key = "links" if "links" in graph else "edges"
    edges = graph.get(edge_key, [])

    # Rebuild every absolute-path-derived "*__entry" id from its relative
    # source_file so it no longer encodes the checkout directory.
    remap: dict[str, str] = {}
    for node in nodes:
        nid = node.get("id", "")
        if nid.endswith("__entry"):
            src = node.get("source_file", "")
            new_id = make_id(src) + "__entry" if src else nid
            if new_id != nid:
                remap[nid] = new_id
                node["id"] = new_id

    for edge in edges:
        if edge.get("source") in remap:
            edge["source"] = remap[edge["source"]]
        if edge.get("target") in remap:
            edge["target"] = remap[edge["target"]]

    nodes.sort(key=lambda n: n.get("id", ""))
    edges.sort(
        key=lambda e: (
            e.get("source", ""),
            e.get("target", ""),
            e.get("relation", ""),
            e.get("source_location", ""),
        )
    )
    graph["nodes"] = nodes
    graph[edge_key] = edges
    return graph


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: graphify-normalize.py <graph.json>", file=sys.stderr)
        return 2
    path = Path(sys.argv[1])
    graph = json.loads(path.read_text(encoding="utf-8"))
    graph = normalize(graph)
    # Canonical JSON: sorted keys, fixed separators, trailing newline.
    path.write_text(
        json.dumps(graph, indent=2, sort_keys=True, ensure_ascii=False) + "\n",
        encoding="utf-8",
    )
    print(f"normalized {path} ({len(graph.get('nodes', []))} nodes)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
