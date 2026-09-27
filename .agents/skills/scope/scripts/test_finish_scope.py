#!/usr/bin/env python3
"""Tests for the scope skill's finishing script."""

from __future__ import annotations

import contextlib
import io
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from typing import Any

import finish_scope


ROOT = Path(finish_scope.__file__).resolve().parents[4]
CANONICAL_SOURCE = (ROOT / "scripts" / "surface-match.py").read_text(encoding="utf-8")
POLICY = """\
default = "judge"

[[rules]]
glob = "crates/aether-kit/**"
tier = "auto"
"""

PREFIX = (
    "## Description\n\nOriginal user prose.\n\n"
    '<!-- aether-approval:v2 {"authority":"owner","base_sha":"'
    + ("a" * 40)
    + '","effective_tier":"human","issue":501,"model":"sonnet","plan_sha256":"'
    + ("b" * 64)
    + '","policy_tier":"human","size":"m"} -->\n\n'
)
TITLE = "chore: fix bug"

IMPLEMENTATION_PLAN = (
    "1. Do the thing.\n\n**Size:** m\n**Implementation model:** sonnet\n**Routing reason:** Mechanical.\n"
)
BASE_SECTIONS = {
    "Problem statement": "The workflow lacks a tracker.",
    "Design notes": (
        "### Chosen approach\n\nDo the thing to `crates/aether-kit/src/lib.rs`.\n\n"
        "### Rejected options\n\n- **Nothing** — insufficient.\n\n### Affected surfaces\n\nNone.\n"
    ),
    "Implementation plan": IMPLEMENTATION_PLAN,
    "Declared surface": "```\ncrates/aether-kit/**\n```\n",
}


def render_sections(sections: dict[str, str]) -> str:
    names = [name for name in finish_scope.plan_digest.MANAGED_ORDER if name in sections]
    parts = []
    for index, name in enumerate(names):
        parts.append(f"## {name}\n\n{sections[name].strip()}\n")
        if index + 1 < len(names):
            parts.append("\n")
    return "".join(parts)


def render_body(prefix: str, sections: dict[str, str]) -> str:
    return prefix + render_sections(sections)


class FakeGitHubClient(finish_scope.GitHubClient):
    def __init__(
        self,
        issue_states: list[dict[str, Any]],
        pull_refs: list[str] | None = None,
        patch_error: bool = False,
    ) -> None:
        self.issue_states = list(issue_states)
        self.pull_refs = pull_refs or []
        self.patch_calls: list[str] = []
        self.patch_error = patch_error
        self._read_index = 0

    def read_issue(self, issue: int) -> dict[str, Any]:
        index = min(self._read_index, len(self.issue_states) - 1)
        self._read_index += 1
        return dict(self.issue_states[index])

    def open_pull_head_refs(self) -> list[str]:
        return list(self.pull_refs)

    def patch_body(self, issue: int, request_file: Path) -> str:
        self.patch_calls.append(Path(request_file).read_text(encoding="utf-8"))
        if self.patch_error:
            raise finish_scope.GitHubError("gh api patch failed")
        return "2026-09-26T00:00:00Z"


class Fixture:
    def __init__(self, test: unittest.TestCase, policy: str = POLICY) -> None:
        self.test = test
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self._git("init", "-q")
        self._git("config", "user.email", "finisher@example.invalid")
        self._git("config", "user.name", "Finisher Test")

        files = {
            "approval-policy.toml": policy,
            "scripts/surface-match.py": CANONICAL_SOURCE,
            "Cargo.toml": "[workspace]\n",
            "crates/aether-kit/Cargo.toml": "[package]\n",
            "crates/aether-kit/src/lib.rs": "kit\n",
            "docs/guide/page.md": "guide\n",
        }
        for relative, contents in files.items():
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")
        self._git("add", "--all")
        self._git("commit", "-q", "-m", "fixture")
        self.base = self._git("rev-parse", "HEAD").stdout.strip()
        self._git("update-ref", "refs/remotes/origin/main", self.base)

    def close(self) -> None:
        self.temporary.cleanup()

    def _git(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            ["git", "-C", str(self.repo), *arguments], check=False, capture_output=True, text=True
        )
        self.test.assertEqual(completed.returncode, 0, completed.stderr)
        return completed

    def run(
        self,
        *,
        issue: int,
        snapshot: dict[str, Any],
        sections: str,
        github: finish_scope.GitHubClient,
        write: bool = False,
    ) -> tuple[int, dict[str, Any]]:
        snapshot_file = self.root / f"snapshot-{id(snapshot)}.json"
        sections_file = self.root / f"sections-{id(sections)}.md"
        snapshot_file.write_text(json.dumps(snapshot), encoding="utf-8")
        sections_file.write_text(sections, encoding="utf-8")

        argv = [
            "--repo",
            str(self.repo),
            "--base",
            self.base,
            "--issue",
            str(issue),
            "--snapshot-file",
            str(snapshot_file),
            "--sections-file",
            str(sections_file),
        ]
        if write:
            argv.append("--write")

        captured = io.StringIO()
        with contextlib.redirect_stdout(captured):
            exit_code = finish_scope.main(argv, github=github)
        return exit_code, json.loads(captured.getvalue())


