#!/usr/bin/env python3
"""Extract per-box usage from fleet session transcripts (issue #3264 spike)."""
import json, glob, os, sys

meta = {}
with open("transcripts.jsonl") as f:
    for line in f:
        m = json.loads(line)
        meta[str(m["id"])] = m

rows = []
for path in glob.glob("jsonl/*/*.jsonl"):
    art_id = path.split("/")[1]
    m = meta.get(art_id, {})
    result = None
    first_main = None      # first assistant usage on a non-haiku model
    first_any = None       # first assistant usage at all
    main_model = None
    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            try:
                ev = json.loads(line)
            except json.JSONDecodeError:
                continue
            if ev.get("type") == "assistant":
                msg = ev.get("message") or {}
                u = msg.get("usage")
                model = msg.get("model", "")
                if u:
                    if first_any is None:
                        first_any = (model, u)
                    if first_main is None and "haiku" not in model:
                        first_main = (model, u)
                        main_model = model
            elif ev.get("type") == "result":
                result = ev

    if result is None:
        rows.append({"artifact": art_id, "name": m.get("name"), "created_at": m.get("created_at"),
                     "run_id": m.get("run_id"), "no_result": True})
        continue

    u = result.get("usage") or {}
    cc = u.get("cache_creation") or {}
    row = {
        "artifact": art_id,
        "name": m.get("name"),
        "created_at": m.get("created_at"),
        "run_id": m.get("run_id"),
        "num_turns": result.get("num_turns"),
        "cost_usd": result.get("total_cost_usd"),
        "is_error": result.get("is_error"),
        "input": u.get("input_tokens", 0),
        "cache_write": u.get("cache_creation_input_tokens", 0),
        "cache_write_1h": cc.get("ephemeral_1h_input_tokens", 0),
        "cache_write_5m": cc.get("ephemeral_5m_input_tokens", 0),
        "cache_read": u.get("cache_read_input_tokens", 0),
        "output": u.get("output_tokens", 0),
        "main_model": main_model,
        "first_call_model": first_main[0] if first_main else None,
        "first_call_cache_read": (first_main[1].get("cache_read_input_tokens", 0) if first_main else None),
        "first_call_cache_write": (first_main[1].get("cache_creation_input_tokens", 0) if first_main else None),
        "first_call_input": (first_main[1].get("input_tokens", 0) if first_main else None),
    }
    rows.append(row)

with open("boxes.jsonl", "w") as f:
    for r in rows:
        f.write(json.dumps(r) + "\n")
print(f"{len(rows)} boxes extracted, {sum(1 for r in rows if r.get('no_result'))} without a result record")
