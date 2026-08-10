#!/usr/bin/env python3
"""Parse Aether managed Plan artifacts and emit their canonical SHA-256 digest."""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
from dataclasses import dataclass
from pathlib import Path
from typing import Sequence


MANAGED_ORDER = (
    "Problem statement",
    "Design notes",
    "Implementation plan",
    "Sub-issues",
    "Depends on",
    "Declared surface",
    "Dogfood brief",
    "Side findings",
)
DIGEST_ORDER = MANAGED_ORDER[:-1]
REQUIRED = {
    "Problem statement",
    "Design notes",
    "Implementation plan",
    "Declared surface",
    "Dogfood brief",
}
H2 = re.compile(r"(?m)^## ([^\r\n]+)(?:\r?\n|$)")
SIZE = re.compile(r"^\*\*Size:\*\* (s|m|l)$")
MODEL = re.compile(r"^\*\*Implementation model:\*\* (haiku|sonnet|opus)$")
REASON = re.compile(r"^\*\*Routing reason:\*\* (\S.*)$")


class PlanDigestError(ValueError):
    """The issue body is not a valid gate-bearing Plan artifact."""


@dataclass(frozen=True)
class PlanDigest:
    plan_sha256: str
    size: str
    model: str
    sections: tuple[str, ...]

    def as_dict(self) -> dict[str, object]:
        return {
            "model": self.model,
            "plan_sha256": self.plan_sha256,
            "sections": list(self.sections),
            "size": self.size,
            "version": "aether-plan:v1",
        }


def _section_spans(body: str) -> dict[str, str]:
    matches = list(H2.finditer(body))
    spans: dict[str, str] = {}
    positions: dict[str, int] = {}

    for index, match in enumerate(matches):
        name = match.group(1)
        if name not in MANAGED_ORDER:
            continue
        if name in spans:
            raise PlanDigestError(f"duplicate managed heading: ## {name}")
        end = matches[index + 1].start() if index + 1 < len(matches) else len(body)
        spans[name] = body[match.start() : end]
        positions[name] = match.start()

    missing = sorted(REQUIRED.difference(spans))
    if missing:
        raise PlanDigestError("missing required managed sections: " + ", ".join(missing))

    empty = []
    for name in REQUIRED:
        heading = H2.match(spans[name])
        assert heading is not None
        if not spans[name][heading.end() :].strip():
            empty.append(name)
    if empty:
        raise PlanDigestError("empty required managed sections: " + ", ".join(sorted(empty)))

    present_order = [name for name in MANAGED_ORDER if name in positions]
    actual_order = [name for name, _ in sorted(positions.items(), key=lambda item: item[1])]
    if actual_order != present_order:
        raise PlanDigestError("managed sections are not in scope-owned order")

    return spans


def _routing(plan_span: str) -> tuple[str, str]:
    heading = H2.match(plan_span)
    assert heading is not None
    lines = [line for line in plan_span[heading.end() :].splitlines() if line.strip()]

    size_lines = [line for line in lines if line.lstrip().startswith("**Size:**")]
    model_lines = [line for line in lines if line.lstrip().startswith("**Implementation model:**")]
    reason_lines = [line for line in lines if line.lstrip().startswith("**Routing reason:**")]
    if len(size_lines) != 1:
        raise PlanDigestError(f"expected exactly one Size routing line, found {len(size_lines)}")
    if len(model_lines) != 1:
        raise PlanDigestError(
            f"expected exactly one Implementation model routing line, found {len(model_lines)}"
        )
    if len(reason_lines) != 1:
        raise PlanDigestError(f"expected exactly one Routing reason line, found {len(reason_lines)}")
    if len(lines) < 3 or lines[-3:] != [size_lines[0], model_lines[0], reason_lines[0]]:
        raise PlanDigestError("routing lines must be the final three non-empty Implementation plan lines")

    size_match = SIZE.fullmatch(size_lines[0])
    model_match = MODEL.fullmatch(model_lines[0])
    reason_match = REASON.fullmatch(reason_lines[0])
    if size_match is None:
        raise PlanDigestError("invalid Size routing line")
    if model_match is None:
        raise PlanDigestError("invalid Implementation model routing line")
    if reason_match is None:
        raise PlanDigestError("invalid or empty Routing reason line")
    return size_match.group(1), model_match.group(1)


def digest_body(body: str) -> PlanDigest:
    spans = _section_spans(body)
    size, model = _routing(spans["Implementation plan"])
    included = tuple(name for name in DIGEST_ORDER if name in spans)

    canonical = bytearray(b"aether-plan:v1\0")
    for name in included:
        # Blank line separators belong to the issue layout, not to either
        # neighboring section. Canonicalize only that boundary so adding or
        # removing an excluded H2 cannot change the preceding managed span.
        payload = (spans[name].rstrip("\r\n") + "\n").encode("utf-8")
        canonical.extend(name.encode("utf-8"))
        canonical.extend(b"\0")
        canonical.extend(str(len(payload)).encode("ascii"))
        canonical.extend(b"\0")
        canonical.extend(payload)
        canonical.extend(b"\0")

    return PlanDigest(hashlib.sha256(canonical).hexdigest(), size, model, included)


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--body-file", required=True, help="UTF-8 issue body to parse")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        body = Path(arguments.body_file).read_text(encoding="utf-8")
        result = digest_body(body)
    except (OSError, UnicodeError, PlanDigestError) as error:
        print(f"plan-digest: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result.as_dict(), sort_keys=True, separators=(",", ":")))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
