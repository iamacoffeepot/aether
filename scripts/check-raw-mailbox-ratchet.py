#!/usr/bin/env python3
"""Ratchet the count of raw-mailbox door call sites — the count may only fall.

ADR-0230's contract phase retires a fixed set of "old-door" functions (direct
mailbox addressing, unproven references, and the `#[allow(clippy::
disallowed_methods)]` suppressions that stand in for them) one call site at a
time. This scanner enforces the one rule that makes that retirement durable:
during expand and migrate, a change that raises an old-door count, or that
raises the checked-in baseline to match new call sites, does not land.

Base/head split. CI materializes this file from the pull request's *base*
commit (`git show "${BASE_SHA}:scripts/check-raw-mailbox-ratchet.py"`), the
same way `.github/workflows/ci.yml` already does for the suppression and
reference-mint scans, so the pattern table and the comparison a candidate is
judged by are never candidate-editable. `scripts/raw-mailbox-baseline.json`
is read from the *head* instead, because lowering it in the same change is
the required half of the ratchet — see `compare()` and its docstring for the
three rules this composes into.

One-way rule. `--base <sha>` additionally requires the head's baseline not to
exceed the baseline recorded at that base commit for any tracked pattern, so
a candidate cannot satisfy the exact-match rule by raising the baseline
alongside new call sites. Without `--base`, only the exact-match rule runs.

Regex limits. The grep in `PATTERNS` is a pre-flight, not a proof. It counts
textual occurrences after simple end-of-line comment stripping and cannot see
every spelling of a call: a function-pointer or closure capture of a method
path (`let f = <T as Trait>::m;`) is invisible to any regex here. For a
still-present door that is the one spelling this ratchet cannot see (grep the
tree by hand at a contract deletion). For a *deleted* method it is harmless:
`cargo check` is the proof, since the compiler rejects the capture once the
function no longer exists.
"""

from __future__ import annotations

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


BASELINE_RELATIVE_PATH = "scripts/raw-mailbox-baseline.json"

PATTERNS: tuple[tuple[str, re.Pattern[str]], ...] = (
    ("send_to(", re.compile(r"(::|\.)send_to\(")),
    ("send_to_named(", re.compile(r"(::|\.)send_to_named\(")),
    ("actor_at::<", re.compile(r"(::|\.)actor_at::<")),
    ("send_envelope_tracked(", re.compile(r"(::|\.)send_envelope_tracked\(")),
    ("send_envelope_tracked_with_reply_to(", re.compile(r"(::|\.)send_envelope_tracked_with_reply_to\(")),
    ("send_envelope_detached(", re.compile(r"(::|\.)send_envelope_detached\(")),
    ("monitor(", re.compile(r"(::|\.)monitor\(")),
    ("despawn_inline_child(", re.compile(r"(::|\.)despawn_inline_child\(")),
    ("resolve_actor::<", re.compile(r"(::|\.)resolve_actor::<")),
    ("resolve_embedded::<", re.compile(r"(::|\.)resolve_embedded::<")),
    ("mailer()", re.compile(r"(::|\.)mailer\(\)")),
    ("registry()", re.compile(r"\.registry\(\)")),
    ("source_mailbox()", re.compile(r"(::|\.)source_mailbox\(\)")),
    ("mailbox_id_from_name(", re.compile(r"\bmailbox_id_from_name\(")),
    ("mailbox_id_from_name_pair(", re.compile(r"\bmailbox_id_from_name_pair\(")),
    ("mailbox_id_from_path(", re.compile(r"\bmailbox_id_from_path\(")),
    ("MailboxId::NONE", re.compile(r"\bMailboxId::NONE\b")),
    (
        "<recipient>.id() at a send",
        re.compile(r"\b(send_to|send_detached_to|send_envelope_tracked|monitor|despawn_inline_child)\(&?[A-Za-z_][A-Za-z0-9_]*\.id\(\)"),
    ),
    ("clippy::disallowed_methods", re.compile(r"clippy::disallowed_methods")),
)
PATTERN_NAMES: tuple[str, ...] = tuple(name for name, _ in PATTERNS)
_PATTERN_NAME_SET = frozenset(PATTERN_NAMES)


class OperationalError(RuntimeError):
    """The scanner could not compute an authoritative verdict."""


