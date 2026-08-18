#!/usr/bin/env python3
"""Matrix sweep of the review finder over Muse's reasoning-effort ladder.

One effort level at a time, one item at a time within a level, so every
duration is a clean per-call measurement uncontended by our own concurrency.
Same 16-item dataset, same production finder prompts, same seeded worktree and
JSON output contract as the muse/sonnet comparison.
"""
import json, os, subprocess, sys, time, pathlib

RUN = pathlib.Path(os.environ.get("MUSE_RUN_DIR", pathlib.Path(__file__).resolve().parent.parent))
SEEDED = os.environ.get("MUSE_SEEDED_TREE", "")  # checkout of the dataset tree state
MODEL = "muse-spark-1.2-contributor"
TRIAL = os.environ.get("TRIAL", "")
PROMPTS = RUN / os.environ.get("PROMPTS", "prompts/v1")
SUFFIX = ("-" + TRIAL) if TRIAL else ""
TIMEOUT = 1800

JSON_CONTRACT = """

OUTPUT FORMAT — return ONLY a single JSON object as your final message. No prose around it, no markdown fence:
{"file": string, "lens": string, "findings": [{"symbol": string, "line": integer, "category": string, "severity": "high"|"medium"|"low", "confidence": "high"|"medium"|"low", "recommendation": "fix"|"remove"|"rewrite"|"promote-lint", "current_form": string, "suggested_form": string, "rationale": string}], "lintCandidates": [{"symbol": string, "note": string}]}
If the file is clean under this lens, return the object with an empty findings array."""


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


def run_effort(effort, key):
    rawdir = RUN / "raw" / f"muse-{effort}{SUFFIX}"; rawdir.mkdir(parents=True, exist_ok=True)
    out = open(RUN / "results" / f"muse-{effort}{SUFFIX}.jsonl", "w", buffering=1)

    for item in key:
        iid = item["id"]
        pf = RUN / "results" / f"{iid}.muse-{effort}{SUFFIX}.built.txt"
        pf.write_text((PROMPTS / f"{iid}.txt").read_text() + JSON_CONTRACT)

        argv = ["muse", "exec", "--prompt-file", str(pf),
                "--model", MODEL, "--reasoning-effort", effort,
                "--workspace", SEEDED,
                "--disable-approval", "--disable-write", "--disable-shell",
                "--disable-web-tools", "--no-session-log"]

        t0 = time.time()
        try:
            p = subprocess.run(argv, cwd=SEEDED, capture_output=True, text=True, timeout=TIMEOUT)
            stdout, stderr, rc = p.stdout, p.stderr, p.returncode
        except subprocess.TimeoutExpired:
            stdout, stderr, rc = "", "TIMEOUT", -9
        dur = time.time() - t0

        (rawdir / f"{iid}.stdout").write_text(stdout)
        (rawdir / f"{iid}.stderr").write_text(stderr[-4000:])

        result = extract_json(stdout)
        out.write(json.dumps({"arm": "muse", "effort": effort, "trial": TRIAL or "t1", "item": iid,
                              "level": item["level"], "lens": item["lens"],
                              "duration_s": round(dur, 1), "rc": rc,
                              "parsed": result is not None, "result": result}) + "\n")
        print(f"[muse:{effort:8s}{SUFFIX}] {iid:22s} {dur:6.1f}s rc={rc} parsed={result is not None} "
              f"findings={len(result['findings']) if result else '-'}", flush=True)
    out.close()


def main():
    key = json.load(open(pathlib.Path(__file__).resolve().parent / "key.json"))
    (RUN / "results").mkdir(exist_ok=True)
    for effort in sys.argv[1:]:
        print(f"=== effort={effort} ===", flush=True)
        run_effort(effort, key)
    print("SWEEP COMPLETE", flush=True)


if __name__ == "__main__":
    main()
