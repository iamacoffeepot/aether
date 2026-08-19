#!/usr/bin/env python3
"""The declared-surface matcher — the one place gitwildmatch semantics are decided.

An issue's `## Declared surface` is a block of globs; `approval-policy.toml`
maps globs to approval tiers. The direct-drive Codex approve skill loads this
matcher from the captured `origin/main` commit so Plan-to-Ready validation and
tier resolution use owner-authored semantics. The CLI also retains containment
mode for deterministic local surface audits.

Keeping both modes here gives them identical glob semantics; a second matcher
would drift from the first.

Policy globs are gitwildmatch (gitignore semantics): `**` spans path segments,
`*` stays within one, `?` is one non-slash character, and a pattern naming a
directory covers everything beneath it. Declared surfaces use the same grammar
but are repository-relative, so even a slashless concrete path is root-anchored.
Bash has no such matcher and the runner ships no pathspec module, so the
patterns are translated to regexes here.

Declared surfaces come from issue bodies, so both modes hold every declared
pattern to the validated grammar (a concrete path, or a literal directory
prefix followed by one final `/**`). A pattern outside the grammar never
reaches the regex engine: containment ignores it with a stderr note (so the
paths it claimed to cover escape), and tier mode resolves it `human`. This
also keeps an adversarial pattern such as a run of `**/` segments from
backtracking the regex engine for hours.

Two modes:

    surface-match.py <globs_file> <changed_file> [<policy_file>]
        Containment. Prints `path\ttier` for every path in <changed_file> that
        NO glob in <globs_file> matches; silent when the diff is contained. The
        tier column is annotation only — it reads `?` without a <policy_file>,
        or when the policy is empty (not on the default branch yet). A root
        Cargo.lock change is treated as contained when the changed set itself
        contains an in-surface Cargo.toml manifest — the generated lockfile
        delta an in-surface manifest change necessarily produces.

    surface-match.py --tier <surface_file> <policy_file>
        Set-sound tier resolution over declared surface patterns. Exact paths and
        literal subtrees ending in /** include every policy rule that can match
        the path or anything beneath it, plus the policy default unless one rule
        covers the entire set. This mirrors containment's directory-tail
        semantics even for a planned path that does not exist yet. More complex
        wildcard declarations fail closed to human. Prints the single most
        restrictive result (human > judge > auto). An empty file prints the
        policy default.
"""

import re
import sys
import tomllib


def compile_glob(pat):
    pat = pat.rstrip("/")
    root_anchored = pat.startswith("/")
    if root_anchored:
        pat = pat[1:]
    anchored = root_anchored or "/" in pat
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


def compile_surface_glob(pattern):
    """Compile a repository-relative declared-surface pattern."""

    return compile_glob(pattern if pattern.startswith("/") else "/" + pattern)


SURFACE_SAFE = re.compile(r"[A-Za-z0-9._/*\-]+\Z")


def valid_surface_glob(pattern):
    """Whether a declared-surface pattern is inside the validated grammar.

    The same shape /approve's resolver accepts: a concrete repository-relative
    path, or a literal directory prefix followed by one final `/**`. Declared
    surfaces are untrusted issue-body text, so anything outside this grammar is
    refused before it can reach the regex engine.
    """

    if SURFACE_SAFE.fullmatch(pattern) is None:
        return False
    if pattern.startswith(("/", "-", "!", "#")) or pattern.endswith("/"):
        return False
    if any(segment in {"", ".", ".."} for segment in pattern.split("/")):
        return False
    if "*" not in pattern:
        return True
    return pattern.endswith("/**") and pattern.count("*") == 2


RANK = {"auto": 0, "judge": 1, "human": 2}


def read_globs(path):
    return [
        line.strip()
        for line in open(path, encoding="utf-8").read().splitlines()
        if line.strip() and not line.strip().startswith("#")
    ]


