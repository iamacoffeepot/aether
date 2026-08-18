#!/usr/bin/env python3
"""Run one review-finder arm over the 16-item calibration dataset.

Both arms get: the production finder prompt verbatim, the same seeded worktree
as cwd with read-only file tools, the same appended JSON output contract, and
one sequential call per item so wall-clock is a per-call measurement.
"""
import json, os, subprocess, sys, time, pathlib

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
SEEDED = os.environ.get("MUSE_SEEDED_TREE", "")  # checkout of the dataset tree state
TIMEOUT = 900

JSON_CONTRACT = """

OUTPUT FORMAT — return ONLY a single JSON object as your final message. No prose around it, no markdown fence:
{"file": string, "lens": string, "findings": [{"symbol": string, "line": integer, "category": string, "severity": "high"|"medium"|"low", "confidence": "high"|"medium"|"low", "recommendation": "fix"|"remove"|"rewrite"|"promote-lint", "current_form": string, "suggested_form": string, "rationale": string}], "lintCandidates": [{"symbol": string, "note": string}]}
If the file is clean under this lens, return the object with an empty findings array."""

ARMS = {
    "muse": lambda pf: [
        "muse", "exec", "--prompt-file", pf,
        "--model", "muse-spark-1.2-contributor", "--reasoning-effort", "high",
        "--workspace", SEEDED,
        "--disable-approval", "--disable-write", "--disable-shell",
        "--disable-web-tools", "--no-session-log",
    ],
    "sonnet": lambda pf: [
        "claude", "-p", open(pf).read(),
        "--model", "sonnet", "--effort", "high",
        "--allowedTools", "Read,Grep,Glob",
        "--output-format", "json",
    ],
}


def extract_json(text):
    """Last balanced {...} in the text that parses and carries a findings array."""
    best = None
    for start in range(len(text)):
        if text[start] != "{":
            continue
        depth, instr, esc = 0, False, False
        for i in range(start, len(text)):
            c = text[i]
            if instr:
                if esc: esc = False
                elif c == "\\": esc = True
                elif c == '"': instr = False
                continue
            if c == '"': instr = True
            elif c == "{": depth += 1
            elif c == "}":
                depth -= 1
                if depth == 0:
                    try:
                        obj = json.loads(text[start:i + 1])
                    except Exception:
                        break
                    if isinstance(obj, dict) and isinstance(obj.get("findings"), list):
                        best = obj
                    break
    return best


def main():
    arm = sys.argv[1]
    key = json.load(open(pathlib.Path(__file__).resolve().parent / "key.json"))
    outdir = RUN / "results"; outdir.mkdir(exist_ok=True)
    rawdir = RUN / "raw" / arm; rawdir.mkdir(parents=True, exist_ok=True)
    out = open(outdir / f"{arm}.jsonl", "w", buffering=1)

    for item in key:
        iid = item["id"]
        prompt = (RUN / os.environ.get("PROMPTS", "prompts/v1") / f"{iid}.txt").read_text() + JSON_CONTRACT
        pf = RUN / "results" / f"{iid}.{arm}.built.txt"
        pf.write_text(prompt)

        t0 = time.time()
        try:
            p = subprocess.run(ARMS[arm](str(pf)), cwd=SEEDED, capture_output=True,
                               text=True, timeout=TIMEOUT)
            stdout, stderr, rc = p.stdout, p.stderr, p.returncode
        except subprocess.TimeoutExpired:
            stdout, stderr, rc = "", "TIMEOUT", -9
        dur = time.time() - t0

        (rawdir / f"{iid}.stdout").write_text(stdout)
        (rawdir / f"{iid}.stderr").write_text(stderr[-4000:])

        payload, meta = stdout, {}
        if arm == "sonnet":
            try:
                env = json.loads(stdout)
                payload = env.get("result", "") or ""
                mu = (env.get("modelUsage") or {}).get("claude-sonnet-5", {})
                meta = {"cli_duration_ms": env.get("duration_ms"),
                        "cost_usd": env.get("total_cost_usd") or mu.get("costUSD"),
                        "input_tokens": mu.get("inputTokens"),
                        "output_tokens": mu.get("outputTokens"),
                        "cache_read_tokens": mu.get("cacheReadInputTokens"),
                        "cache_create_tokens": mu.get("cacheCreationInputTokens")}
            except Exception:
                payload = stdout

        result = extract_json(payload)
        rec = {"arm": arm, "item": iid, "level": item["level"], "lens": item["lens"],
               "duration_s": round(dur, 1), "rc": rc, "parsed": result is not None,
               "result": result, **meta}
        out.write(json.dumps(rec) + "\n")
        print(f"[{arm}] {iid:22s} {dur:6.1f}s rc={rc} parsed={result is not None} "
              f"findings={len(result['findings']) if result else '-'}", flush=True)

    out.close()


if __name__ == "__main__":
    main()
