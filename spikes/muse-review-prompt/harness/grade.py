#!/usr/bin/env python3
"""Grade both arms against the calibration answer key.

The hit rule is ported verbatim from the original sweep's grade(): a finding
matches when its symbol or current_form contains the key's fn name, or its line
is within +/-10 of the key's line. CLEAN items can only accrue false positives.
"""
import json, pathlib, statistics as st
import os

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
KEY = {i["id"]: i for i in json.load(open(pathlib.Path(__file__).resolve().parent / "key.json"))}


def grade(item, result):
    findings = (result or {}).get("findings") or []
    fn = (item["fn"] or "").lower()

    def is_hit(f):
        if fn and (fn in str(f.get("symbol", "")).lower() or fn in str(f.get("current_form", "")).lower()):
            return True
        line = f.get("line")
        return item["line"] > 0 and isinstance(line, int) and abs(line - item["line"]) <= 10

    hits = [] if item["level"] == "CLEAN" else [f for f in findings if is_hit(f)]
    fps = [f for f in findings if f not in hits]
    return bool(hits), len(fps), findings


def load(arm):
    p = RUN / "results" / f"{arm}.jsonl"
    return [json.loads(l) for l in open(p) if l.strip()] if p.exists() else []


rows, summary = [], {}
for arm in ("muse", "sonnet"):
    recs = load(arm)
    if not recs:
        continue
    bugs = fps = hitc = 0
    durs, costs = [], []
    for r in recs:
        item = KEY[r["item"]]
        hit, fpc, findings = grade(item, r.get("result"))
        durs.append(r["duration_s"])
        if r.get("cost_usd"):
            costs.append(r["cost_usd"])
        if item["level"] != "CLEAN":
            bugs += 1
            hitc += hit
        fps += fpc
        rows.append({"arm": arm, "item": r["item"], "level": item["level"],
                     "hit": hit, "fp": fpc, "duration_s": r["duration_s"],
                     "parsed": r["parsed"], "n_findings": len(findings)})
    summary[arm] = {
        "recall": f"{hitc}/{bugs}" + (f" ({100*hitc/bugs:.1f}%)" if bugs else ""),
        "false_positives": fps,
        "parse_failures": sum(1 for r in recs if not r["parsed"]),
        "mean_s": round(st.mean(durs), 1),
        "median_s": round(st.median(durs), 1),
        "max_s": round(max(durs), 1),
        "total_wall_s": round(sum(durs), 1),
        "total_cost_usd": round(sum(costs), 4) if costs else None,
    }

print("=" * 78)
print(f"{'item':24s} {'level':6s} {'muse':>18s} {'sonnet':>18s}")
print("=" * 78)
by = {}
for r in rows:
    by.setdefault(r["item"], {})[r["arm"]] = r
for iid in KEY:
    if iid not in by:
        continue
    cells = []
    for arm in ("muse", "sonnet"):
        c = by[iid].get(arm)
        if not c:
            cells.append(f"{'-':>18s}"); continue
        mark = "HIT " if c["hit"] else ("--  " if KEY[iid]["level"] != "CLEAN" else "ok  ")
        if KEY[iid]["level"] == "CLEAN" and c["fp"]:
            mark = "FP  "
        cells.append(f"{mark}{c['duration_s']:>6.1f}s fp={c['fp']:<2d}")
    print(f"{iid:24s} {KEY[iid]['level']:6s} {cells[0]:>18s} {cells[1]:>18s}")
print("=" * 78)
print(json.dumps(summary, indent=2))
json.dump({"summary": summary, "rows": rows}, open(RUN / "results" / "graded.json", "w"), indent=2)