def load_policy_text(text):
    # The file's shape is owned by this repo (a `default` tier plus a list of
    # {glob, tier} rules). Parse it as typed TOML through the standard library
    # so the Python matcher and the chassis host (`toml::from_str` into
    # `ApprovalPolicy`, deny_unknown_fields) refuse the same surprises: unknown
    # keys, unknown tier spellings, and structural surprises are None rather
    # than a guessed tier. Tier resolution is most-restrictive-wins over the
    # matching rules, falling back to the file's own `default` — the same
    # resolution /approve applies at the Plan->Ready edge, so both consumers
    # quote the tier a path would actually cost.
    if not text.strip():
        # The fetch failed (no policy on the default branch yet), so the tier is
        # honestly unresolved rather than assumed.
        return None
    try:
        parsed = tomllib.loads(text)
    except tomllib.TOMLDecodeError:
        return None

    if not isinstance(parsed, dict) or parsed.keys() - {"default", "rules"}:
        return None
    default = parsed.get("default")
    if not isinstance(default, str) or default not in RANK:
        return None
    raw_rules = parsed.get("rules")
    if not isinstance(raw_rules, list) or not raw_rules:
        return None

    rules = []
    for item in raw_rules:
        if not isinstance(item, dict) or item.keys() != {"glob", "tier"}:
            return None
        glob, tier = item["glob"], item["tier"]
        if not isinstance(glob, str) or not isinstance(tier, str) or tier not in RANK or not valid_policy_glob(glob):
            return None
        try:
            matcher = compile_glob(glob)
        except re.error:
            return None
        rules.append((glob, matcher, tier))
    return default, rules


def valid_policy_glob(pattern):
    """Whether a policy glob is inside the canonical, provable grammar."""

    if not pattern or not pattern.isascii() or pattern.startswith(("!", "#", "-")):
        return False
    if "\\" in pattern or any(ord(character) < 32 for character in pattern):
        return False

    body = pattern[1:] if pattern.startswith("/") else pattern
    if not body or body.endswith("/") or "//" in body:
        return False
    segments = body.split("/")
    if any(segment in {"", ".", ".."} for segment in segments):
        return False
    # `**` has recursive meaning only as a complete path segment. Rejecting
    # ambiguous star runs keeps the subtree intersection proof aligned with the
    # regex compiler instead of accepting a pattern the two could normalize
    # differently.
    return not any("**" in segment and segment != "**" for segment in segments)


def load_policy(path):
    return load_policy_text(open(path, encoding="utf-8").read())


def tier_of(path, policy):
    if policy is None:
        return "?"
    default, rules = policy
    matched = [tier for _, matcher, tier in rules if matcher.match(path)]
    return max(matched, key=lambda t: RANK[t]) if matched else default


META = "*?["


def _segment_matches(pattern, literal):
    # Prefixing with / selects compile_glob's repository-anchored form. A segment
    # contains no slash, so the directory-tail suffix cannot create a false match.
    return compile_glob("/" + pattern).match(literal) is not None


def _normalise_pattern(pattern):
    pattern = pattern.rstrip("/")
    root_anchored = pattern.startswith("/")
    if root_anchored:
        pattern = pattern[1:]
    return pattern, root_anchored


def rule_intersects_subtree(rule_glob, surface_prefix):
    """Whether a policy rule can match any path at/below a literal prefix.

    This is exact for the matcher grammar at path-segment level. Once the fixed
    surface prefix is consumed, arbitrary descendant segments may be chosen.
    Once the policy pattern is consumed, compile_glob's directory-tail rule
    covers any remaining fixed surface segments.
    """

    pattern, root_anchored = _normalise_pattern(rule_glob)
    anchored = root_anchored or "/" in pattern
    if not anchored:
        # A slashless gitignore pattern can match a segment at any depth.
        return True

    policy_segments = pattern.split("/") if pattern else []
    surface_segments = surface_prefix.split("/") if surface_prefix else []
    seen = set()

    def intersects(policy_index, surface_index):
        state = (policy_index, surface_index)
        if state in seen:
            return False
        seen.add(state)
        if policy_index == len(policy_segments):
            return True
        if surface_index == len(surface_segments):
            # Every validated remaining glob segment has at least one witness,
            # which can be appended below the fixed surface prefix.
            return True

        segment = policy_segments[policy_index]
        if segment == "**":
            return intersects(policy_index + 1, surface_index) or intersects(policy_index, surface_index + 1)
        return _segment_matches(segment, surface_segments[surface_index]) and intersects(
            policy_index + 1, surface_index + 1
        )

    return intersects(0, 0)


