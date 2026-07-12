#!/usr/bin/env python3
"""Tests for the captured-ref Codex approval-tier resolver."""

from __future__ import annotations

import json
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

import resolve_approval_tier as resolver


ROOT = Path(resolver.__file__).resolve().parents[4]
CANONICAL_SOURCE = (ROOT / "scripts" / "surface-match.py").read_text(encoding="utf-8")
POLICY = """\
default: judge
rules:
  - glob: "/Cargo.toml"
    tier: human
  - glob: "crates/*/Cargo.toml"
    tier: human
  - glob: "crates/aether-data/**"
    tier: human
  - glob: "docs/adr/**"
    tier: human
  - glob: ".agents/**"
    tier: human
  - glob: "docs/guide/**"
    tier: auto
  - glob: "crates/aether-kit/**"
    tier: auto
  - glob: "crates/aether-capabilities/**"
    tier: judge
"""


class ResolverFixture:
    def __init__(self, test: unittest.TestCase, policy: str = POLICY) -> None:
        self.test = test
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.repo = self.root / "repo"
        self.repo.mkdir()
        self._git("init", "-q")
        self._git("config", "user.email", "resolver@example.invalid")
        self._git("config", "user.name", "Resolver Test")

        files = {
            ".agents/skills/approve/SKILL.md": "approve\n",
            ".github/approval-policy.yml": policy,
            "Cargo.toml": "[workspace]\n",
            "Cargo.lock": "# lock\n",
            "crates/aether-capabilities/src/lib.rs": "cap\n",
            "crates/aether-data/src/lib.rs": "data\n",
            "crates/aether-kit/Cargo.toml": "[package]\n",
            "crates/aether-kit/src/lib.rs": "kit\n",
            "docs/adr/0001-example.md": "adr\n",
            "docs/guide/page.md": "guide\n",
            "scripts/surface-match.py": CANONICAL_SOURCE,
            "unclassified.txt": "default\n",
        }
        for relative, contents in files.items():
            path = self.repo / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(contents, encoding="utf-8")
        self._git("add", "--all")
        self._git("commit", "-q", "-m", "fixture")
        self.ref = self._git("rev-parse", "HEAD").stdout.strip()
        # The resolver only executes a matcher reachable from origin/main.
        self._git("update-ref", "refs/remotes/origin/main", self.ref)

    def close(self) -> None:
        self.temporary.cleanup()

    def install_commit_replacement(self) -> str:
        matcher = self.repo / "scripts" / "surface-match.py"
        matcher.write_text('raise RuntimeError("replacement matcher executed")\n', encoding="utf-8")
        self._git("add", "scripts/surface-match.py")
        self._git("commit", "-q", "-m", "hostile replacement")
        replacement = self._git("rev-parse", "HEAD").stdout.strip()
        self._git("replace", self.ref, replacement)
        return replacement

    def _git(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        completed = subprocess.run(
            ["git", "-C", str(self.repo), *arguments],
            check=False,
            capture_output=True,
            text=True,
        )
        self.test.assertEqual(completed.returncode, 0, completed.stderr)
        return completed

    def invoke(
        self, surfaces: list[str], targets: list[str], ref: str | None = None
    ) -> subprocess.CompletedProcess[str]:
        surface_file = self.root / "surfaces.txt"
        target_file = self.root / "targets.txt"
        surface_file.write_text("\n".join(surfaces) + ("\n" if surfaces else ""), encoding="utf-8")
        target_file.write_text("\n".join(targets) + ("\n" if targets else ""), encoding="utf-8")
        return subprocess.run(
            [
                sys.executable,
                "-I",
                str(Path(resolver.__file__)),
                "--repo",
                str(self.repo),
                "--ref",
                ref if ref is not None else self.ref,
                "--surface-file",
                str(surface_file),
                "--targets-file",
                str(target_file),
            ],
            check=False,
            capture_output=True,
            text=True,
        )


class ResolverTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = ResolverFixture(self)

    def tearDown(self) -> None:
        self.fixture.close()

    def success(self, surfaces: list[str], targets: list[str]) -> dict[str, object]:
        completed = self.fixture.invoke(surfaces, targets)
        self.assertEqual(completed.returncode, 0, completed.stderr)
        return json.loads(completed.stdout)

    def test_auto_judge_human_and_default(self) -> None:
        cases = [
            (["docs/guide/**"], ["docs/guide/page.md"], "auto"),
            (["crates/aether-capabilities/src/**"], ["crates/aether-capabilities/src/lib.rs"], "judge"),
            ([".agents/skills/approve/SKILL.md"], [".agents/skills/approve/SKILL.md"], "human"),
            (["unclassified.txt"], ["unclassified.txt"], "judge"),
        ]
        for surfaces, targets, expected in cases:
            with self.subTest(surfaces=surfaces):
                result = self.success(surfaces, targets)
                self.assertEqual(result["tier"], expected)
                self.assertEqual(result["default"], "judge")

    def test_subtree_resolution_is_set_sound(self) -> None:
        cases = [
            (["crates/aether-kit/src/**"], ["crates/aether-kit/src/lib.rs"], "auto"),
            (["crates/aether-kit/**"], ["crates/aether-kit/src/lib.rs"], "human"),
            (["docs/**"], ["docs/guide/page.md"], "human"),
            (["new-top/**"], ["new-top/new.txt"], "judge"),
        ]
        for surfaces, targets, expected in cases:
            with self.subTest(surfaces=surfaces):
                self.assertEqual(self.success(surfaces, targets)["tier"], expected)

    def test_mixed_surface_uses_most_restrictive(self) -> None:
        result = self.success(
            ["docs/guide/**", "crates/aether-data/src/lib.rs"],
            ["docs/guide/new.md", "crates/aether-data/src/lib.rs"],
        )
        self.assertEqual(result["tier"], "human")
        self.assertEqual([surface["tier"] for surface in result["surfaces"]], ["auto", "human"])

    def test_output_carries_canonical_evidence(self) -> None:
        result = self.success(["crates/aether-kit/**"], ["crates/aether-kit/src/lib.rs"])
        surface = result["surfaces"][0]
        self.assertTrue(surface["default_applies"] is False)
        self.assertIn(
            {"glob": "crates/*/Cargo.toml", "tier": "human"},
            surface["matched_policy_rules"],
        )
        self.assertEqual(result["targets"][0]["tier"], "auto")
        self.assertEqual(surface["tier"], "human")

    def test_uncovered_target_and_orphan_surface_fail(self) -> None:
        completed = self.fixture.invoke(["docs/guide/**"], ["crates/aether-data/src/lib.rs"])
        self.assertEqual(completed.returncode, 2)
        self.assertIn("outside the declared surface", completed.stderr)

        completed = self.fixture.invoke(
            ["docs/guide/**", "missing/**"],
            ["docs/guide/page.md"],
        )
        self.assertEqual(completed.returncode, 2)
        self.assertIn("matches no tracked or target path", completed.stderr)

    def test_empty_inputs_and_directory_as_concrete_path_fail(self) -> None:
        for surfaces, targets, message in [
            ([], ["docs/guide/page.md"], "contains no surface patterns"),
            (["docs/guide/**"], [], "contains no concrete paths"),
            (["docs/guide"], ["docs/guide/page.md"], "not tracked or planned"),
        ]:
            with self.subTest(surfaces=surfaces, targets=targets):
                completed = self.fixture.invoke(surfaces, targets)
                self.assertEqual(completed.returncode, 2)
                self.assertIn(message, completed.stderr)

    def test_unsafe_or_cross_root_surface_fails(self) -> None:
        for surface, message in [
            ("../**", "unsafe path segment"),
            ("**", "escape-hatch"),
            ("crates/**", "escape-hatch"),
            ("docs/*", "literal directory prefix"),
            ("crates/aether-*/future/**", "literal directory prefix"),
            ("# docs/guide/**", "comments are not allowed"),
        ]:
            with self.subTest(surface=surface):
                completed = self.fixture.invoke([surface], ["docs/guide/page.md"])
                self.assertEqual(completed.returncode, 2)
                self.assertIn(message, completed.stderr)

    def test_malformed_policy_fails(self) -> None:
        self.fixture.close()
        self.fixture = ResolverFixture(
            self,
            'default: judge\nrules:\n  - glob: "docs/guide/**"\n    tier: owner\n',
        )
        completed = self.fixture.invoke(["docs/guide/**"], ["docs/guide/page.md"])
        self.assertEqual(completed.returncode, 2)
        self.assertIn("empty or malformed", completed.stderr)

    def test_local_replace_ref_cannot_substitute_captured_commit(self) -> None:
        self.fixture.install_commit_replacement()
        result = self.success(["docs/guide/**"], ["docs/guide/page.md"])
        self.assertEqual(result["ref"], self.fixture.ref)
        self.assertEqual(result["tier"], "auto")

    def test_ref_not_on_origin_main_is_refused(self) -> None:
        # The hostile commit exists in the object store but origin/main never
        # published it, so its matcher must not be loaded or executed.
        hostile = self.fixture.install_commit_replacement()
        completed = self.fixture.invoke(["docs/guide/**"], ["docs/guide/page.md"], ref=hostile)
        self.assertEqual(completed.returncode, 2)
        self.assertIn("not reachable from refs/remotes/origin/main", completed.stderr)
        self.assertNotIn("replacement matcher executed", completed.stderr)

    def test_slashless_concrete_surface_is_root_anchored(self) -> None:
        result = self.success(["Cargo.lock"], ["Cargo.lock"])
        self.assertEqual(result["tier"], "judge")

        completed = self.fixture.invoke(["Cargo.lock"], [".agents/Cargo.lock"])
        self.assertEqual(completed.returncode, 2)
        self.assertIn("outside the declared surface", completed.stderr)


if __name__ == "__main__":
    unittest.main()
