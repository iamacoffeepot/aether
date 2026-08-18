#!/usr/bin/env python3
"""Grade every medium trial against the calibration answer key.

Same hit rule as matrix.py (ported verbatim from the original sweep's grade()).
Reports per-trial recall, per-item catch rate across trials, and the pooled
mean so a single lucky or unlucky cell cannot carry the conclusion.
"""
import json, pathlib, re, statistics as st
import os

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
KEY = json.load(open(pathlib.Path(__file__).resolve().parent / "key.json"))
BUGS = [i for i in KEY if i["level"] != "CLEAN"]
CLEAN = [i for i in KEY if i["level"] == "CLEAN"]


# Sonnet's CLI arm answered these two in correct prose instead of the JSON
# object, so the parser scores them as misses. Adjudicated as hits, matching
# the original agent()-based sweep where a forced schema made the contract moot.
SONNET_ADJUDICATED = {"r1_config_cap", "r2_fuel_order"}


def is_hit(item, f):
    fn = (item["fn"] or "").lower()
    if fn and (fn in str(f.get("symbol", "")).lower() or fn in str(f.get("current_form", "")).lower()):
        return True
    line = f.get("line")
    return item["line"] > 0 and isinstance(line, int) and abs(line - item["line"]) <= 10


def findings(rec):
    return (rec["result"] or {}).get("findings", []) if rec else []


trials = {}
for p in sorted(RUN.glob("results/muse-medium-t*.jsonl")):
    label = re.search(r"-(t\d+)\.jsonl$", p.name).group(1)
    recs = {r["item"]: r for r in (json.loads(l) for l in open(p) if l.strip())}
    if len(recs) == len(KEY):
        trials[label] = recs
    else:
        print(f"note: skipping {p.name} — {len(recs)}/{len(KEY)} items (incomplete)")

sonnet = {r["item"]: r for r in (json.loads(l) for l in open(RUN / "results/sonnet.jsonl") if l.strip())}
labels = sorted(trials, key=lambda s: int(s[1:]))
if not labels:
    raise SystemExit("no complete medium trials found")

hdr = f"{'item':22s} {'lvl':5s} " + " ".join(f"{t:>5s}" for t in labels) + f" {'rate':>6s}  sonnet"
print("=" * len(hdr)); print(hdr); print("=" * len(hdr))
catch = {}
for item in BUGS + CLEAN:
    marks, hits = [], 0
    for t in labels:
        fs = findings(trials[t].get(item["id"]))
        if item["level"] == "CLEAN":
            marks.append(f"{('FP' if fs else 'ok'):>5s}")
        else:
            h = any(is_hit(item, f) for f in fs)
            hits += h
            marks.append(f"{('HIT' if h else '.'):>5s}")
    catch[item["id"]] = hits
    sfs = findings(sonnet.get(item["id"]))
    if item["level"] == "CLEAN":
        smark = "FP" if sfs else "ok"
    elif any(is_hit(item, f) for f in sfs):
        smark = "HIT"
    else:
        smark = "HIT*" if item["id"] in SONNET_ADJUDICATED else "."
    rate = "-" if item["level"] == "CLEAN" else f"{hits}/{len(labels)}"
    print(f"{item['id']:22s} {item['level']:5s} " + " ".join(marks) + f" {rate:>6s}  {smark}")
print("=" * len(hdr))
print("* adjudicated: correct prose diagnosis that broke the JSON contract")

print(f"\n{'trial':6s} {'recall':>14s} {'FP':>4s} {'pf':>3s} {'mean':>7s} {'med':>7s} {'max':>7s}   misses")
pcts = []
for t in labels:
    recs = trials[t]
    hits = sum(any(is_hit(i, f) for f in findings(recs.get(i["id"]))) for i in BUGS)
    fps = sum(len([f for f in findings(recs.get(i["id"])) if not is_hit(i, f)]) for i in BUGS)
    fps += sum(len(findings(recs.get(i["id"]))) for i in CLEAN)
    pf = sum(1 for r in recs.values() if not r["parsed"])
    durs = [r["duration_s"] for r in recs.values()]
    misses = [i["id"] for i in BUGS if not any(is_hit(i, f) for f in findings(recs.get(i["id"])))]
    pct = 100 * hits / len(BUGS)
    pcts.append(pct)
    print(f"{t:6s} {f'{hits}/{len(BUGS)} ({pct:.1f}%)':>14s} {fps:>4d} {pf:>3d} "
          f"{st.mean(durs):>6.1f}s {st.median(durs):>6.1f}s {max(durs):>6.1f}s   {', '.join(misses) or '-'}")

n = len(labels)
print(f"\npooled over {n} trials: mean recall {st.mean(pcts):.1f}%"
      + (f", sd {st.stdev(pcts):.1f}pp" if n > 1 else "")
      + f", range {min(pcts):.1f}-{max(pcts):.1f}%")
print(f"items caught every trial: {sum(1 for i in BUGS if catch[i['id']] == n)}/{len(BUGS)}")
print("unreliable items (caught in some trials, missed in others):")
for i in BUGS:
    if 0 < catch[i["id"]] < n:
        print(f"  {i['id']:22s} {catch[i['id']]}/{n}  level={i['level']}")
print("never caught:")
for i in BUGS:
    if catch[i["id"]] == 0:
        print(f"  {i['id']:22s} 0/{n}  level={i['level']}")

json.dump({"trials": labels, "per_item_catch": catch, "pcts": pcts},
          open(RUN / "results" / "medium_trials.json", "w"), indent=2)
