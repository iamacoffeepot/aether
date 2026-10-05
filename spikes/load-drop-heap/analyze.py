#!/usr/bin/env python3
"""SPIKE (issue 7414): per-site slopes from the dhat profiles run.sh wrote.

    analyze.py <out-dir> <label> <N> [top]

Reads <label>-{N,2N,4N}.dhat.json and the three modes' sample series. A
site is one distinct backtrace (frames compared by symbol and source
position, addresses dropped). Its bytes at exit are dhat's `eb`; its slope
over (N, 2N) and over (2N, 4N) is the difference divided by the cycles
between. Sites are also grouped by their first frame inside this workspace.
"""
import json, re, sys
from collections import defaultdict

out, label, n = sys.argv[1], sys.argv[2], int(sys.argv[3])
top = int(sys.argv[4]) if len(sys.argv) > 4 else 40
counts = [n, 2 * n, 4 * n]

import glob, os

def suffixes():
    """Every one-to-three-component path suffix of a workspace source file.
    dhat prints a frame's file as its last three components."""
    found = set()
    for path in glob.glob("crates/**/*.rs", recursive=True) + glob.glob("spikes/**/*.rs", recursive=True):
        parts = path.split(os.sep)
        found.add("/".join(parts[-3:]))
    return found

WORKSPACE = suffixes()
# Three-component suffixes std and the allocator share with nothing here,
# listed so a same-named workspace file cannot claim their frames.
FOREIGN = ("alloc/src/", "core/src/", "std/src/")

def strip(frame):
    return re.sub(r"^0x[0-9a-f]+: ", "", frame)

def file_of(frame):
    match = re.search(r"\(([^()]*?):\d+:\d+\)$", frame)
    return match.group(1) if match else ""

def in_workspace(frame):
    path = file_of(frame)
    return path in WORKSPACE and not path.startswith(FOREIGN)

def load(count):
    with open(f"{out}/{label}-{count}.dhat.json") as handle:
        data = json.load(handle)
    table = [strip(frame) for frame in data["ftbl"]]
    sites = defaultdict(lambda: [0, 0])
    for point in data["pps"]:
        key = tuple(table[index] for index in point["fs"])
        sites[key][0] += point.get("eb", 0)
        sites[key][1] += point.get("ebk", 0)
    return sites

def short(frame):
    """`name (file:line)`, generics and the column dropped."""
    name = re.sub(r"<.*", "", frame.split(" (")[0])
    return f"{name} ({file_of(frame)}:{frame.rsplit(':', 2)[-2]})"

def workspace_owner(key):
    """The innermost three workspace frames: the allocating line and its
    two callers."""
    frames = [short(frame) for frame in key if in_workspace(frame)]
    if not frames:
        outer = [frame for frame in key if frame != "[root]"]
        return "(no workspace frame) " + (short(outer[-1]) if outer else "?")
    return " <- ".join(frames[:3])

profiles = [load(count) for count in counts]
keys = set().union(*profiles)

def slopes(get):
    first = (get(1) - get(0)) / (counts[1] - counts[0])
    second = (get(2) - get(1)) / (counts[2] - counts[1])
    return first, second

def classify(first, second):
    """flat: no slope. LINEAR: both slopes positive and within a factor of
    two. FILLING: growth that slows to under half. NOISE: anything else,
    which is a block that happened to be live at one exit."""
    if abs(first) < 0.05 and abs(second) < 0.05:
        return "flat"
    if first > 0 and second > 0 and 0.5 * first <= second <= 2 * first:
        return "LINEAR"
    if first > 0 and 0 <= second < 0.5 * first:
        return "FILLING"
    return "NOISE"

rows = []
for key in keys:
    bytes_at = [profile[key][0] if key in profile else 0 for profile in profiles]
    blocks_at = [profile[key][1] if key in profile else 0 for profile in profiles]
    first, second = slopes(lambda i: bytes_at[i])
    block_first, block_second = slopes(lambda i: blocks_at[i])
    rows.append((second, first, block_second, bytes_at, blocks_at, key))

total = [sum(profile[key][0] for key in profile) for profile in profiles]
total_blocks = [sum(profile[key][1] for key in profile) for profile in profiles]
print(f"== {label}: counts {counts}")
print(f"dhat live bytes at exit: {total}  blocks: {total_blocks}")
first, second = slopes(lambda i: total[i])
block_first, block_second = slopes(lambda i: total_blocks[i])
print(f"heap slope bytes/cycle: (N,2N) {first:.1f}  (2N,4N) {second:.1f}   blocks/cycle: {block_first:.2f} {block_second:.2f}")

def series(mode, count):
    try:
        with open(f"{out}/{label}-{count}.{mode}.series.json") as handle:
            return json.load(handle)
    except (OSError, ValueError):
        return None

