#!/usr/bin/env python3
"""Regression tests for check-reference-mint.py."""

from __future__ import annotations

import contextlib
import importlib.util
import io
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path


SCRIPT = Path(__file__).with_name("check-reference-mint.py")
SPEC = importlib.util.spec_from_file_location("check_reference_mint", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
scanner = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = scanner
SPEC.loader.exec_module(scanner)


class Repository:
    def __init__(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.root = Path(self.temp.name)
        self.git("init", "-q", "-b", "main")
        self.git("config", "user.name", "Reference Mint Test")
        self.git("config", "user.email", "reference-mint@example.invalid")

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

    def commit(self, message: str) -> None:
        self.git("add", "-A")
        self.git("commit", "-q", "-m", message)

    def scan(self) -> list[str]:
        return scanner.scan(self.root)

    def run(self) -> tuple[int, list[str]]:
        buffer = io.StringIO()
        with contextlib.redirect_stdout(buffer):
            status = scanner.main(["--root", str(self.root)])
        return status, buffer.getvalue().splitlines()


class ScannerTests(unittest.TestCase):
    def setUp(self) -> None:
        self.repo = Repository()

    def tearDown(self) -> None:
        self.repo.close()

    def test_allowed_paths_pass(self) -> None:
        for path in scanner.ALLOWED_PATHS:
            self.repo.write(path, "pub const fn __mint_actor_ref<R>(id: MailboxId) -> ActorRef<R>;\n")
        self.repo.commit("allowed mints")

        self.assertEqual(self.repo.scan(), [])
        status, output = self.repo.run()
        self.assertEqual(status, 0)
        self.assertEqual(output, [])

    def test_disallowed_path_fails_with_path_and_line(self) -> None:
        self.repo.write(
            "crates/aether-substrate/src/sneaky.rs",
            "fn sneak() {\n    let reference = __mint_actor_ref(id);\n}\n",
        )
        self.repo.commit("sneaky mint")

        findings = self.repo.scan()

        self.assertEqual(len(findings), 1)
        self.assertTrue(
            findings[0].startswith("crates/aether-substrate/src/sneaky.rs:2: "),
            findings[0],
        )
        self.assertIn("__mint_actor_ref", findings[0])
        status, _ = self.repo.run()
        self.assertEqual(status, 1)

    def test_shared_blob_mint_outside_the_allowlist_fails(self) -> None:
        self.repo.write(
            "crates/aether-substrate/src/store/mod.rs",
            "fn sneak(entry: Arc<BlobEntry>) -> Blob {\n    aether_data::__mint_shared_blob(entry)\n}\n",
        )
        self.repo.commit("sneaky blob mint")

        findings = self.repo.scan()

        self.assertEqual(len(findings), 1)
        self.assertTrue(
            findings[0].startswith("crates/aether-substrate/src/store/mod.rs:2: "),
            findings[0],
        )
        status, _ = self.repo.run()
        self.assertEqual(status, 1)

    def test_attribute_and_suppression_comment_do_not_relax(self) -> None:
        self.repo.write(
            "crates/aether-substrate/src/sneaky.rs",
            "#[allow(clippy::disallowed_methods)]\n"
            "fn sneak() {\n"
            "    let reference = __mint_erased_actor_ref(id); // aether-suppression-request: testing the gate\n"
            "}\n",
        )
        self.repo.commit("sneaky mint with escapes")

        findings = self.repo.scan()

        self.assertEqual(len(findings), 1)
        self.assertTrue(
            findings[0].startswith("crates/aether-substrate/src/sneaky.rs:3: "),
            findings[0],
        )
        status, _ = self.repo.run()
        self.assertEqual(status, 1)


if __name__ == "__main__":
    unittest.main()
