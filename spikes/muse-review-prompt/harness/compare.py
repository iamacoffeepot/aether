#!/usr/bin/env python3
"""Baseline prompt vs revised prompt, same model, same effort, same dataset."""
import json, pathlib, re, statistics as st
import os

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
KEY = json.load(open(pathlib.Path(__file__).resolve().parent / "key.json"))
BUGS = [i for i in KEY if i["level"] != "CLEAN"]
CLEAN = [i for i in KEY if i["level"] == "CLEAN"]


def is_hit(item, f):
    fn = (item["fn"] or "").lower()
    if fn and (fn in str(f.get("symbol", "")).lower() or fn in str(f.get("current_form", "")).lower()):
        return True
    line = f.get("line")
    return item["line"] > 0 and isinstance(line, int) and abs(line - item["line"]) <= 10


ARMS = ("v1", "v2", "v3")
arms = {a: [] for a in ARMS}
trials = {}
for p in sorted(RUN.glob("results/muse-medium-*.jsonl")):
    label = re.search(r"muse-medium-(.+)\.jsonl$", p.name).group(1)
    recs = {r["item"]: r for r in (json.loads(l) for l in open(p) if l.strip())}
    if len(recs) != len(KEY):
        print(f"note: skipping {p.name} — {len(recs)}/{len(KEY)} items (incomplete)")
        continue
    trials[label] = recs
    arms[next((a for a in ("v2", "v3") if label.startswith(a)), "v1")].append(label)

for a in arms:
    arms[a].sort()
order = [t for a in ARMS for t in arms[a]]

hdr = f"{'item':22s} {'lvl':5s} " + "  ".join(f"{t:>4s}" for t in order)
print("=" * len(hdr)); print(hdr)
armof = {t: a for a in ARMS for t in arms[a]}
print(f"{'':28s} " + "  ".join(f"{armof[t]:>4s}" for t in order))
print("=" * len(hdr))
for item in BUGS + CLEAN:
    marks = []
    for t in order:
        fs = (trials[t][item["id"]]["result"] or {}).get("findings", [])
        if item["level"] == "CLEAN":
            marks.append(f"{('FP' if fs else 'ok'):>4s}")
        else:
            marks.append(f"{('HIT' if any(is_hit(item, f) for f in fs) else '.'):>4s}")
    print(f"{item['id']:22s} {item['level']:5s} " + "  ".join(marks))
print("=" * len(hdr))


def score(t):
    recs = trials[t]
    hits = sum(any(is_hit(i, f) for f in (recs[i["id"]]["result"] or {}).get("findings", [])) for i in BUGS)
    fps = sum(len([f for f in (recs[i["id"]]["result"] or {}).get("findings", []) if not is_hit(i, f)]) for i in BUGS)
    fps += sum(len((recs[i["id"]]["result"] or {}).get("findings", [])) for i in CLEAN)
    durs = [r["duration_s"] for r in recs.values()]
    return hits, fps, st.mean(durs), max(durs)


print(f"\n{'trial':6s} {'arm':4s} {'recall':>14s} {'FP':>4s} {'mean':>7s} {'max':>7s}   misses")
for t in order:
    h, fp, mean, mx = score(t)
    miss = [i["id"] for i in BUGS if not any(is_hit(i, f) for f in (trials[t][i["id"]]["result"] or {}).get("findings", []))]
    arm = armof[t]
    print(f"{t:6s} {arm:4s} {f'{h}/{len(BUGS)} ({100*h/len(BUGS):.1f}%)':>14s} {fp:>4d} "
          f"{mean:>6.1f}s {mx:>6.1f}s   {', '.join(miss) or '-'}")

print()
for a in ARMS:
    if not arms[a]:
        continue
    pcts = [100 * score(t)[0] / len(BUGS) for t in arms[a]]
    fps = sum(score(t)[1] for t in arms[a])
    print(f"{a}: {len(arms[a])} trials, mean recall {st.mean(pcts):.1f}%"
          + (f" (sd {st.stdev(pcts):.1f}pp)" if len(pcts) > 1 else "")
          + f", range {min(pcts):.1f}-{max(pcts):.1f}%, {fps} FP total")

print("\nper-item catch rate by arm:")
for item in BUGS:
    row = []
    for a in ARMS:
        n = sum(any(is_hit(item, f) for f in (trials[t][item["id"]]["result"] or {}).get("findings", []))
                for t in arms[a])
        row.append(f"{n}/{len(arms[a])}" if arms[a] else "-")
    print(f"  {item['id']:22s} {item['level']:5s} " + "   ".join(f"{a} {r:>4s}" for a, r in zip(ARMS, row)))
