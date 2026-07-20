#!/usr/bin/env python3
"""Regression tests for the canonical declared-surface matcher."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("surface-match.py")
SPEC = importlib.util.spec_from_file_location("surface_match", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
surface_match = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(surface_match)


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
  - glob: "crates/aether-substrate-bundle/**"
    tier: judge
  - glob: "scripts/surface-match.py"
    tier: human
"""


class MatcherTests(unittest.TestCase):
    def setUp(self) -> None:
        self.policy = surface_match.load_policy_text(POLICY)

    def test_root_anchor(self) -> None:
        matcher = surface_match.compile_glob("/Cargo.toml")
        self.assertIsNotNone(matcher.match("Cargo.toml"))
        self.assertIsNone(matcher.match("nested/Cargo.toml"))

    def test_slashless_concrete_surface_is_root_anchored(self) -> None:
        matcher = surface_match.compile_surface_glob("Cargo.lock")
        self.assertIsNotNone(matcher.match("Cargo.lock"))
        self.assertIsNone(matcher.match(".agents/Cargo.lock"))

    def test_existing_matcher_semantics_remain(self) -> None:
        matcher = surface_match.compile_glob("docs/**/index?.md")
        self.assertIsNotNone(matcher.match("docs/index1.md"))
        self.assertIsNotNone(matcher.match("docs/a/b/indexz.md"))
        self.assertIsNone(matcher.match("docs/index.md"))
        self.assertIsNone(matcher.match("other/docs/index1.md"))

    def test_exact_paths(self) -> None:
        self.assertEqual(surface_match.tier_of_surface("docs/guide/page.md", self.policy), "auto")
        self.assertEqual(surface_match.tier_of_surface("Cargo.toml", self.policy), "human")
        self.assertEqual(surface_match.tier_of_surface("new-top/file.txt", self.policy), "judge")

    def test_exact_path_respects_directory_tail_semantics(self) -> None:
        self.assertEqual(surface_match.tier_of_surface("crates/aether-kit", self.policy), "human")

    def test_literal_subtrees_are_set_sound(self) -> None:
        cases = {
            "docs/guide/**": "auto",
            "crates/aether-kit/src/**": "auto",
            "crates/aether-kit/**": "human",
            "docs/**": "human",
            "crates/aether-substrate-bundle/new/**": "judge",
            "new-top/**": "judge",
        }
        for surface, expected in cases.items():
            with self.subTest(surface=surface):
                self.assertEqual(surface_match.tier_of_surface(surface, self.policy), expected)

    def test_complex_surface_wildcards_fail_closed(self) -> None:
        for surface in ["**", "docs/*", "crates/aether-*/future/**", "docs/[ag]uide/**"]:
            with self.subTest(surface=surface):
                self.assertEqual(surface_match.tier_of_surface(surface, self.policy), "human")

    def test_malformed_policy_fails_closed(self) -> None:
        malformed = [
            'default: judge\nrules:\n  - glob: "docs/**"\n    tier: owner\n',
            'default: judge\nrules:\n  - glob: "docs//**"\n    tier: auto\n',
            'default: judge\nrules:\n  - glob: "docs***"\n    tier: auto\n',
            'default: judge\nrules:\n  - glob: "../**"\n    tier: auto\n',
            'default: judge\nrules:\n  - glob: "docs/guide/**\n    tier: auto\n',
            'default: judge\nrules:\n  - glob: docs/guide/**"\n    tier: auto\n',
            'default: judge\nrules:\n- glob: docs/guide/**\ntier: auto\n',
            'default: judge\nrules:\n  - glob: docs/guide/**\n  tier: auto\n',
        ]
        for policy in malformed:
            with self.subTest(policy=policy):
                self.assertIsNone(surface_match.load_policy_text(policy))

    def test_matcher_implementation_is_human(self) -> None:
        self.assertEqual(
            surface_match.tier_of_surface("scripts/surface-match.py", self.policy),
            "human",
        )

    def test_surface_grammar(self) -> None:
        for pattern in ["Cargo.lock", "docs/guide/page.md", "docs/guide/**", "crates/aether-kit/src/**"]:
            with self.subTest(pattern=pattern):
                self.assertTrue(surface_match.valid_surface_glob(pattern))
        rejected = [
            "",
            "/Cargo.toml",
            "docs/",
            "docs//guide",
            "../escape",
            "docs/guide/../adr/x.md",
            "docs/*",
            "docs/**/x.md",
            "a/**/**",
            "**",
            "docs/[ag]uide/**",
            "docs/guide/page?.md",
            "docs\\guide",
        ]
        for pattern in rejected:
            with self.subTest(pattern=pattern):
                self.assertFalse(surface_match.valid_surface_glob(pattern))

    def test_repository_policy_file_parses_and_guards_itself(self) -> None:
        # Tripwire: the strict parser fails closed, so a malformed edit to the
        # real policy file would silently stop every auto dispatch — this is the
        # one place that failure is loud. The guarded paths pin the
        # constitutional carve-outs against an accidental policy edit.
        policy = surface_match.load_policy(str(SCRIPT.parent.parent / ".github" / "approval-policy.yml"))
        self.assertIsNotNone(policy)
        for guarded in [
            "scripts/surface-match.py",
            "scripts/test-surface-match.py",
            ".github/approval-policy.yml",
            ".github/workflows/ci.yml",
            ".agents/skills/approve/scripts/resolve_approval_tier.py",
            ".claude/hooks/check-no-divider-comments.sh",
        ]:
            with self.subTest(guarded=guarded):
                self.assertEqual(surface_match.tier_of(guarded, policy), "human")

    def test_out_of_grammar_surface_resolves_human_in_tier_mode(self) -> None:
        for surface in ["docs/guide/../adr/0001-x.md", "/docs/guide/page.md", "docs//guide/page.md"]:
            with self.subTest(surface=surface), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                surfaces = root / "surface.txt"
                policy = root / "policy.yml"
                surfaces.write_text(surface + "\n", encoding="utf-8")
                policy.write_text(POLICY, encoding="utf-8")
                completed = subprocess.run(
                    [sys.executable, "-I", str(SCRIPT), "--tier", str(surfaces), str(policy)],
                    check=False,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(completed.returncode, 0, completed.stderr)
                self.assertEqual(completed.stdout, "human\n")

    def test_containment_ignores_out_of_grammar_globs_without_backtracking(self) -> None:
        # Tripwire: the hostile glob compiles to nested unbounded regex groups;
        # without the grammar gate this match backtracks effectively forever.
        hostile = "**/" * 12 + "a"
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            globs = root / "globs.txt"
            paths = root / "paths.txt"
            policy = root / "policy.yml"
            globs.write_text(hostile + "\ndocs/guide/**\n", encoding="utf-8")
            paths.write_text("b/" * 40 + "c\ndocs/guide/page.md\n", encoding="utf-8")
            policy.write_text(POLICY, encoding="utf-8")
            completed = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(globs), str(paths), str(policy)],
                check=False,
                capture_output=True,
                text=True,
                timeout=30,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout, "b/" * 40 + "c\tjudge\n")
            self.assertIn("ignored out-of-grammar surface glob", completed.stderr)

    def test_most_restrictive_across_surface_file(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            surfaces = root / "surface.txt"
            policy = root / "policy.yml"
            surfaces.write_text("docs/guide/**\ncrates/aether-data/src/lib.rs\n", encoding="utf-8")
            policy.write_text(POLICY, encoding="utf-8")
            completed = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), "--tier", str(surfaces), str(policy)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(completed.stdout, "human\n")

    def test_cargo_lock_exempt_when_manifest_declared(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            policy = root / "policy.yml"
            policy.write_text(POLICY, encoding="utf-8")

            # Case A: an in-surface, changed manifest exempts root Cargo.lock
            # (and a nested x/Cargo.lock stays flagged — the exemption is
            # root-only).
            globs_a = root / "globs-a.txt"
            paths_a = root / "paths-a.txt"
            globs_a.write_text("crates/aether-data/Cargo.toml\ncrates/aether-data/**\n", encoding="utf-8")
            paths_a.write_text(
                "Cargo.lock\ncrates/aether-data/Cargo.toml\ncrates/aether-data/src/lib.rs\nx/Cargo.lock\n",
                encoding="utf-8",
            )
            completed_a = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(globs_a), str(paths_a), str(policy)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed_a.returncode, 0, completed_a.stderr)
            escaping_paths = [line.split("\t")[0] for line in completed_a.stdout.splitlines()]
            self.assertNotIn("Cargo.lock", escaping_paths)
            self.assertIn("x/Cargo.lock", escaping_paths)

            # Case B: no in-surface manifest change, root Cargo.lock still escapes.
            globs_b = root / "globs-b.txt"
            paths_b = root / "paths-b.txt"
            globs_b.write_text("docs/guide/**\n", encoding="utf-8")
            paths_b.write_text("Cargo.lock\n", encoding="utf-8")
            completed_b = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(globs_b), str(paths_b), str(policy)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed_b.returncode, 0, completed_b.stderr)
            self.assertTrue(completed_b.stdout.startswith("Cargo.lock\t"))

    def test_cargo_lock_exempt_under_wildcard_subtree_covering_changed_manifest(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            policy = root / "policy.yml"
            policy.write_text(POLICY, encoding="utf-8")

            # Positive: a `/**` subtree surface that covers a changed manifest
            # exempts root Cargo.lock — the #3490 shape (#3492).
            globs_pos = root / "globs-pos.txt"
            paths_pos = root / "paths-pos.txt"
            globs_pos.write_text("crates/aether-bloomery-host/**\n", encoding="utf-8")
            paths_pos.write_text(
                "crates/aether-bloomery-host/Cargo.toml\ncrates/aether-bloomery-host/src/lib.rs\nCargo.lock\n",
                encoding="utf-8",
            )
            completed_pos = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(globs_pos), str(paths_pos), str(policy)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed_pos.returncode, 0, completed_pos.stderr)
            self.assertNotIn("Cargo.lock", [line.split("\t")[0] for line in completed_pos.stdout.splitlines()])

            # Negative: the same `/**` subtree surface with no manifest change
            # in the diff — Cargo.lock still escapes.
            globs_neg = root / "globs-neg.txt"
            paths_neg = root / "paths-neg.txt"
            globs_neg.write_text("crates/aether-bloomery-host/**\n", encoding="utf-8")
            paths_neg.write_text("Cargo.lock\n", encoding="utf-8")
            completed_neg = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(globs_neg), str(paths_neg), str(policy)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed_neg.returncode, 0, completed_neg.stderr)
            self.assertTrue(completed_neg.stdout.startswith("Cargo.lock\t"))

    def test_containment_cli_and_root_anchor(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            globs = root / "globs.txt"
            paths = root / "paths.txt"
            policy = root / "policy.yml"
            globs.write_text("Cargo.toml\nCargo.lock\ndocs/guide/**\n", encoding="utf-8")
            paths.write_text(
                "Cargo.toml\nnested/Cargo.toml\nCargo.lock\n.agents/Cargo.lock\ndocs/guide/page.md\n",
                encoding="utf-8",
            )
            policy.write_text(POLICY, encoding="utf-8")
            completed = subprocess.run(
                [sys.executable, "-I", str(SCRIPT), str(globs), str(paths), str(policy)],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            self.assertEqual(
                completed.stdout,
                "nested/Cargo.toml\tjudge\n.agents/Cargo.lock\thuman\n",
            )


if __name__ == "__main__":
    unittest.main()