def tracked_rust_files(root: Path) -> list[str]:
    completed = subprocess.run(
        ["git", "ls-files", "-z", "--", "*.rs"],
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or "git ls-files failed"
        raise OperationalError(detail)
    return [path for path in completed.stdout.split("\0") if path]


def count(root: Path) -> dict[str, int]:
    counts: dict[str, int] = {name: 0 for name in PATTERN_NAMES}
    for path in tracked_rust_files(root):
        try:
            text = (root / path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise OperationalError(f"cannot read {path}: {error}") from error
        for line in text.splitlines():
            code = line.split("//", 1)[0]
            for name, pattern in PATTERNS:
                counts[name] += len(pattern.findall(code))
    return counts


def list_sites(root: Path, name: str) -> list[str]:
    matches = [pattern for pattern_name, pattern in PATTERNS if pattern_name == name]
    if not matches:
        raise OperationalError(f"unknown pattern for --list: {name!r}")
    pattern = matches[0]
    sites: list[str] = []
    for path in tracked_rust_files(root):
        try:
            text = (root / path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise OperationalError(f"cannot read {path}: {error}") from error
        for number, line in enumerate(text.splitlines(), start=1):
            code = line.split("//", 1)[0]
            if pattern.search(code):
                sites.append(f"{path}:{number}: {line.strip()}")
    return sites


def _parse_baseline(text: str, *, source: str) -> dict[str, int]:
    try:
        data = json.loads(text)
    except json.JSONDecodeError as error:
        raise OperationalError(f"{source}: malformed JSON: {error}") from error
    patterns = data.get("patterns") if isinstance(data, dict) else None
    if not isinstance(patterns, dict):
        raise OperationalError(f'{source}: "patterns" must be a JSON object')
    result: dict[str, int] = {}
    for key, value in patterns.items():
        if not isinstance(value, int) or isinstance(value, bool):
            raise OperationalError(f"{source}: pattern {key!r} has a non-integer count")
        result[key] = value
    return result


def load_baseline(path: Path) -> dict[str, int]:
    if not path.exists():
        return {}
    try:
        text = path.read_text(encoding="utf-8")
    except OSError as error:
        raise OperationalError(f"cannot read {path}: {error}") from error
    return _parse_baseline(text, source=str(path))


def base_baseline(root: Path, sha: str) -> dict[str, int] | None:
    probe = subprocess.run(
        ["git", "cat-file", "-e", f"{sha}:{BASELINE_RELATIVE_PATH}"],
        cwd=root,
        check=False,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )
    if probe.returncode != 0:
        return None
    completed = subprocess.run(
        ["git", "show", f"{sha}:{BASELINE_RELATIVE_PATH}"],
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode != 0:
        detail = completed.stderr.strip() or "git show failed"
        raise OperationalError(detail)
    return _parse_baseline(completed.stdout, source=f"{sha}:{BASELINE_RELATIVE_PATH}")


def compare(
    head_counts: dict[str, int],
    head_baseline: dict[str, int],
    base_baseline: dict[str, int] | None,
    base_sha: str | None,
) -> list[str]:
    """Apply the three ratchet rules, in order, and return the findings.

    1. Exact match: `head_count != head_baseline` is a finding for every
       tracked pattern, whichever direction the mismatch runs — a rise needs
       the call sites reverted, a stale-high baseline (the count fell but the
       baseline was left behind) needs the baseline lowered in this change.
    2. One-way baseline: with a `base_sha`, a head baseline above the base's
       baseline for the same pattern is a finding even when rule 1 is
       satisfied — this is what stops a candidate raising both together.
    3. Absent row is zero: a pattern with no baseline row (head or base) is
       read as baseline 0, so a door's retirement is one pull request and a
       baseline row deleted to dodge the ratchet fails loudly against a
       nonzero head count.

    A baseline row this scanner does not name is not a rule-3 zero; it is
    printed as a notice and never fails, since it names a pattern a later
    scanner revision will track.
    """

    findings: list[str] = []

    for name in PATTERN_NAMES:
        head = head_counts[name]
        baseline = head_baseline.get(name, 0)
        if head == baseline:
            continue
        delta = head - baseline
        if delta > 0:
            findings.append(
                f"raw-mailbox ratchet: `{name}` — baseline {baseline}, head {head} (+{delta})\n"
                "    ADR-0230: an old-door count may not rise during expand and migrate. Reach the target\n"
                "    through a proven reference instead of this door. Raising the baseline is also a failure."
            )
        else:
            findings.append(
                f"raw-mailbox ratchet: `{name}` — baseline {baseline}, head {head} ({delta})\n"
                f'    Set "{name}" to {head} in scripts/raw-mailbox-baseline.json, in this same change.'
            )

    if base_sha is None:
        pass
    elif base_baseline is None:
        print(f"::notice::raw-mailbox ratchet: no baseline at base {base_sha}; the one-way check is skipped (bootstrap).")
    else:
        for name in PATTERN_NAMES:
            head_value = head_baseline.get(name, 0)
            base_value = base_baseline.get(name, 0)
            if head_value > base_value:
                findings.append(
                    f"raw-mailbox ratchet: `{name}` — baseline raised {base_value} -> {head_value} against {base_sha}\n"
                    "    The ratchet is one-way. Restore the baseline and drop the new call sites."
                )

    for key in head_baseline:
        if key not in _PATTERN_NAME_SET:
            print(f'::notice::raw-mailbox ratchet: baseline row "{key}" is not tracked by this scanner; ignored.')

    return findings


def arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="directory to scan (default: the working directory)")
    parser.add_argument(
        "--baseline",
        default=None,
        help="path to the baseline JSON (default: <root>/scripts/raw-mailbox-baseline.json)",
    )
    parser.add_argument(
        "--base",
        default=None,
        help="git-ish to read the base baseline from for the one-way rule (omit to skip that rule)",
    )
    parser.add_argument(
        "--list",
        dest="list_pattern",
        default=None,
        metavar="PATTERN",
        help="print path:line: text call sites for one tracked pattern instead of comparing",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = arguments(argv if argv is not None else sys.argv[1:])
    root = Path(args.root)
    baseline_path = Path(args.baseline) if args.baseline is not None else root / BASELINE_RELATIVE_PATH

    try:
        if args.list_pattern is not None:
            for site in list_sites(root, args.list_pattern):
                print(site)
            return 0

        head_counts = count(root)
        head_baseline = load_baseline(baseline_path)
        base_line = base_baseline(root, args.base) if args.base is not None else None
        findings = compare(head_counts, head_baseline, base_line, args.base)
    except OperationalError as error:
        print(f"raw-mailbox ratchet scan error: {error}", file=sys.stderr)
        return 2

    for finding in findings:
        print(finding)
    return 1 if findings else 0


if __name__ == "__main__":
    raise SystemExit(main())
