#!/usr/bin/env python3
"""Render the Muse effort matrix against the calibration answer key.

Hit rule ported verbatim from the original sweep's grade(): a finding matches
when its symbol or current_form contains the key's fn name, or its line falls
within +/-10 of the key's line. CLEAN items can only accrue false positives.
"""
import json, pathlib, statistics as st
import os

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
KEY = [json.loads(json.dumps(i)) for i in json.load(open(pathlib.Path(__file__).resolve().parent / "key.json"))]
BYID = {i["id"]: i for i in KEY}
LADDER = ["none", "minimal", "low", "medium", "high", "xhigh", "ultra"]


def is_hit(item, f):
    fn = (item["fn"] or "").lower()
    if fn and (fn in str(f.get("symbol", "")).lower() or fn in str(f.get("current_form", "")).lower()):
        return True
    line = f.get("line")
    return item["line"] > 0 and isinstance(line, int) and abs(line - item["line"]) <= 10


def load(effort):
    p = RUN / "results" / f"muse-{effort}.jsonl"
    if not p.exists():
        return None
    return {r["item"]: r for r in (json.loads(l) for l in open(p) if l.strip())}


cells = {e: load(e) for e in LADDER}
present = [e for e in LADDER if cells[e]]

hdr = f"{'item':22s} {'lvl':5s} " + " ".join(f"{e[:7]:>7s}" for e in present)
print("=" * len(hdr)); print(hdr); print("=" * len(hdr))
for item in KEY:
    row = f"{item['id']:22s} {item['level']:5s} "
    marks = []
    for e in present:
        r = cells[e].get(item["id"])
        if not r:
            marks.append(f"{'-':>7s}"); continue
        fs = (r["result"] or {}).get("findings", [])
        if item["level"] == "CLEAN":
            marks.append(f"{('FP' if fs else 'ok'):>7s}")
        else:
            marks.append(f"{('HIT' if any(is_hit(item, f) for f in fs) else '.'):>7s}")
    print(row + " ".join(marks))
print("=" * len(hdr))

print(f"\n{'effort':9s} {'recall':>13s} {'FP':>4s} {'parsefail':>10s} "
      f"{'mean':>7s} {'med':>7s} {'p90':>7s} {'max':>8s} {'total':>8s}")
summary = {}
for e in present:
    recs = cells[e]
    bugs = hits = fps = 0
    durs = []
    misses = []
    for item in KEY:
        r = recs.get(item["id"])
        if not r:
            continue
        durs.append(r["duration_s"])
        fs = (r["result"] or {}).get("findings", [])
        if item["level"] == "CLEAN":
            fps += len(fs)
        else:
            bugs += 1
            h = any(is_hit(item, f) for f in fs)
            hits += h
            fps += len([f for f in fs if not is_hit(item, f)])
            if not h:
                misses.append(f"{item['id']}({item['level']})")
    d = sorted(durs)
    p90 = d[max(0, int(.9 * len(d)) - 1)] if d else 0
    pf = sum(1 for r in recs.values() if not r["parsed"])
    summary[e] = {"recall": f"{hits}/{bugs}", "pct": round(100 * hits / bugs, 1) if bugs else None,
                  "fp": fps, "parse_failures": pf, "mean_s": round(st.mean(durs), 1),
                  "median_s": round(st.median(durs), 1), "p90_s": p90, "max_s": max(durs),
                  "total_s": round(sum(durs), 1), "misses": misses}
    s = summary[e]
    print(f"{e:9s} {s['recall'] + f' ({s[chr(112)+chr(99)+chr(116)]}%)':>13s} {fps:>4d} {pf:>10d} "
          f"{s['mean_s']:>6.1f}s {s['median_s']:>6.1f}s {p90:>6.1f}s {s['max_s']:>7.1f}s {s['total_s']:>7.1f}s")

print("\nmisses by effort:")
for e in present:
    print(f"  {e:9s} {summary[e]['misses']}")

json.dump(summary, open(RUN / "results" / "muse_matrix.json", "w"), indent=2)
