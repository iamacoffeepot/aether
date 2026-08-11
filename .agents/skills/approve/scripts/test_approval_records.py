from __future__ import annotations

import importlib.util
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("approval_records.py")
SPEC = importlib.util.spec_from_file_location("approval_records", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
approval_records = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(approval_records)


def payload(issue: int = 4784) -> dict[str, object]:
    return {
        "authority": "owner",
        "base_sha": "a" * 40,
        "effective_tier": "human",
        "issue": issue,
        "model": "sonnet",
        "plan_sha256": "b" * 64,
        "policy_tier": "human",
        "size": "m",
    }


def marker(record: dict[str, object]) -> str:
    encoded = json.dumps(record, sort_keys=True, separators=(",", ":"))
    return f"<!-- aether-approval:v2 {encoded} -->"


class ApprovalRecordsTest(unittest.TestCase):
    def test_preserves_valid_history_order_before_problem_statement(self) -> None:
        first = payload()
        second = payload()
        second["base_sha"] = "c" * 40
        body = f"## Description\n\nText\n\n{marker(first)}\n{marker(second)}\n\n## Problem statement\n\nPlan\n"

        self.assertEqual(approval_records.parse_records(body, issue=4784), [first, second])

    def test_ignores_records_inside_managed_sections(self) -> None:
        record = payload()
        body = f"## Problem statement\n\n{marker(record)}\n"

        self.assertEqual(approval_records.parse_records(body, issue=4784), [])

    def test_accepts_crlf_body(self) -> None:
        record = payload()
        body = f"{marker(record)}\r\n\r\n## Problem statement\r\n"

        self.assertEqual(approval_records.parse_records(body), [record])

    def test_rejects_malformed_marker_lookalike_in_prefix(self) -> None:
        body = "<!-- aether-approval:v2 nope -->\n\n## Problem statement\n"

        with self.assertRaisesRegex(approval_records.ApprovalRecordError, "malformed"):
            approval_records.parse_records(body)

    def test_rejects_invalid_json(self) -> None:
        body = "<!-- aether-approval:v2 {bad} -->\n## Problem statement\n"

        with self.assertRaisesRegex(approval_records.ApprovalRecordError, "invalid approval JSON"):
            approval_records.parse_records(body)

    def test_rejects_wrong_keys(self) -> None:
        record = payload()
        record["extra"] = "value"

        with self.assertRaisesRegex(approval_records.ApprovalRecordError, "exactly the eight"):
            approval_records.parse_records(f"{marker(record)}\n## Problem statement\n")

    def test_rejects_wrong_types_and_enums(self) -> None:
        cases = [
            ("issue", True),
            ("authority", "collaborator"),
            ("effective_tier", "manual"),
            ("model", "terra"),
            ("policy_tier", 1),
            ("size", "xl"),
            ("base_sha", "A" * 40),
            ("plan_sha256", "b" * 63),
        ]
        for key, value in cases:
            with self.subTest(key=key, value=value):
                record = payload()
                record[key] = value
                with self.assertRaises(approval_records.ApprovalRecordError):
                    approval_records.parse_records(f"{marker(record)}\n## Problem statement\n")

    def test_rejects_noncanonical_json(self) -> None:
        record = payload()
        encoded = json.dumps(record, sort_keys=False)
        body = f"<!-- aether-approval:v2 {encoded} -->\n## Problem statement\n"

        with self.assertRaisesRegex(approval_records.ApprovalRecordError, "not canonical"):
            approval_records.parse_records(body)

    def test_rejects_wrong_issue_identity(self) -> None:
        with self.assertRaisesRegex(approval_records.ApprovalRecordError, "does not match"):
            approval_records.parse_records(f"{marker(payload(12))}\n## Problem statement\n", issue=4784)

    def test_cli_emits_stable_json(self) -> None:
        record = payload()
        with tempfile.TemporaryDirectory() as directory:
            body_path = Path(directory) / "body.md"
            body_path.write_text(f"{marker(record)}\n## Problem statement\n", encoding="utf-8")
            result = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), "--body-file", str(body_path), "--issue", "4784"],
                check=False,
                capture_output=True,
                text=True,
            )

        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout, json.dumps({"records": [record]}, sort_keys=True, separators=(",", ":")) + "\n")


if __name__ == "__main__":
    unittest.main()
