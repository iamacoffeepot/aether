#!/usr/bin/env python3
"""Tests for the Aether managed Plan digest."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import plan_digest


BASE = """\
## Description

Original user prose.

## Problem statement

The workflow has stale control state.

## Design notes

Use durable artifacts.

## Implementation plan

1. Replace the control state and test it.

**Size:** m
**Implementation model:** sonnet
**Routing reason:** The plan is mechanical and bounded.

## Declared surface

```
.agents/skills/**
```

## Dogfood brief

N/A — workflow-only change.

## Side findings

- unrelated observation
"""


class PlanDigestTests(unittest.TestCase):
    def test_deterministic_output_and_routing(self) -> None:
        first = plan_digest.digest_body(BASE)
        second = plan_digest.digest_body(BASE)
        self.assertEqual(first, second)
        self.assertEqual(first.size, "m")
        self.assertEqual(first.model, "sonnet")
        self.assertRegex(first.plan_sha256, r"^[0-9a-f]{64}$")

    def test_managed_edit_invalidates_approval(self) -> None:
        original = plan_digest.digest_body(BASE).plan_sha256
        for replacement in (
            BASE.replace("stale control state", "missing approval evidence"),
            BASE.replace("Use durable artifacts", "Use signed durable artifacts"),
            BASE.replace("Replace the control state", "Rewrite the control state"),
            BASE.replace(".agents/skills/**", ".agents/skills/approve/**"),
            BASE.replace("workflow-only change", "tooling-only change"),
        ):
            with self.subTest(replacement=replacement):
                self.assertNotEqual(plan_digest.digest_body(replacement).plan_sha256, original)

    def test_user_prose_and_side_findings_are_excluded(self) -> None:
        original = plan_digest.digest_body(BASE).plan_sha256
        changed_prose = BASE.replace("Original user prose.", "Original user prose, expanded.")
        changed_side = BASE.replace("unrelated observation", "different unrelated observation")
        without_side = BASE[: BASE.index("## Side findings")].rstrip() + "\n"
        self.assertEqual(plan_digest.digest_body(changed_prose).plan_sha256, original)
        self.assertEqual(plan_digest.digest_body(changed_side).plan_sha256, original)
        self.assertEqual(plan_digest.digest_body(without_side).plan_sha256, original)

    def test_optional_sections_are_canonical_and_relevant(self) -> None:
        insertion = """\
## Sub-issues

- #12

## Depends on

- #11 — prerequisite

"""
        with_optional = BASE.replace("## Declared surface\n", insertion + "## Declared surface\n")
        result = plan_digest.digest_body(with_optional)
        self.assertIn("Sub-issues", result.sections)
        self.assertIn("Depends on", result.sections)
        self.assertNotEqual(result.plan_sha256, plan_digest.digest_body(BASE).plan_sha256)

    def test_duplicate_heading_is_rejected(self) -> None:
        duplicate = BASE.replace(
            "## Design notes\n",
            "## Problem statement\n\nDuplicate.\n\n## Design notes\n",
        )
        with self.assertRaisesRegex(plan_digest.PlanDigestError, "duplicate managed heading"):
            plan_digest.digest_body(duplicate)

    def test_missing_empty_and_reordered_sections_are_rejected(self) -> None:
        missing = BASE.replace("## Dogfood brief\n\nN/A — workflow-only change.\n\n", "")
        with self.assertRaisesRegex(plan_digest.PlanDigestError, "missing required"):
            plan_digest.digest_body(missing)

        empty = BASE.replace("## Design notes\n\nUse durable artifacts.", "## Design notes\n")
        with self.assertRaisesRegex(plan_digest.PlanDigestError, "empty required"):
            plan_digest.digest_body(empty)

        reordered = BASE.replace(
            "## Problem statement\n\nThe workflow has stale control state.\n\n## Design notes\n\nUse durable artifacts.",
            "## Design notes\n\nUse durable artifacts.\n\n## Problem statement\n\nThe workflow has stale control state.",
        )
        with self.assertRaisesRegex(plan_digest.PlanDigestError, "scope-owned order"):
            plan_digest.digest_body(reordered)

    def test_missing_duplicate_and_invalid_routing_lines_are_rejected(self) -> None:
        cases = {
            "missing": BASE.replace("**Size:** m\n", ""),
            "duplicate": BASE.replace("**Size:** m\n", "**Size:** m\n**Size:** l\n"),
            "invalid model": BASE.replace("**Implementation model:** sonnet", "**Implementation model:** fable"),
            "not final": BASE.replace(
                "**Routing reason:** The plan is mechanical and bounded.",
                "**Routing reason:** The plan is mechanical and bounded.\nExtra plan text.",
            ),
        }
        for name, body in cases.items():
            with self.subTest(name=name), self.assertRaises(plan_digest.PlanDigestError):
                plan_digest.digest_body(body)

    def test_cli_emits_stable_strict_json(self) -> None:
        with tempfile.TemporaryDirectory() as temporary:
            body_path = Path(temporary) / "body.md"
            body_path.write_text(BASE, encoding="utf-8")
            completed = subprocess.run(
                [sys.executable, "-I", str(Path(plan_digest.__file__)), "--body-file", str(body_path)],
                check=False,
                capture_output=True,
                text=True,
            )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        payload = json.loads(completed.stdout)
        self.assertEqual(payload["version"], "aether-plan:v1")
        self.assertEqual(payload["size"], "m")
        self.assertEqual(payload["model"], "sonnet")


if __name__ == "__main__":
    unittest.main()