def issue(number: int, body: str, *, title: str = TITLE, state: str = "open") -> dict[str, Any]:
    return {"number": number, "title": title, "body": body, "state": state}


class FinishScopeTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = Fixture(self)

    def tearDown(self) -> None:
        self.fixture.close()

    def test_splice_preserves_unmanaged_prose_and_approval_line(self) -> None:
        # Catches the splice losing or reordering the hidden approval line or
        # the user's own prose in the unmanaged prefix.
        fresh_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(501, fresh_body)
        new_sections = dict(BASE_SECTIONS)
        new_sections["Problem statement"] = "The workflow lacks a tracker, drafted anew."

        exit_code, report = self.fixture.run(
            issue=501,
            snapshot=snapshot,
            sections=render_sections(new_sections),
            github=FakeGitHubClient([snapshot]),
        )

        self.assertEqual(report["outcome"], "validated", report["failures"])
        self.assertEqual(exit_code, finish_scope.EXIT_OK)
        staged = Path(report["staged_body"]).read_text(encoding="utf-8")
        self.assertTrue(staged.startswith(PREFIX))
        self.assertIn("Original user prose.", staged)
        self.assertIn("drafted anew", staged)

    def test_missing_depends_on_is_inserted_in_scope_order(self) -> None:
        # Catches a missing section being appended or inserted out of
        # MANAGED_ORDER, which plan_digest.digest_body would reject.
        fresh_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(502, fresh_body)
        new_sections = dict(BASE_SECTIONS)
        new_sections["Depends on"] = "- #42 — needed first."

        exit_code, report = self.fixture.run(
            issue=502,
            snapshot=snapshot,
            sections=render_sections(new_sections),
            github=FakeGitHubClient([snapshot]),
        )

        self.assertEqual(report["outcome"], "validated", report["failures"])
        self.assertEqual(exit_code, finish_scope.EXIT_OK)
        staged = Path(report["staged_body"]).read_text(encoding="utf-8")
        self.assertLess(staged.index("## Implementation plan"), staged.index("## Depends on"))
        self.assertLess(staged.index("## Depends on"), staged.index("## Declared surface"))

    def test_splice_with_no_existing_managed_sections_and_no_trailing_newline(self) -> None:
        # Catches a managed heading being glued directly onto trailing prose
        # with no newline, the common case when first scoping an issue that
        # has no managed sections at all yet.
        fresh_body = "## Description\n\nfoo"
        sections = finish_scope._parse_sections_file(render_sections(BASE_SECTIONS))

        proposed_body, dropped = finish_scope._splice(fresh_body, {}, sections)

        self.assertEqual(dropped, [])
        self.assertIn("foo\n\n## Problem statement\n", proposed_body)
        digest = finish_scope.plan_digest.digest_body(proposed_body)
        self.assertEqual(
            set(digest.sections), {"Problem statement", "Design notes", "Implementation plan", "Declared surface"}
        )

    def test_splice_keeps_blank_lines_around_a_trailing_unmanaged_heading(self) -> None:
        # Catches losing the blank line before or after a trailing unmanaged
        # H2 that follows the managed sections, or corrupting its own bytes,
        # when a new managed section is appended after it.
        fresh_body = "## Problem statement\n\nold\n\n## Notes\n\nuser trailing"
        fresh_bounds = finish_scope.plan_digest.managed_span_bounds(fresh_body)
        sections = finish_scope._parse_sections_file(
            render_sections({"Problem statement": "new problem text.", "Design notes": "new design text."})
        )

        proposed_body, dropped = finish_scope._splice(fresh_body, fresh_bounds, sections)

        self.assertEqual(dropped, [])
        self.assertIn("\n\n## Notes\n\nuser trailing\n\n## Design notes\n\nnew design text.\n", proposed_body)

    def test_splice_keeps_the_blank_line_before_a_trailing_unmanaged_heading(self) -> None:
        # Catches the last managed section being closed with a single newline,
        # gluing the unmanaged H2 that follows it onto the section's last line.
        fresh_body = "## Problem statement\n\nold\n\n## Notes\n\nuser trailing"
        fresh_bounds = finish_scope.plan_digest.managed_span_bounds(fresh_body)
        sections = finish_scope._parse_sections_file(render_sections({"Problem statement": "new problem text."}))

        proposed_body, _ = finish_scope._splice(fresh_body, fresh_bounds, sections)

        self.assertEqual(proposed_body, "## Problem statement\n\nnew problem text.\n\n## Notes\n\nuser trailing")

    def test_concurrent_managed_edit_aborts_instead_of_overwriting(self) -> None:
        # Catches the script silently overwriting someone else's concurrent
        # managed-section edit instead of aborting.
        snapshot_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(503, snapshot_body)
        concurrent_sections = dict(BASE_SECTIONS)
        concurrent_sections["Problem statement"] = "Someone else edited this concurrently."
        fresh_issue = issue(503, render_body(PREFIX, concurrent_sections))

        new_sections = dict(BASE_SECTIONS)
        new_sections["Problem statement"] = "Drafted new problem text."
        github = FakeGitHubClient([fresh_issue, fresh_issue])

        exit_code, report = self.fixture.run(
            issue=503,
            snapshot=snapshot,
            sections=render_sections(new_sections),
            github=github,
            write=True,
        )

        self.assertEqual(report["outcome"], "aborted")
        self.assertEqual(exit_code, finish_scope.EXIT_ABORT)
        self.assertTrue(any("changed concurrently" in failure for failure in report["failures"]))
        self.assertEqual(github.patch_calls, [])

    def test_unmanaged_only_concurrent_edit_keeps_the_fresh_prose(self) -> None:
        # Catches the splice using the stale snapshot's unmanaged prefix
        # instead of the freshly re-read one, losing the user's new prose.
        snapshot_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(504, snapshot_body)
        fresh_prefix = PREFIX.replace("Original user prose.", "Original user prose, plus a new remark.")
        fresh_issue = issue(504, render_body(fresh_prefix, BASE_SECTIONS))

        exit_code, report = self.fixture.run(
            issue=504,
            snapshot=snapshot,
            sections=render_sections(BASE_SECTIONS),
            github=FakeGitHubClient([fresh_issue]),
        )

        self.assertEqual(report["outcome"], "validated", report["failures"])
        self.assertEqual(exit_code, finish_scope.EXIT_OK)
        self.assertTrue(report["unmanaged_refreshed"])
        staged = Path(report["staged_body"]).read_text(encoding="utf-8")
        self.assertIn("plus a new remark", staged)

    def test_independent_defects_are_all_listed(self) -> None:
        # Catches the script stopping at the first defect instead of
        # accumulating every failure in one run.
        fresh_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(505, fresh_body)
        new_sections = dict(BASE_SECTIONS)
        new_sections["Implementation plan"] = IMPLEMENTATION_PLAN.replace("**Size:** m", "**Size:** xl")
        new_sections["Design notes"] = BASE_SECTIONS["Design notes"].replace(
            "`crates/aether-kit/src/lib.rs`", "`crates/aether-kit/src/lib.rs` and `crates/missing/src/lib.rs`"
        )

        exit_code, report = self.fixture.run(
            issue=505,
            snapshot=snapshot,
            sections=render_sections(new_sections),
            github=FakeGitHubClient([snapshot]),
        )

        self.assertEqual(report["outcome"], "invalid")
        self.assertEqual(exit_code, finish_scope.EXIT_INVALID)
        self.assertTrue(any("plan digest" in failure for failure in report["failures"]))
        self.assertTrue(any("cited path absent" in failure for failure in report["failures"]))

    def test_creation_already_tracked_and_missing_citation_both_fail(self) -> None:
        # Catches a (create) citation for an already-tracked path, and a
        # cited path absent at the base, each wrongly passing as grounded.
        cases = {
            "existing_creation": (
                "``crates/aether-kit/Cargo.toml (create)``",
                "creation already exists at base",
            ),
            "missing_path": ("`crates/does-not-exist/src/lib.rs`", "cited path absent at base"),
        }
        for name, (citation, expected) in cases.items():
            with self.subTest(name=name):
                issue_number = 600 + hash(name) % 100
                new_sections = dict(BASE_SECTIONS)
                new_sections["Design notes"] = f"### Chosen approach\n\n{citation}.\n"
                fresh_body = render_body(PREFIX, BASE_SECTIONS)
                snapshot = issue(issue_number, fresh_body)

                exit_code, report = self.fixture.run(
                    issue=issue_number,
                    snapshot=snapshot,
                    sections=render_sections(new_sections),
                    github=FakeGitHubClient([snapshot]),
                )

                self.assertEqual(report["outcome"], "invalid")
                self.assertEqual(exit_code, finish_scope.EXIT_INVALID)
                self.assertTrue(any(expected in failure for failure in report["failures"]))

    def test_non_path_code_spans_are_silently_ignored(self) -> None:
        # Catches a non-path code span such as `origin/main` or a bare
        # `plan_digest.digest_body` symbol reference being wrongly flagged as
        # a missing target instead of being silently ignored: only a span
        # that is "kept" (SAFE_TARGET, no '*', top-level entry at base) is
        # scrutinized further, per the Targets rule in the Plan's step 2.
        new_sections = dict(BASE_SECTIONS)
        new_sections["Design notes"] = (
            BASE_SECTIONS["Design notes"].rstrip()
            + "\n\nCompare against `origin/main` and call `plan_digest.digest_body` on it.\n"
        )
        fresh_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(506, fresh_body)

        exit_code, report = self.fixture.run(
            issue=506,
            snapshot=snapshot,
            sections=render_sections(new_sections),
            github=FakeGitHubClient([snapshot]),
        )

        self.assertEqual(report["outcome"], "validated", report["failures"])
        self.assertEqual(exit_code, finish_scope.EXIT_OK)
        self.assertEqual(report["failures"], [])

    def test_dry_run_never_calls_patch_body(self) -> None:
        # Catches a dry run (no --write) still sending the file-backed PATCH.
        fresh_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(507, fresh_body)
        github = FakeGitHubClient([snapshot])

        exit_code, report = self.fixture.run(
            issue=507,
            snapshot=snapshot,
            sections=render_sections(BASE_SECTIONS),
            github=github,
            write=False,
        )

        self.assertEqual(report["outcome"], "validated", report["failures"])
        self.assertEqual(exit_code, finish_scope.EXIT_OK)
        self.assertEqual(github.patch_calls, [])
        self.assertFalse(report["written"])

    def test_write_succeeds_when_the_remote_body_matches(self) -> None:
        # Catches a fully valid write path failing to report "written" or
        # updated_at even though the post-write re-read matches exactly.
        fresh_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(508, fresh_body)
        github = FakeGitHubClient([snapshot, snapshot])

        exit_code, report = self.fixture.run(
            issue=508,
            snapshot=snapshot,
            sections=render_sections(BASE_SECTIONS),
            github=github,
            write=True,
        )

        self.assertEqual(len(github.patch_calls), 1)
        self.assertEqual(report["outcome"], "written", report["failures"])
        self.assertEqual(exit_code, finish_scope.EXIT_OK)
        self.assertTrue(report["written"])
        self.assertEqual(report["updated_at"], "2026-09-26T00:00:00Z")

    def test_post_write_mismatch_reports_written_and_aborted(self) -> None:
        # Catches a post-write re-read that no longer matches the staged
        # body or digest being silently reported as a clean success.
        fresh_body = render_body(PREFIX, BASE_SECTIONS)
        snapshot = issue(509, fresh_body)
        raced_sections = dict(BASE_SECTIONS)
        raced_sections["Problem statement"] = "Someone raced the write."
        raced_issue = issue(509, render_body(PREFIX, raced_sections))
        github = FakeGitHubClient([snapshot, raced_issue])

        exit_code, report = self.fixture.run(
            issue=509,
            snapshot=snapshot,
            sections=render_sections(BASE_SECTIONS),
            github=github,
            write=True,
        )

        self.assertEqual(len(github.patch_calls), 1)
        self.assertEqual(report["outcome"], "aborted")
        self.assertEqual(exit_code, finish_scope.EXIT_ABORT)
        self.assertTrue(report["written"])
        self.assertTrue(any("post-write re-read" in failure for failure in report["failures"]))


if __name__ == "__main__":
    unittest.main()
