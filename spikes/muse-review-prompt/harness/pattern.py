#!/usr/bin/env python3
"""What separates the items Muse always catches from the ones it drops.

Two questions the trial records can answer directly:
  1. On a miss, does it return nothing, or does it return a wrong finding?
     (giving up vs looking in the wrong place)
  2. Does it spend less time on the calls it misses than on the calls it hits
     for the same item? (a search-depth shortfall would show up here)
"""
import json, pathlib, re, statistics as st
import os

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
KEY = json.load(open(pathlib.Path(__file__).resolve().parent / "key.json"))
BUGS = [i for i in KEY if i["level"] != "CLEAN"]


def is_hit(item, f):
    fn = (item["fn"] or "").lower()
    if fn and (fn in str(f.get("symbol", "")).lower() or fn in str(f.get("current_form", "")).lower()):
        return True
    line = f.get("line")
    return item["line"] > 0 and isinstance(line, int) and abs(line - item["line"]) <= 10


trials = {}
for p in sorted(RUN.glob("results/muse-medium-t*.jsonl")):
    label = re.search(r"-(t\d+)\.jsonl$", p.name).group(1)
    recs = {r["item"]: r for r in (json.loads(l) for l in open(p) if l.strip())}
    if len(recs) == len(KEY):
        trials[label] = recs
labels = sorted(trials, key=lambda s: int(s[1:]))

print(f"{'item':22s} {'lvl':4s} {'rate':>5s}  {'miss=empty':>10s} {'miss=wrong':>10s} "
      f"{'hit_s':>7s} {'miss_s':>7s} {'delta':>7s}")
for item in BUGS:
    hit_d, miss_d, empty, wrong = [], [], 0, 0
    for t in labels:
        r = trials[t][item["id"]]
        fs = (r["result"] or {}).get("findings", [])
        if any(is_hit(item, f) for f in fs):
            hit_d.append(r["duration_s"])
        else:
            miss_d.append(r["duration_s"])
            if fs:
                wrong += 1
            else:
                empty += 1
    hm = st.mean(hit_d) if hit_d else None
    mm = st.mean(miss_d) if miss_d else None
    delta = f"{mm - hm:+.1f}s" if (hm is not None and mm is not None) else "-"
    print(f"{item['id']:22s} {item['level']:4s} {len(hit_d)}/{len(labels)}  "
          f"{empty:>10d} {wrong:>10d} "
          f"{(f'{hm:.1f}s' if hm else '-'):>7s} {(f'{mm:.1f}s' if mm else '-'):>7s} {delta:>7s}")

print("\nfindings returned per call, by whether the target was hit:")
hits_n, miss_n = [], []
for item in BUGS:
    for t in labels:
        r = trials[t][item["id"]]
        fs = (r["result"] or {}).get("findings", [])
        (hits_n if any(is_hit(item, f) for f in fs) else miss_n).append(len(fs))
print(f"  on a hit : mean {st.mean(hits_n):.2f} findings over {len(hits_n)} calls")
print(f"  on a miss: mean {st.mean(miss_n):.2f} findings over {len(miss_n)} calls, "
      f"{sum(1 for n in miss_n if n == 0)} returned nothing at all")

print("\nwhat it said instead, on every miss that returned something:")
for item in BUGS:
    for t in labels:
        r = trials[t][item["id"]]
        fs = (r["result"] or {}).get("findings", [])
        if fs and not any(is_hit(item, f) for f in fs):
            for f in fs:
                print(f"  {item['id']:20s} {t}  line={f.get('line')} "
                      f"sym={str(f.get('symbol'))[:38]:38s} {str(f.get('category'))[:24]}")
