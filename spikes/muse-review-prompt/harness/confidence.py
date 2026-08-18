#!/usr/bin/env python3
"""Do the flaky items sit at Muse's reporting threshold?

If the misses are a confidence threshold rather than blindness, the hits on the
unreliable items should carry systematically lower self-reported confidence and
severity than the hits on the items it never drops.
"""
import json, pathlib, re
import os
from collections import Counter

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

catch = {}
for item in BUGS:
    catch[item["id"]] = sum(
        any(is_hit(item, f) for f in (trials[t][item["id"]]["result"] or {}).get("findings", []))
        for t in labels)

print(f"{'item':22s} {'rate':>5s}  {'confidence on its hits':32s} severity")
groups = {"always (5/5)": Counter(), "flaky (<5/5)": Counter()}
sev = {"always (5/5)": Counter(), "flaky (<5/5)": Counter()}
for item in BUGS:
    g = "always (5/5)" if catch[item["id"]] == len(labels) else "flaky (<5/5)"
    conf, svs = Counter(), Counter()
    for t in labels:
        for f in (trials[t][item["id"]]["result"] or {}).get("findings", []):
            if is_hit(item, f):
                conf[str(f.get("confidence"))] += 1
                svs[str(f.get("severity"))] += 1
                groups[g][str(f.get("confidence"))] += 1
                sev[g][str(f.get("severity"))] += 1
    print(f"{item['id']:22s} {catch[item['id']]}/{len(labels)}  {str(dict(conf)):32s} {dict(svs)}")

print()
for g in groups:
    c, s = groups[g], sev[g]
    n = sum(c.values())
    high = 100 * c["high"] / n if n else 0
    print(f"{g:14s} n={n:3d}  confidence {dict(c)}  -> {high:.0f}% high")
    print(f"{'':14s}        severity   {dict(s)}")
