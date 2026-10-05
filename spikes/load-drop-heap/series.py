#!/usr/bin/env python3
"""SPIKE (issue 7414): one run's sample series, and its slope by quarter.

    series.py <series.json> [every]

Columns are bytes; `in_use` is mallinfo2's in-use plus mmapped bytes over
every arena, block headers included.
"""
import json, sys

data = json.load(open(sys.argv[1]))
every = int(sys.argv[2]) if len(sys.argv) > 2 else 1
series = data["series"]

def row(sample):
    smaps = sample["smaps"]
    return {
        "cycles": sample["cycles"],
        "rss": sample["rss_kib"] * 1024,
        "in_use": sample["malloc_in_use"] + sample["malloc_mmapped"],
        "free": sample["malloc_free"],
        "heap_rss": smaps.get("heap", {}).get("rss_kib", 0) * 1024,
        "anon_rw_rss": smaps.get("anon_rw", {}).get("rss_kib", 0) * 1024,
        "dhat": sample.get("dhat_bytes", 0),
    }

rows = [row(sample) for sample in series]
flags = {key: data[key] for key in ("cell", "addressed", "live", "assets", "payload", "count")}
print(f"== {sys.argv[1]}  {flags}")
print(f"{'cycles':>7} {'rss':>12} {'in_use':>12} {'free':>12} {'heap_rss':>12} {'anon_rw_rss':>12} {'dhat':>12}")
for index, entry in enumerate(rows):
    if index % every == 0 or index == len(rows) - 1:
        print(" ".join(f"{entry[key]:>{7 if key == 'cycles' else 12}}" for key in entry))

def fit(points, key):
    """Least-squares slope of `key` against cycles."""
    count = len(points)
    mean_x = sum(point["cycles"] for point in points) / count
    mean_y = sum(point[key] for point in points) / count
    over = sum((point["cycles"] - mean_x) * (point[key] - mean_y) for point in points)
    under = sum((point["cycles"] - mean_x) ** 2 for point in points)
    return over / under if under else 0.0

quarter = max(len(rows) // 4, 2)
print("least-squares slope, bytes per cycle, by quarter of the run:")
for part in range(4):
    points = rows[part * quarter : (part + 1) * quarter + 1]
    if len(points) < 2:
        continue
    span = f"{points[0]['cycles']}..{points[-1]['cycles']}"
    print(f"   {span:>14}  " + "  ".join(f"{key} {fit(points, key):10.1f}" for key in ("rss", "in_use", "heap_rss", "anon_rw_rss", "dhat")))
trim = row(data["after_trim"])
print(f"after malloc_trim: rss {trim['rss']}  (released {rows[-1]['rss'] - trim['rss']})")
