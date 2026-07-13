#!/usr/bin/env python3
"""Warm session pool prototype (issue #3264 spike).

deposit  <transcript.jsonl> <store-dir>            -- derive a manifest from what the session
                                                      actually read, key it, file it in the store
checkout <store-dir> --model M --path P [--cutoff S] -- lease the newest eligible warm session
release  <store-dir> <session-id>                  -- drop the lease after the box exits

Pool key (owner invariants, #3264): (model, tools-fingerprint, declared-file-set, tree-hash),
eligibility age < cutoff AND tree unchanged AND not leased. Leases expire on their own.
"""
import json, hashlib, os, sys, time, argparse

def sha256_file(path):
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()

def tree_hash(files):
    lines = "".join(f"{p}:{h}\n" for p, h in sorted(files.items()))
    return hashlib.sha256(lines.encode()).hexdigest()

def deposit(transcript, store):
    init, last_usage, reads = None, None, set()
    for line in open(transcript):
        try:
            ev = json.loads(line)
        except json.JSONDecodeError:
            continue
        if ev.get("type") == "system" and ev.get("subtype") == "init":
            init = ev
        elif ev.get("type") == "assistant":
            msg = ev.get("message") or {}
            if "haiku" in msg.get("model", ""):
                continue
            if msg.get("usage"):
                last_usage = msg["usage"]
            for blk in msg.get("content") or []:
                if isinstance(blk, dict) and blk.get("type") == "tool_use" and blk.get("name") == "Read":
                    fp = (blk.get("input") or {}).get("file_path")
                    if fp:
                        reads.add(fp)
    if not init:
        sys.exit("no init event in transcript")

    cwd = init["cwd"]
    # The declared file set is what the box ACTUALLY read (authoritative, may be wider
    # than the issue's declared surface). Paths outside cwd can't be tree-checked; keep
    # them but mark the manifest unpoolable if any exists.
    files = {}
    external = []
    for fp in sorted(reads):
        rel = os.path.relpath(fp, cwd)
        (external.append(fp) if rel.startswith("..") else files.__setitem__(rel, sha256_file(fp) if os.path.exists(fp) else "MISSING"))

    manifest = {
        "session_id": init["session_id"],
        "model": init["model"],
        "cwd": cwd,
        "tools_fingerprint": hashlib.sha256(json.dumps(init["tools"]).encode()).hexdigest()[:16],
        "files": files,
        "external_reads": external,
        "tree_hash": tree_hash(files),
        "deposited_at": time.time(),
        "context_tokens": (last_usage or {}).get("cache_read_input_tokens", 0)
                          + (last_usage or {}).get("cache_creation_input_tokens", 0),
    }
    os.makedirs(store, exist_ok=True)
    out = os.path.join(store, f"{manifest['session_id']}.manifest.json")
    with open(out, "w") as f:
        json.dump(manifest, f, indent=2)
    print(f"deposited {manifest['session_id']} model={manifest['model']} files={len(files)} "
          f"context={manifest['context_tokens']} tree={manifest['tree_hash'][:12]}")

def lease_path(store, sid):
    return os.path.join(store, f"{sid}.lease")

def try_lease(store, sid, ttl):
    path = lease_path(store, sid)
    now = time.time()
    if os.path.exists(path):
        try:
            expiry = float(open(path).read().strip())
        except ValueError:
            expiry = 0
        if expiry > now:
            return False          # live lease — someone else holds it
        os.unlink(path)           # expired lease from a dead box — reclaim
    try:
        fd = os.open(path, os.O_CREAT | os.O_EXCL | os.O_WRONLY)  # atomic: loser of a race errors
    except FileExistsError:
        return False
    with os.fdopen(fd, "w") as f:
        f.write(str(now + ttl))
    return True

def checkout(store, model, path, cutoff, lease_ttl):
    now = time.time()
    candidates = []
    for name in os.listdir(store):
        if not name.endswith(".manifest.json"):
            continue
        m = json.load(open(os.path.join(store, name)))
        age = now - m["deposited_at"]
        if m["model"] != model:
            print(f"  skip {m['session_id'][:8]}: model {m['model']} != {model}", file=sys.stderr)
        elif age >= cutoff:
            os.unlink(os.path.join(store, name))   # invariant 1: past the age bound = delete, not keep
            print(f"  retired {m['session_id'][:8]}: age {age:.0f}s >= cutoff {cutoff:.0f}s", file=sys.stderr)
        elif not any(f == path or f.startswith(path.rstrip('/') + '/') for f in m["files"]):
            print(f"  skip {m['session_id'][:8]}: no file under {path}", file=sys.stderr)
        else:
            live = {p: (sha256_file(os.path.join(m["cwd"], p)) if os.path.exists(os.path.join(m["cwd"], p)) else "MISSING")
                    for p in m["files"]}
            if tree_hash(live) != m["tree_hash"]:
                os.unlink(os.path.join(store, name))   # invariant 3: subtree moved = the session's beliefs are stale
                print(f"  retired {m['session_id'][:8]}: tree changed under it", file=sys.stderr)
            else:
                candidates.append(m)
    for m in sorted(candidates, key=lambda m: -m["deposited_at"]):
        if try_lease(store, m["session_id"], lease_ttl):   # invariant 2: exclusive, expiring
            print(m["session_id"])
            return
        print(f"  skip {m['session_id'][:8]}: leased", file=sys.stderr)
    print("COLD")   # no eligible warm session — run cold, deposit on exit

def main():
    ap = argparse.ArgumentParser()
    sub = ap.add_subparsers(dest="cmd", required=True)
    d = sub.add_parser("deposit"); d.add_argument("transcript"); d.add_argument("store")
    c = sub.add_parser("checkout"); c.add_argument("store"); c.add_argument("--model", required=True)
    c.add_argument("--path", required=True); c.add_argument("--cutoff", type=float, default=3600)
    c.add_argument("--lease-ttl", type=float, default=1800)
    r = sub.add_parser("release"); r.add_argument("store"); r.add_argument("session_id")
    a = ap.parse_args()
    if a.cmd == "deposit":
        deposit(a.transcript, a.store)
    elif a.cmd == "checkout":
        checkout(a.store, a.model, a.path, a.cutoff, a.lease_ttl)
    else:
        p = lease_path(a.store, a.session_id)
        os.path.exists(p) and os.unlink(p)
        print(f"released {a.session_id}")

main()
