#!/usr/bin/env python3
"""For every miss, show whether the correct diagnosis was demoted to lintCandidates."""
import json, pathlib, re
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

n = recovered = 0
for item in BUGS:
    for t in labels:
        res = trials[t][item["id"]]["result"] or {}
        if any(is_hit(item, f) for f in res.get("findings", [])):
            continue
        n += 1
        lints = res.get("lintCandidates", [])
        blob = json.dumps(lints).lower()
        fn = (item["fn"] or "").lower()
        near = bool(fn and fn in blob)
        recovered += near
        print(f"{n:2d}. {item['id']:20s} {t}  lints={len(lints)}  diagnosis_demoted={near}")
        for c in lints:
            print(f"      symbol: {c.get('symbol')}")
            print(f"      note:   {str(c.get('note'))[:300]}")

print(f"\n{recovered}/{n} misses had the correct diagnosis sitting in lintCandidates")

print("\n--- for contrast, lintCandidates emitted alongside a HIT ---")
k = 0
for item in BUGS:
    for t in labels:
        res = trials[t][item["id"]]["result"] or {}
        if any(is_hit(item, f) for f in res.get("findings", [])) and res.get("lintCandidates"):
            k += 1
print(f"{k}/50 hits also emitted lintCandidates")

print("\n--- lintCandidates on the four CLEAN controls (all 5 trials) ---")
for item in [i for i in KEY if i["level"] == "CLEAN"]:
    tot = sum(len((trials[t][item["id"]]["result"] or {}).get("lintCandidates", [])) for t in labels)
    print(f"  {item['id']:20s} {tot} lint candidates across {len(labels)} trials")
