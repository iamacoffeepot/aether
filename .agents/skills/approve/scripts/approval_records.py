#!/usr/bin/env python3
"""Parse canonical hidden approval records from an Aether issue body."""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path
from typing import Any


MANAGED_HEADINGS = frozenset(
    {
        "## Problem statement",
        "## Design notes",
        "## Implementation plan",
        "## Sub-issues",
        "## Depends on",
        "## Declared surface",
        "## Dogfood brief",
        "## Side findings",
    }
)
MARKER_PREFIX = "<!-- aether-approval:v2"
RECORD_PATTERN = re.compile(r"^<!-- aether-approval:v2 (\{.*\}) -->$")
SHA_PATTERN = re.compile(r"^[0-9a-f]{40}$")
DIGEST_PATTERN = re.compile(r"^[0-9a-f]{64}$")
EXPECTED_KEYS = {
    "authority",
    "base_sha",
    "effective_tier",
    "issue",
    "model",
    "plan_sha256",
    "policy_tier",
    "size",
}


class ApprovalRecordError(ValueError):
    """Raised when a hidden approval lookalike violates the v2 contract."""


def _require_enum(record: dict[str, Any], key: str, values: set[str]) -> None:
    value = record[key]
    if not isinstance(value, str) or value not in values:
        raise ApprovalRecordError(f"{key} must be one of {sorted(values)}")


def _validate_record(record: Any, issue: int | None) -> dict[str, Any]:
    if not isinstance(record, dict) or set(record) != EXPECTED_KEYS:
        raise ApprovalRecordError("approval payload must contain exactly the eight contract keys")

    _require_enum(record, "authority", {"owner", "policy-auto"})
    _require_enum(record, "effective_tier", {"auto", "judge", "human"})
    _require_enum(record, "model", {"haiku", "sonnet", "opus"})
    _require_enum(record, "policy_tier", {"auto", "judge", "human"})
    _require_enum(record, "size", {"s", "m", "l"})

    if not isinstance(record["issue"], int) or isinstance(record["issue"], bool) or record["issue"] <= 0:
        raise ApprovalRecordError("issue must be a positive integer")
    if issue is not None and record["issue"] != issue:
        raise ApprovalRecordError(f"approval payload issue {record['issue']} does not match requested issue {issue}")
    if not isinstance(record["base_sha"], str) or SHA_PATTERN.fullmatch(record["base_sha"]) is None:
        raise ApprovalRecordError("base_sha must be 40 lowercase hexadecimal characters")
    if not isinstance(record["plan_sha256"], str) or DIGEST_PATTERN.fullmatch(record["plan_sha256"]) is None:
        raise ApprovalRecordError("plan_sha256 must be 64 lowercase hexadecimal characters")

    return record


def parse_records(body: str, issue: int | None = None) -> list[dict[str, Any]]:
    """Return validated v2 records from before the first managed H2, in body order."""

    prefix_lines: list[str] = []
    for line in body.splitlines():
        if line in MANAGED_HEADINGS:
            break
        prefix_lines.append(line)

    records: list[dict[str, Any]] = []
    for line_number, line in enumerate(prefix_lines, start=1):
        if MARKER_PREFIX not in line:
            continue

        match = RECORD_PATTERN.fullmatch(line)
        if match is None:
            raise ApprovalRecordError(f"line {line_number}: malformed aether-approval:v2 record")
        try:
            payload = json.loads(match.group(1))
        except json.JSONDecodeError as error:
            raise ApprovalRecordError(f"line {line_number}: invalid approval JSON: {error.msg}") from error

        record = _validate_record(payload, issue)
        canonical_json = json.dumps(record, sort_keys=True, separators=(",", ":"))
        if match.group(1) != canonical_json:
            raise ApprovalRecordError(f"line {line_number}: approval JSON is not canonical compact sorted JSON")
        records.append(record)

    return records


def _parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--body-file", required=True, type=Path)
    parser.add_argument("--issue", type=int)
    return parser.parse_args()


def main() -> int:
    args = _parse_args()
    try:
        body = args.body_file.read_bytes().decode("utf-8")
        records = parse_records(body, args.issue)
    except (OSError, UnicodeDecodeError, ApprovalRecordError) as error:
        print(str(error), file=sys.stderr)
        return 2

    print(json.dumps({"records": records}, sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
