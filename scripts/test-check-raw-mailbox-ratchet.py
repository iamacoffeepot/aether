#!/usr/bin/env python3
"""Regression tests for check-raw-mailbox-ratchet.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-raw-mailbox-ratchet.py")
SPEC = importlib.util.spec_from_file_location("check_raw_mailbox_ratchet", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
scanner = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = scanner
SPEC.loader.exec_module(scanner)


class Repository:
    def __init__(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Raw Mailbox Ratchet Test")
        self.git("config", "user.email", "raw-mailbox-ratchet@example.invalid")

    def close(self) -> None:
        self.temp.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", *args],
            cwd=self.root,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        ).stdout.strip()

    def write(self, path: str, content: str) -> None:
        destination = self.root / path
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(content)

    def write_baseline(self, patterns: dict[str, int]) -> None:
        self.write(
            scanner.BASELINE_RELATIVE_PATH,
            json.dumps({"note": "test fixture", "patterns": patterns}),
        )

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def count(self) -> dict[str, int]:
        return scanner.count(self.root)

    def run(self, *args: str) -> tuple[int, list[str]]:
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            status = scanner.main(["--root", str(self.root), *args])
        return status, buffer.getvalue().splitlines()


def zero_baseline() -> dict[str, int]:
    return {name: 0 for name in scanner.PATTERN_NAMES}


class ScannerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repository()

    def tearDown(self) -> None:
        self.repo.close()

    def test_matching_counts_pass_with_no_output(self) -> None:
        self.repo.write("crates/example/src/lib.rs", "fn f() {}\n")
        self.repo.write_baseline(zero_baseline())
        self.repo.commit("clean tree")

        status, output = self.repo.run()

        self.assertEqual(status, 0)
        self.assertEqual(output, [])

    def test_added_call_site_fails_with_rise_message(self) -> None:
        self.repo.write("crates/example/src/lib.rs", "fn f(x: Actor) {\n    mailbox_id_from_path(y);\n}\n")
        self.repo.write_baseline(zero_baseline())
        self.repo.commit("new mailbox_id_from_path( call site")

        status, output = self.repo.run()

        self.assertEqual(status, 1)
        text = "\n".join(output)
        self.assertIn("raw-mailbox ratchet: `mailbox_id_from_path(` — baseline 0, head 1 (+1)", text)
        self.assertIn("an old-door count may not rise during expand and migrate", text)
        self.assertIn("Raising the baseline is also a failure.", text)

    def test_removed_call_site_fails_naming_the_baseline_edit(self) -> None:
        baseline = zero_baseline()
        baseline["mailbox_id_from_path("] = 2
        self.repo.write("crates/example/src/lib.rs", "fn f() {}\n")
        self.repo.write_baseline(baseline)
        self.repo.commit("call sites already gone")

        status, output = self.repo.run()

        self.assertEqual(status, 1)
        text = "\n".join(output)
        self.assertIn("raw-mailbox ratchet: `mailbox_id_from_path(` — baseline 2, head 0 (-2)", text)
        self.assertIn('Set "mailbox_id_from_path(" to 0 in scripts/raw-mailbox-baseline.json, in this same change.', text)

    def test_absent_row_reads_as_zero_baseline(self) -> None:
        baseline = zero_baseline()
        del baseline["mailbox_id_from_name("]
        self.repo.write("crates/example/src/lib.rs", "fn f() {}\n")
        self.repo.write_baseline(baseline)
        self.repo.commit("no mailbox_id_from_name( row, no mailbox_id_from_name( sites")

        status, output = self.repo.run()
        self.assertEqual(status, 0)
        self.assertEqual(output, [])

        self.repo.write("crates/example/src/lib.rs", "fn f(ctx: Ctx) {\n    mailbox_id_from_name(m);\n}\n")
        self.repo.commit("one mailbox_id_from_name( site with no baseline row")

        status, output = self.repo.run()
        self.assertEqual(status, 1)
        text = "\n".join(output)
        self.assertIn("raw-mailbox ratchet: `mailbox_id_from_name(` — baseline 0, head 1 (+1)", text)

    def test_one_way_rule_fails_when_head_baseline_exceeds_base(self) -> None:
        baseline = zero_baseline()
        baseline["mailbox_id_from_path("] = 1
        self.repo.write("crates/example/src/lib.rs", "fn f(x: Actor) {\n    mailbox_id_from_path(y);\n}\n")
        self.repo.write_baseline(baseline)
        base_sha = self.repo.commit("base: one mailbox_id_from_path( site, baseline 1")

        baseline["mailbox_id_from_path("] = 2
        self.repo.write(
            "crates/example/src/lib.rs",
            "fn f(x: Actor) {\n    mailbox_id_from_path(y);\n    mailbox_id_from_path(z);\n}\n",
        )
        self.repo.write_baseline(baseline)
        self.repo.commit("head: two mailbox_id_from_path( sites, baseline raised to match")

        status, output = self.repo.run("--base", base_sha)

        self.assertEqual(status, 1)
        text = "\n".join(output)
        self.assertIn(f"raw-mailbox ratchet: `mailbox_id_from_path(` — baseline raised 1 -> 2 against {base_sha}", text)
        self.assertIn("The ratchet is one-way. Restore the baseline and drop the new call sites.", text)

    def test_base_without_baseline_file_bootstraps(self) -> None:
        self.repo.write("crates/example/src/lib.rs", "fn f() {}\n")
        base_sha = self.repo.commit("pre-gate base: no baseline file at all")

        self.repo.write_baseline(zero_baseline())
        self.repo.commit("head: baseline file introduced")

        status, output = self.repo.run("--base", base_sha)

        self.assertEqual(status, 0)
        text = "\n".join(output)
        self.assertIn(f"no baseline at base {base_sha}", text)
        self.assertIn("bootstrap", text)

    def test_untracked_baseline_row_is_a_notice_not_a_failure(self) -> None:
        baseline = zero_baseline()
        baseline["a_retired_door("] = 5
        self.repo.write("crates/example/src/lib.rs", "fn f() {}\n")
        self.repo.write_baseline(baseline)
        self.repo.commit("baseline carries a row this scanner does not track")

        status, output = self.repo.run()

        self.assertEqual(status, 0)
        text = "\n".join(output)
        self.assertIn('baseline row "a_retired_door(" is not tracked by this scanner; ignored.', text)

    def test_comment_stripped_attribute_counted_ufcs_counted(self) -> None:
        self.repo.write(
            "crates/example/src/lib.rs",
            "fn f(x: Actor) {\n"
            "    // mailbox_id_from_path(y); commented out, must not count\n"
            "    mailbox_id_from_path(y); // a real call\n"
            "    <T as Trait>::mailer();\n"
            "}\n"
            "#[allow(clippy::disallowed_methods)] // aether-suppression-request: legacy\n"
            "fn g() {}\n",
        )
        self.repo.commit("comment stripping and UFCS coverage")

        counts = self.repo.count()

        self.assertEqual(counts["mailbox_id_from_path("], 1)
        self.assertEqual(counts["mailer()"], 1)
        self.assertEqual(counts["clippy::disallowed_methods"], 1)

    def test_malformed_or_non_integer_baseline_is_an_operational_error(self) -> None:
        self.repo.write("crates/example/src/lib.rs", "fn f() {}\n")
        self.repo.write(scanner.BASELINE_RELATIVE_PATH, "{not valid json")
        self.repo.commit("malformed baseline")

        status, _ = self.repo.run()
        self.assertEqual(status, 2)

        self.repo.write(
            scanner.BASELINE_RELATIVE_PATH,
            json.dumps({"note": "bad", "patterns": {"mailbox_id_from_path(": "3"}}),
        )
        self.repo.commit("non-integer baseline count")

        status, _ = self.repo.run()
        self.assertEqual(status, 2)


if __name__ == "__main__":
    unittest.main()
