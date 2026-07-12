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
  - glob: "crates/aether-capabilities/**"
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
            "crates/aether-capabilities/new/**": "judge",
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
