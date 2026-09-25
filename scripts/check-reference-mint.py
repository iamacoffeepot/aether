#!/usr/bin/env python3
"""Reject reference mints outside the gate's allowlist.

The gated mints turn a confirmed-`Live` position into a proven reference
(`__mint_actor_ref`, `__mint_erased_actor_ref`), or an engine backing into a
`Shared` `Blob` (`__mint_shared_blob`, and the guest backing's
`__mint_guest_blob`, ADR-0238 decision 4), so only the paths in
`ALLOWED_PATHS` may name them. Every other mention in tracked Rust source
is a finding: nothing written in scanned source — no comment, attribute,
marker, or flag — relaxes this scan, and widening the allowlist means editing
this gate in a reviewed diff.
"""

from __future__ import annotations

import argparse
import re
import subprocess
import sys
from pathlib import Path


ALLOWED_PATHS = (
    "crates/aether-actor/src/blob/guest.rs",
    "crates/aether-actor/src/reference/mint.rs",
    "crates/aether-actor/src/reference/mod.rs",
    "crates/aether-actor/src/lib.rs",
    "crates/aether-data/src/blob/mod.rs",
    "crates/aether-data/src/lib.rs",
    "crates/aether-substrate/src/mail/registry/mailbox/proven.rs",
    "crates/aether-substrate/src/store/entry.rs",
)

MINT_RE = re.compile(r"\b__mint_(actor_ref|erased_actor_ref|shared_blob|guest_blob)\b")


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


def scan(root: Path) -> list[str]:
    findings: list[str] = []
    for path in tracked_rust_files(root):
        if path in ALLOWED_PATHS:
            continue
        try:
            text = (root / path).read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError) as error:
            raise OperationalError(f"cannot read {path}: {error}") from error
        for number, line in enumerate(text.splitlines(), start=1):
            if MINT_RE.search(line):
                findings.append(f"{path}:{number}: {line.strip()}")
    return findings


def arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--root", default=".", help="directory to scan (default: the working directory)")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = arguments(argv if argv is not None else sys.argv[1:])
    try:
        findings = scan(Path(args.root))
    except OperationalError as error:
        print(f"reference-mint scan error: {error}", file=sys.stderr)
        return 2
    for finding in findings:
        print(finding)
    return 1 if findings else 0


if __name__ == "__main__":
    raise SystemExit(main())