def reading(sample):
    smaps = sample["smaps"]
    return {
        "rss": sample["rss_kib"] * 1024,
        "in_use": sample["malloc_in_use"] + sample["malloc_mmapped"],
        "free": sample["malloc_free"],
        "heap_rss": smaps.get("heap", {}).get("rss_kib", 0) * 1024,
        "anon_rw_rss": smaps.get("anon_rw", {}).get("rss_kib", 0) * 1024,
        "anon_rw_maps": smaps.get("anon_rw", {}).get("mappings", 0),
        "anon_exec_rss": smaps.get("anon_exec", {}).get("rss_kib", 0) * 1024,
        "anon_ro_rss": smaps.get("anon_ro", {}).get("rss_kib", 0) * 1024,
        "file_rss": smaps.get("file", {}).get("rss_kib", 0) * 1024,
        "memfd_rss": smaps.get("memfd", {}).get("rss_kib", 0) * 1024,
        "maps": sum(entry["mappings"] for entry in smaps.values()),
        "dhat": sample.get("dhat_bytes", 0),
    }

for mode in ("dhat", "plain", "arena1"):
    runs = [series(mode, count) for count in counts]
    if any(run is None for run in runs):
        print(f"-- {mode}: missing series")
        continue
    print(f"-- {mode}: exit readings across runs (bytes), then slope per cycle over (N,2N) and (2N,4N)")
    ends = [reading(run["series"][-1]) for run in runs]
    trims = [reading(run["after_trim"]) for run in runs]
    for field in ends[0]:
        values = [end[field] for end in ends]
        first, second = slopes(lambda i: values[i])
        trimmed = [trim[field] for trim in trims]
        trim_first, trim_second = slopes(lambda i: trimmed[i])
        print(f"   {field:14} {values}  slope {first:9.1f} {second:9.1f}   after malloc_trim slope {trim_first:9.1f} {trim_second:9.1f}")
    longest = runs[-1]
    print(f"   within the {counts[-1]}-cycle run (cycles: rss, malloc in_use+mmapped, dhat live), slope from the previous sample:")
    previous = None
    for sample in longest["series"]:
        now = reading(sample)
        line = f"   {sample['cycles']:6} rss {now['rss']:12} in_use {now['in_use']:12} dhat {now['dhat']:12}"
        if previous is not None:
            cycles = sample["cycles"] - previous[0]
            line += f"   d_rss {(now['rss'] - previous[1]['rss']) / cycles:10.1f} d_in_use {(now['in_use'] - previous[1]['in_use']) / cycles:9.1f} d_dhat {(now['dhat'] - previous[1]['dhat']) / cycles:9.1f}"
        print(line)
        previous = (sample["cycles"], now)

print(f"\n-- by first workspace frame (bytes/cycle (N,2N), (2N,4N); blocks/cycle (2N,4N); class)")
groups = defaultdict(lambda: [[0, 0, 0], [0, 0, 0]])
for second, first, block_second, bytes_at, blocks_at, key in rows:
    group = groups[workspace_owner(key)]
    for index in range(3):
        group[0][index] += bytes_at[index]
        group[1][index] += blocks_at[index]
ordered = []
for name, (bytes_at, blocks_at) in groups.items():
    first, second = slopes(lambda i: bytes_at[i])
    _, block_second = slopes(lambda i: blocks_at[i])
    ordered.append((second, first, block_second, name, bytes_at))
ordered.sort(key=lambda row: -abs(row[0]) - abs(row[1]))
sums = defaultdict(lambda: [0.0, 0.0])
for second, first, block_second, name, bytes_at in ordered:
    kind = classify(first, second)
    sums[kind][0] += first
    sums[kind][1] += second
for second, first, block_second, name, bytes_at in ordered[:top]:
    print(f"{first:10.1f} {second:10.1f} {block_second:7.2f}  {classify(first, second):8} {name}   {bytes_at}")
for kind, (first, second) in sorted(sums.items()):
    print(f"   class {kind:8} sum over all groups: (N,2N) {first:10.1f}  (2N,4N) {second:10.1f} bytes/cycle")

print(f"\n-- top {top} sites by slope, full trimmed backtrace")
rows.sort(key=lambda row: -abs(row[0]) - abs(row[1]))
for second, first, block_second, bytes_at, blocks_at, key in rows[:top]:
    print(f"\n{first:10.1f} {second:10.1f} bytes/cycle  {block_second:.2f} blocks/cycle  {classify(first, second)}  bytes {bytes_at} blocks {blocks_at}")
    for frame in key:
        mark = "*" if in_workspace(frame) else " "
        print(f"    {mark} {short(frame)}")
