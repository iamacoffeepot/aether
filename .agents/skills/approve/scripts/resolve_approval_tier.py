#!/usr/bin/env python3
"""Validate a Plan surface and resolve its tier from one captured Git commit.

Matcher and policy semantics are loaded from scripts/surface-match.py at the
captured ref. This wrapper adds Codex's Plan-to-Ready validation and evidence;
it deliberately does not carry a second glob implementation.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from typing import Any, Sequence


CANONICAL_PATH = "scripts/surface-match.py"
POLICY_PATH = ".github/approval-policy.yml"
FULL_SHA = re.compile(r"(?:[0-9a-fA-F]{40}|[0-9a-fA-F]{64})\Z")
SAFE_SURFACE = re.compile(r"[A-Za-z0-9._/*\-]+\Z")
SAFE_TARGET = re.compile(r"[A-Za-z0-9._/\-]+\Z")


class ResolverError(RuntimeError):
    """A deterministic, user-actionable resolver failure."""


def _run_git(repo: Path, arguments: Sequence[str], *, operation: str) -> bytes:
    # A local replacement ref, alternate object directory, or injected Git
    # config must not substitute a different tree for the captured origin/main
    # object we are about to execute. These reads need no Git-specific ambient
    # state, so strip it all and disable replacement objects redundantly in both
    # the environment and argv.
    environment = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("GIT_")
    }
    environment["GIT_NO_REPLACE_OBJECTS"] = "1"
    try:
        completed = subprocess.run(
            ["git", "--no-replace-objects", "-C", str(repo), *arguments],
            check=False,
            env=environment,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
    except OSError as error:
        raise ResolverError(f"cannot run git while {operation}: {error}") from error
    if completed.returncode:
        detail = completed.stderr.decode("utf-8", errors="replace").strip()
        raise ResolverError(
            f"git failed while {operation} (exit {completed.returncode}): "
            f"{detail or 'no diagnostic'}"
        )
    return completed.stdout


def _validate_surface(pattern: str, *, context: str) -> None:
    if not pattern:
        raise ResolverError(f"{context}: surface entry is empty")
    if not pattern.isascii() or SAFE_SURFACE.fullmatch(pattern) is None:
        raise ResolverError(f"{context}: surface contains unsupported characters: {pattern!r}")
    if pattern.startswith(("/", "-", "!", "#")) or pattern.endswith("/"):
        raise ResolverError(f"{context}: surface must be a positive repository-relative pattern: {pattern!r}")
    if "//" in pattern:
        raise ResolverError(f"{context}: surface contains an empty path segment: {pattern!r}")
    if any(segment in {"", ".", ".."} for segment in pattern.split("/")):
        raise ResolverError(f"{context}: surface contains an unsafe path segment: {pattern!r}")
    if pattern in {"**", "crates/**"}:
        raise ResolverError(f"{context}: escape-hatch surface is not allowed: {pattern!r}")

    if "*" not in pattern:
        return
    if pattern.endswith("/**") and pattern.count("*") == 2:
        return
    raise ResolverError(
        f"{context}: surface must be a concrete path or a literal directory prefix "
        f"followed by '/**': {pattern!r}"
    )


def _validate_target(path: str, *, context: str) -> None:
    if not path:
        raise ResolverError(f"{context}: target is empty")
    if not path.isascii() or SAFE_TARGET.fullmatch(path) is None:
        raise ResolverError(
            f"{context}: target must contain only ASCII letters, digits, '.', '_', '/', and '-': {path!r}"
        )
    if path.startswith(("/", "-")) or path.endswith("/"):
        raise ResolverError(f"{context}: target must be a concrete repository-relative path: {path!r}")
    segments = path.split("/")
    if any(segment in {"", ".", ".."} for segment in segments):
        raise ResolverError(f"{context}: target contains an unsafe path segment: {path!r}")
    if segments[0] == ".git":
        raise ResolverError(f"{context}: target cannot name Git's private directory: {path!r}")


def _read_list(path: Path, *, kind: str) -> list[str]:
    try:
        lines = path.read_text(encoding="utf-8").splitlines()
    except (OSError, UnicodeError) as error:
        raise ResolverError(f"cannot read {kind} file {path}: {error}") from error

    entries: list[str] = []
    seen: set[str] = set()
    for line_number, line in enumerate(lines, start=1):
        entry = line.strip()
        if not entry:
            continue
        context = f"{path}:{line_number}"
        if entry.startswith("#"):
            raise ResolverError(f"{context}: comments are not allowed in {kind} files")
        if kind == "surface":
            _validate_surface(entry, context=context)
        else:
            _validate_target(entry, context=context)
        if entry in seen:
            raise ResolverError(f"{context}: duplicate {kind} entry: {entry!r}")
        seen.add(entry)
        entries.append(entry)

    label = "surface patterns" if kind == "surface" else "concrete paths"
    if not entries:
        raise ResolverError(f"{kind} file {path} contains no {label}")
    return entries


def _load_snapshot(repo_argument: str, ref_argument: str) -> tuple[str, dict[str, Any], Any, list[str]]:
    try:
        repo = Path(repo_argument).expanduser().resolve(strict=True)
    except OSError as error:
        raise ResolverError(f"repository path does not exist: {repo_argument!r}: {error}") from error
    if not repo.is_dir():
        raise ResolverError(f"repository path is not a directory: {repo}")
    if FULL_SHA.fullmatch(ref_argument) is None:
        raise ResolverError("--ref must be a full 40- or 64-character hexadecimal commit SHA")

    ref = ref_argument.lower()
    if _run_git(repo, ["rev-parse", "--is-inside-work-tree"], operation="validating the repository").strip() != b"true":
        raise ResolverError(f"path is not a Git worktree: {repo}")
    resolved = _run_git(
        repo,
        ["rev-parse", "--verify", "--end-of-options", f"{ref}^{{commit}}"],
        operation=f"validating commit {ref}",
    ).decode("ascii", errors="strict").strip().lower()
    if resolved != ref:
        raise ResolverError(f"--ref must identify the commit object itself (resolved {ref} to {resolved})")

    canonical_bytes = _run_git(
        repo,
        ["cat-file", "blob", f"{ref}:{CANONICAL_PATH}"],
        operation=f"reading {CANONICAL_PATH} at {ref}",
    )
    policy_bytes = _run_git(
        repo,
        ["cat-file", "blob", f"{ref}:{POLICY_PATH}"],
        operation=f"reading {POLICY_PATH} at {ref}",
    )
    try:
        canonical_source = canonical_bytes.decode("utf-8")
        policy_text = policy_bytes.decode("utf-8")
    except UnicodeError as error:
        raise ResolverError(f"canonical matcher or policy at {ref} is not UTF-8") from error

    namespace: dict[str, Any] = {
        "__name__": "aether_captured_surface_match",
        "__file__": f"{ref}:{CANONICAL_PATH}",
    }
    try:
        exec(compile(canonical_source, namespace["__file__"], "exec"), namespace)
    except Exception as error:
        raise ResolverError(f"cannot load canonical matcher at {ref}: {error}") from error

    required = {
        "compile_glob",
        "compile_surface_glob",
        "load_policy_text",
        "tier_of",
        "tier_of_surface",
        "rule_intersects_subtree",
        "rule_covers_subtree",
    }
    missing = sorted(name for name in required if not callable(namespace.get(name)))
    rank = namespace.get("RANK")
    if missing or rank != {"auto": 0, "judge": 1, "human": 2}:
        raise ResolverError(
            f"canonical matcher at {ref} lacks the required resolver API"
            + (f": {', '.join(missing)}" if missing else "")
        )

    policy = namespace["load_policy_text"](policy_text)
    if policy is None:
        raise ResolverError(f"{POLICY_PATH} at {ref} is empty or malformed")

    tree = _run_git(
        repo,
        ["ls-tree", "-r", "-z", "--name-only", ref],
        operation=f"listing tracked paths at {ref}",
    )
    try:
        tracked = sorted(entry.decode("utf-8") for entry in tree.split(b"\0") if entry)
    except UnicodeError as error:
        raise ResolverError(f"the tree at {ref} contains a non-UTF-8 path") from error
    if not tracked or len(tracked) != len(set(tracked)):
        raise ResolverError(f"the tree at {ref} is empty or contains duplicate paths")

    return ref, namespace, policy, tracked


def resolve(repo: str, ref: str, surface_file: str, targets_file: str) -> dict[str, object]:
    resolved_ref, canonical, policy, tracked = _load_snapshot(repo, ref)
    surfaces = _read_list(Path(surface_file), kind="surface")
    targets = _read_list(Path(targets_file), kind="target")
    universe = sorted(set(tracked).union(targets))
    target_set = set(targets)
    matchers = [(surface, canonical["compile_surface_glob"](surface)) for surface in surfaces]

    for surface, _ in matchers:
        if "*" not in surface and surface not in universe:
            raise ResolverError(
                f"concrete surface path is not tracked or planned at {resolved_ref}: {surface!r}; "
                "use a literal directory prefix ending in '/**' for a subtree"
            )

    uncovered = [path for path in targets if not any(matcher.match(path) for _, matcher in matchers)]
    if uncovered:
        raise ResolverError("targets are outside the declared surface: " + ", ".join(map(repr, uncovered)))

    default, rules = policy
    surface_results: list[dict[str, object]] = []
    allowed_paths: set[str] = set()
    for surface, matcher in matchers:
        matched_paths = [path for path in universe if matcher.match(path)]
        if not matched_paths:
            raise ResolverError(
                f"surface pattern matches no tracked or target path at {resolved_ref}: {surface!r}"
            )
        allowed_paths.update(matched_paths)
        tier = canonical["tier_of_surface"](surface, policy)

        prefix = surface if "*" not in surface else surface[:-3].rstrip("/")
        intersecting = [
            {"glob": glob, "tier": rule_tier}
            for glob, _, rule_tier in rules
            if canonical["rule_intersects_subtree"](glob, prefix)
        ]
        default_applies = not any(
            canonical["rule_covers_subtree"](glob, prefix) for glob, _, _ in rules
        )

        surface_results.append(
            {
                "default_applies": default_applies,
                "glob": surface,
                "matched_path_count": len(matched_paths),
                "matched_policy_rules": sorted(intersecting, key=lambda item: (item["glob"], item["tier"])),
                "matched_targets": sorted(path for path in matched_paths if path in target_set),
                "tier": tier,
            }
        )

    target_results = []
    for path in sorted(targets):
        matches = [
            {"glob": glob, "tier": rule_tier}
            for glob, matcher, rule_tier in rules
            if matcher.match(path)
        ]
        target_results.append(
            {
                "matched_policy_rules": sorted(matches, key=lambda item: (item["glob"], item["tier"])),
                "path": path,
                "surfaces": sorted(surface for surface, matcher in matchers if matcher.match(path)),
                "tier": canonical["tier_of"](path, policy),
            }
        )

    rank = canonical["RANK"]
    overall = max((entry["tier"] for entry in surface_results), key=lambda tier: rank.get(tier, len(rank)))
    return {
        "default": default,
        "ref": resolved_ref,
        "surface_path_count": len(allowed_paths),
        "surfaces": surface_results,
        "targets": target_results,
        "tier": overall,
        "tracked_path_count": len(tracked),
    }


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="path to a Git worktree")
    parser.add_argument("--ref", required=True, help="full hexadecimal commit SHA to inspect")
    parser.add_argument("--surface-file", required=True, help="newline-delimited declared surface patterns")
    parser.add_argument("--targets-file", required=True, help="newline-delimited concrete repository targets")
    return parser


def main(argv: Sequence[str] | None = None) -> int:
    arguments = _parser().parse_args(argv)
    try:
        result = resolve(arguments.repo, arguments.ref, arguments.surface_file, arguments.targets_file)
    except ResolverError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2
    print(json.dumps(result, indent=2, sort_keys=True, ensure_ascii=True))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
