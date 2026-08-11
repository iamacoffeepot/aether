#!/usr/bin/env python3
"""Regression tests for check-suppressions.py."""

from __future__ import annotations

import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-suppressions.py")
SPEC = importlib.util.spec_from_file_location("check_suppressions", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
scanner = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = scanner
SPEC.loader.exec_module(scanner)


class Repository:
    def __init__(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Suppression Test")
        self.git("config", "user.email", "suppression@example.invalid")

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

    def remove(self, path: str) -> None:
        (self.root / path).unlink()

    def commit(self, message: str) -> str:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)
        return self.git("rev-parse", "HEAD")

    def scan(self, base: str, head: str):
        return scanner.scan_repository(self.root, base, head)[2]


class ScannerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repository()

    def tearDown(self) -> None:
        self.repo.close()

    def test_rust_outer_inner_allow_expect_and_both_ignore_forms(self) -> None:
        self.repo.write("src/lib.rs", "pub fn baseline() {}\n")
        base = self.repo.commit("base")
        self.repo.write(
            "src/lib.rs",
            """#![expect(dead_code)]
#[allow(clippy::needless_return)]
fn one() { return; }

#[ignore]
fn two() {}

#[ignore = "needs a device"]
fn three() {}
""",
        )
        head = self.repo.commit("suppress")

        findings = self.repo.scan(base, head)

        self.assertEqual([item.line for item in findings], [1, 2, 5, 8])
        self.assertEqual([item.token for item in findings], ["expect(dead_code)", "allow(clippy::needless_return)", "ignore", "ignore"])

    def test_added_lint_inside_an_existing_multiline_attribute_is_found(self) -> None:
        self.repo.write(
            "src/lib.rs",
            """#[allow(
    dead_code,
)]
fn baseline() {}
""",
        )
        base = self.repo.commit("base")
        self.repo.write(
            "src/lib.rs",
            """#[allow(
    dead_code,
    clippy::needless_return,
)]
fn baseline() {}
""",
        )
        head = self.repo.commit("add lint")

        findings = self.repo.scan(base, head)

        self.assertEqual(len(findings), 1)
        self.assertEqual(findings[0].line, 3)
        self.assertEqual(findings[0].token, "allow(clippy::needless_return)")

    def test_new_multiline_attribute_reports_once(self) -> None:
        self.repo.write("src/lib.rs", "fn baseline() {}\n")
        base = self.repo.commit("base")
        self.repo.write(
            "src/lib.rs",
            """#[expect(
    dead_code,
    reason = "fixture",
)]
fn baseline() {}
""",
        )
        head = self.repo.commit("attribute")

        findings = self.repo.scan(base, head)

        self.assertEqual([(item.line, item.token) for item in findings], [(1, "expect(...)")])

    def test_comments_strings_and_unrelated_rust_attributes_do_not_match(self) -> None:
        self.repo.write("src/lib.rs", "fn baseline() {}\n")
        base = self.repo.commit("base")
        self.repo.write(
            "src/lib.rs",
            r'''// #[allow(clippy::all)]
const TEXT: &str = "#[ignore]";
const RAW: &str = r#"
++ b/not-a-diff-header
#[allow(clippy::all)]
"#;
/*
#[expect(dead_code)]
*/
#[cfg_attr(test, allow(dead_code))]
fn baseline() {}
''',
        )
        head = self.repo.commit("non suppressions")

        self.assertEqual(self.repo.scan(base, head), [])

    def test_removal_and_exact_rename_do_not_match(self) -> None:
        self.repo.write("src/old.rs", "#[allow(dead_code)]\nfn old() {}\n")
        base = self.repo.commit("base")
        self.repo.git("mv", "src/old.rs", "src/new.rs")
        head = self.repo.commit("rename")

        self.assertEqual(self.repo.scan(base, head), [])

        self.repo.write("src/new.rs", "fn old() {}\n")
        removed = self.repo.commit("remove")
        self.assertEqual(self.repo.scan(head, removed), [])

    def test_modified_rename_reports_the_head_path_and_exact_line(self) -> None:
        self.repo.write("src/old.rs", "fn old() {}\n")
        base = self.repo.commit("base")
        self.repo.git("mv", "src/old.rs", "src/new.rs")
        self.repo.write("src/new.rs", "#[allow(dead_code)]\nfn old() {}\n")
        head = self.repo.commit("rename and suppress")

        findings = self.repo.scan(base, head)

        self.assertEqual([(item.path, item.line) for item in findings], [("src/new.rs", 1)])

    def test_jscpd_and_machete_report_only_new_members(self) -> None:
        self.repo.write(
            ".jscpd.json",
            json.dumps({"ignore": ["**/target/**"], "nested": {"ignore": ["not-top-level"]}}, indent=2) + "\n",
        )
        self.repo.write(
            "crates/demo/Cargo.toml",
            """[package]
name = "demo"
version = "0.1.0"

[package.metadata.cargo-machete]
ignored = ["macro-dep"]

[package.metadata.unrelated]
ignored = ["not-machete"]
""",
        )
        base = self.repo.commit("base")
        self.repo.write(
            ".jscpd.json",
            json.dumps({"ignore": ["**/target/**", "**/generated/**"], "nested": {"ignore": ["still-unrelated"]}}, indent=2)
            + "\n",
        )
        self.repo.write(
            "crates/demo/Cargo.toml",
            """[package]
name = "demo"
version = "0.1.0"

[package.metadata.cargo-machete]
ignored = [
  "macro-dep",
  "generated-dep",
]

[package.metadata.unrelated]
ignored = ["changed-but-unrelated"]
""",
        )
        head = self.repo.commit("config suppressions")

        findings = self.repo.scan(base, head)

        self.assertEqual(len(findings), 2)
        self.assertEqual({item.token for item in findings}, {'jscpd-ignore("**/generated/**")', 'cargo-machete-ignored("generated-dep")'})
        for finding in findings:
            self.assertIn(finding.line, {4, 8})

    def test_inline_config_arrays_map_multiple_new_members_to_the_added_line(self) -> None:
        self.repo.write(".jscpd.json", '{"ignore": []}\n')
        self.repo.write("Cargo.toml", '[package]\nname="x"\nversion="0.1.0"\n')
        base = self.repo.commit("base")
        self.repo.write(".jscpd.json", '{"ignore": ["one", "two"]}\n')
        self.repo.write(
            "Cargo.toml",
            '[package]\nname="x"\nversion="0.1.0"\n\n[package.metadata.cargo-machete]\nignored = ["dep-a", "dep-b"]\n',
        )
        head = self.repo.commit("inline")

        findings = self.repo.scan(base, head)

        self.assertEqual(len(findings), 4)
        self.assertEqual({item.line for item in findings if item.path == ".jscpd.json"}, {1})
        self.assertEqual({item.line for item in findings if item.path == "Cargo.toml"}, {6})

    def test_clean_finding_and_invalid_ref_exit_codes(self) -> None:
        self.repo.write("src/lib.rs", "fn clean() {}\n")
        base = self.repo.commit("base")
        self.repo.write("src/lib.rs", "#[allow(dead_code)]\nfn clean() {}\n")
        head = self.repo.commit("finding")

        finding = subprocess.run(
            [sys.executable, str(SCRIPT), "--base", base, "--head", head],
            cwd=self.repo.root,
            check=False,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        clean = subprocess.run(
            [sys.executable, str(SCRIPT), "--base", base, "--head", base],
            cwd=self.repo.root,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        invalid = subprocess.run(
            [sys.executable, str(SCRIPT), "--base", "not-a-ref", "--head", head],
            cwd=self.repo.root,
            check=False,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )

        self.assertEqual(finding.returncode, 1)
        self.assertRegex(finding.stdout, r"src/lib\.rs:1 — allow\(dead_code\)")
        self.assertEqual(clean.returncode, 0)
        self.assertEqual(invalid.returncode, 2)


class SignoffTests(unittest.TestCase):
    base = "1" * 40
    head = "2" * 40
    number = 123

    def marker(self, **updates: object) -> str:
        payload: dict[str, object] = {"base_sha": self.base, "head_sha": self.head, "pull_request": self.number}
        payload.update(updates)
        return scanner.canonical_marker(payload)

    def response(self, body: str, editor: str | None = "iamacoffeepot", head: str | None = None) -> dict[str, object]:
        return {
            "data": {
                "repository": {
                    "owner": {"login": "iamacoffeepot"},
                    "pullRequest": {
                        "number": self.number,
                        "body": body,
                        "editor": None if editor is None else {"login": editor},
                        "headRefOid": head or self.head,
                    },
                }
            }
        }

    def trust(self, response: dict[str, object]) -> bool:
        return scanner.trusted_signoff(
            self.number,
            "iamacoffeepot/aether",
            self.base,
            self.head,
            "token",
            lambda _query, _variables, _token: response,
        )

    def test_exact_owner_edited_marker_is_trusted(self) -> None:
        self.assertTrue(self.trust(self.response("Summary\n\n" + self.marker())))

    def test_absent_duplicate_malformed_and_stale_markers_are_untrusted(self) -> None:
        cases = [
            "no marker",
            self.marker() + "\n" + self.marker(),
            '<!-- aether-suppression-signoff:v1 {"base_sha":} -->',
            self.marker(head_sha="3" * 40),
            '<!-- aether-suppression-signoff:v1 {"head_sha":"' + self.head + '","base_sha":"' + self.base + '","pull_request":123} -->',
        ]
        for body in cases:
            with self.subTest(body=body):
                self.assertFalse(self.trust(self.response(body)))

    def test_initial_agent_later_agent_and_missing_editor_are_untrusted(self) -> None:
        marker = self.marker()
        self.assertFalse(self.trust(self.response(marker, editor="github-actions[bot]")))
        self.assertFalse(self.trust(self.response(marker, editor="aether-agent")))
        self.assertFalse(self.trust(self.response(marker, editor=None)))

    def test_current_pr_head_must_match_the_scanned_head(self) -> None:
        self.assertFalse(self.trust(self.response(self.marker(), head="4" * 40)))

    def test_graphql_errors_fail_operationally(self) -> None:
        with self.assertRaises(scanner.OperationalError):
            self.trust({"errors": [{"message": "down"}]})

        def unavailable(_query, _variables, _token):
            raise scanner.OperationalError("network unavailable")

        with self.assertRaises(scanner.OperationalError):
            scanner.trusted_signoff(self.number, "iamacoffeepot/aether", self.base, self.head, "token", unavailable)


if __name__ == "__main__":
    unittest.main(verbosity=2)
