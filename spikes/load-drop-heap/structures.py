#!/usr/bin/env python3
"""SPIKE (issue 7414): sum the per-site slopes by the structure that keeps
the bytes.

    structures.py <out-dir> <label> <N>

A site is assigned to the first structure whose pattern matches any frame of
its backtrace. Only sites classed LINEAR or FILLING are summed; the rest is
reported as one NOISE line.
"""
import contextlib, io, re, runpy, sys
from collections import defaultdict

out, label, n = sys.argv[1], sys.argv[2], sys.argv[3]
sys.argv = ["analyze.py", out, label, n, "0"]
with contextlib.redirect_stdout(io.StringIO()):
    g = runpy.run_path(__file__.replace("structures.py", "analyze.py"))
rows, short, classify = g["rows"], g["short"], g["classify"]

STRUCTURES = [
    ("settled-root memory (SettlementRegistry cells)", r"remember_settled \(src/chassis/settlement.rs"),
    ("module manifest: asset index", r"asset_manifest.rs|AssetIndex|new \(wasm/module/mod.rs:127|clone \(wasm/module/mod.rs:118|new \(wasm/module/manifest.rs:(5|6)\d"),
    ("module manifest: kind descriptors (merged schema trees)", r"merge \(actor/wasm/kind_manifest.rs|read_from_bytes \(actor/wasm/kind_manifest.rs|canonical/merge.rs"),
    ("module manifest: actor inputs and lineage", r"read_actor_inputs|read_private_actor_inputs|read_inputs_groups|read_actor_lineage|read_boot|read_namespace"),
    ("module manifest: kind id set and the rest of parse", r"parse \(wasm/module/manifest.rs"),
    ("module cache: entry Arc and weak slot map", r"\(wasm/module/cache.rs"),
    ("publication table: surface, publication Arc, namespace map", r"registry/publication/"),
    ("route record name (ErasedActorPath clone)", r"clone \(src/reference/actor_path.rs"),
    ("route maps: owner `mailboxes` and both double-buffer halves (hash table slots)", r"double_buffer.rs:(38|97)|promote_locked \(registry/mailbox/birth.rs:235|commit_staged"),
    ("route contract rows (Arc<[(KindId, ReplyContract)]>)", r"from_capabilities \(mail/registry/contract.rs"),
    ("actor registry `actors` map slots", r"promote_starting \(src/actor/registry.rs|reserve_starting|ActorRegistry"),
    ("actor registry `tombstones` set", r"mark_dead \(src/actor/registry.rs"),
    ("trace and log rings", r"aether-actor/src/(trace|log).rs"),
]

sums = defaultdict(lambda: [0.0, 0.0, 0.0])
noise = [0.0, 0.0]
other = defaultdict(lambda: [0.0, 0.0, 0.0])
for second, first, block_second, bytes_at, blocks_at, key in rows:
    kind = classify(first, second)
    if kind == "flat":
        continue
    if kind == "NOISE":
        noise[0] += first
        noise[1] += second
        continue
    stack = " | ".join(short(frame) for frame in key)
    for name, pattern in STRUCTURES:
        if re.search(pattern, stack):
            target = sums[name]
            break
    else:
        target = other[g["workspace_owner"](key)]
    target[0] += first
    target[1] += second
    target[2] += block_second

print(f"== {label}: bytes/cycle (N,2N), (2N,4N); blocks/cycle (2N,4N); structure")
total = [0.0, 0.0]
for name, _ in STRUCTURES:
    if name in sums:
        first, second, blocks = sums[name]
        total[0] += first
        total[1] += second
        print(f"{first:10.1f} {second:10.1f} {blocks:8.2f}  {name}")
for name, (first, second, blocks) in sorted(other.items(), key=lambda item: -abs(item[1][1])):
    total[0] += first
    total[1] += second
    if abs(first) >= 1 or abs(second) >= 1:
        print(f"{first:10.1f} {second:10.1f} {blocks:8.2f}  OTHER {name[:150]}")
print(f"{total[0]:10.1f} {total[1]:10.1f}           sum of LINEAR and FILLING sites")
print(f"{noise[0]:10.1f} {noise[1]:10.1f}           NOISE sites (live at one exit, not at another)")