def rule_covers_subtree(rule_glob, surface_prefix):
    """Conservatively prove that one policy rule covers the whole subtree."""

    pattern, root_anchored = _normalise_pattern(rule_glob)
    if not (root_anchored or "/" in pattern):
        return False

    if pattern.endswith("/**"):
        rule_prefix = pattern[:-3].rstrip("/")
    elif not any(character in pattern for character in META):
        # A literal directory pattern covers descendants under compile_glob.
        rule_prefix = pattern
    else:
        return False

    if any(character in rule_prefix for character in META):
        return False
    return surface_prefix == rule_prefix or surface_prefix.startswith(rule_prefix + "/")


def tier_of_surface(surface, policy):
    """Resolve the maximum tier of every concrete path a declaration permits."""

    if policy is None:
        return "?"

    surface = surface.rstrip("/")
    if not any(character in surface for character in META):
        prefix = surface.lstrip("/")
    else:
        if not surface.endswith("/**"):
            return "human"
        prefix = surface[:-3].rstrip("/").lstrip("/")
        if not prefix or any(character in prefix for character in META):
            return "human"

    default, rules = policy
    tiers = [
        tier
        for glob, _, tier in rules
        if rule_intersects_subtree(glob, prefix)
    ]
    if not any(rule_covers_subtree(glob, prefix) for glob, _, _ in rules):
        tiers.append(default)
    return max(tiers, key=lambda tier: RANK.get(tier, len(RANK))) if tiers else default


def read_paths(path):
    return [line.strip() for line in open(path, encoding="utf-8").read().splitlines() if line.strip()]


def main(argv):
    if len(argv) > 1 and argv[1] == "--tier":
        policy = load_policy(argv[3])
        if policy is None:
            print("?")
            return
        # Most restrictive over every path each declaration permits. The policy
        # default covers an empty surface, and any declaration outside the
        # validated grammar resolves human — only a proved `auto` result may
        # dispatch unattended approval.
        tiers = [
            tier_of_surface(p, policy) if valid_surface_glob(p) else "human" for p in read_globs(argv[2])
        ] or [policy[0]]
        print(max(tiers, key=lambda t: RANK.get(t, len(RANK))))
        return

    globs = read_globs(argv[1])
    matchers = []
    for glob in globs:
        if valid_surface_glob(glob):
            matchers.append(compile_surface_glob(glob))
        else:
            # Fail toward escape: the paths this pattern claimed to cover are
            # reported as outside the surface rather than silently contained.
            print(f"ignored out-of-grammar surface glob: {glob}", file=sys.stderr)
    changed_paths = read_paths(argv[2])
    # Issue #3492: a root Cargo.lock delta is the generated, unavoidable
    # consequence of an in-surface Cargo.toml change, so it stays in-surface
    # whenever the changed set itself contains a manifest that a declared
    # glob matches — diff-aware, so a `/**` subtree covering the changed
    # manifest qualifies same as an explicit `Cargo.toml` glob (#3483).
    manifest_changed_in_surface = any(
        (p == "Cargo.toml" or p.endswith("/Cargo.toml")) and any(m.match(p) for m in matchers)
        for p in changed_paths
    )
    policy = load_policy(argv[3]) if len(argv) > 3 else None
    for path in changed_paths:
        if not any(m.match(path) for m in matchers):
            if path == "Cargo.lock" and manifest_changed_in_surface:
                continue
            print(f"{path}\t{tier_of(path, policy)}")


if __name__ == "__main__":
    main(sys.argv)
