#!/usr/bin/env python3
"""The declared-surface matcher — the one place gitwildmatch semantics are decided.

An issue's `## Declared surface` is a block of globs; `.github/approval-policy.yml`
maps globs to approval tiers. Two consumers need to evaluate those globs with
identical semantics, and a second copy of the matcher would drift from the first:

  - reconciler.yml — does a merged PR's changed-file set stay inside the surface
    its closing issue declared? (containment; the policy only annotates each
    escaping path with the tier it would have cost)
  - agent-tick.yml — what approval tier does a `phase:plan` issue's declared
    surface resolve to? (only an `auto` tier is dispatched for auto-approval)

Globs are gitwildmatch (gitignore semantics): `**` spans path segments, `*` stays
within one, `?` is one non-slash character, and a pattern naming a directory
covers everything beneath it. Bash has no such matcher and the runner ships no
pathspec module, so the patterns are translated to regexes here.

Two modes:

    surface-match.py <globs_file> <changed_file> [<policy_file>]
        Containment. Prints `path\ttier` for every path in <changed_file> that
        NO glob in <globs_file> matches; silent when the diff is contained. The
        tier column is annotation only — it reads `?` without a <policy_file>,
        or when the policy is empty (not on the default branch yet).

    surface-match.py --tier <paths_file> <policy_file>
        Tier resolution. Prints the single most restrictive tier (human > judge >
        auto) over every path in <paths_file>, falling back to the policy's
        `default` for a path no rule matches. An empty <paths_file> prints the
        default: a surface that declares nothing can never resolve to `auto`.
"""

import re
import sys


def compile_glob(pat):
    pat = pat.rstrip("/")
    anchored = "/" in pat
    out, i, n = [], 0, len(pat)
    while i < n:
        c = pat[i]
        if c == "*":
            j = i
            while j < n and pat[j] == "*":
                j += 1
            doubled = j - i > 1
            at_start = i == 0 or pat[i - 1] == "/"
            if doubled and at_start and j < n and pat[j] == "/":
                out.append("(?:[^/]+/)*")  # "**/" — zero or more segments
                i = j + 1
            elif doubled and at_start and j >= n:
                out.append(".*")  # trailing "/**" — everything beneath
                i = j
            else:
                out.append("[^/]*")
                i = j
        elif c == "?":
            out.append("[^/]")
            i += 1
        elif c == "[":
            j = i + 1
            if j < n and pat[j] in "!^":
                j += 1
            if j < n and pat[j] == "]":
                j += 1
            while j < n and pat[j] != "]":
                j += 1
            if j >= n:
                out.append(re.escape("["))
                i += 1
            else:
                cls = pat[i + 1 : j]
                out.append("[" + ("^" + cls[1:] if cls.startswith("!") else cls) + "]")
                i = j + 1
        else:
            out.append(re.escape(c))
            i += 1
    body = "".join(out)
    # An unanchored pattern (no slash) matches at any depth. The trailing
    # group is the directory rule: naming a directory covers its contents.
    return re.compile(("^" if anchored else "^(?:.*/)?") + body + "(?:/.*)?$")


RANK = {"auto": 0, "judge": 1, "human": 2}


def read_globs(path):
    return [
        line.strip()
        for line in open(path, encoding="utf-8").read().splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]


def load_policy(path):
    # The file's shape is owned by this repo (a `default` tier plus a list of
    # {glob, tier} rules), so it is parsed with the standard library rather than
    # PyYAML, which the runner is not guaranteed to ship: a hand parser keeps the
    # tier deterministic instead of silently degrading to "?". Tier resolution is
    # most-restrictive-wins over the matching rules, falling back to the file's
    # own `default` — the same resolution /approve applies at the Plan->Ready
    # edge, so both consumers quote the tier a path would actually cost.
    text = open(path, encoding="utf-8").read()
    if not text.strip():
        # The fetch failed (no policy on the default branch yet), so the tier is
        # honestly unresolved rather than assumed.
        return None
    default, rules, pending = "human", [], None
    for raw in text.splitlines():
        line = raw.split("#", 1)[0].rstrip()
        m = re.match(r"^default:\s*(\w+)\s*$", line)
        if m:
            default = m.group(1)
            continue
        m = re.match(r"""^\s*-\s*glob:\s*["']?(.+?)["']?\s*$""", line)
        if m:
            pending = m.group(1)
            continue
        m = re.match(r"""^\s*tier:\s*["']?(\w+)["']?\s*$""", line)
        if m and pending is not None:
            if m.group(1) in RANK:
                rules.append((compile_glob(pending), m.group(1)))
            pending = None
    return default, rules


def tier_of(path, policy):
    if policy is None:
        return "?"
    default, rules = policy
    matched = [tier for matcher, tier in rules if matcher.match(path)]
    return max(matched, key=lambda t: RANK[t]) if matched else default


def read_paths(path):
    return [line.strip() for line in open(path, encoding="utf-8").read().splitlines() if line.strip()]


def main(argv):
    if len(argv) > 1 and argv[1] == "--tier":
        policy = load_policy(argv[3])
        if policy is None:
            print("?")
            return
        # Most restrictive over every declared path, and the policy default for a
        # surface that declares nothing — the fail-safe direction, since `auto` is
        # the only tier that dispatches an unattended approval.
        tiers = [tier_of(p, policy) for p in read_globs(argv[2])] or [policy[0]]
        print(max(tiers, key=lambda t: RANK.get(t, len(RANK))))
        return

    matchers = [compile_glob(g) for g in read_globs(argv[1])]
    policy = load_policy(argv[3]) if len(argv) > 3 else None
    for path in read_paths(argv[2]):
        if not any(m.match(path) for m in matchers):
            print(f"{path}\t{tier_of(path, policy)}")


if __name__ == "__main__":
    main(sys.argv)
