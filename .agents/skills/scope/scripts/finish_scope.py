#!/usr/bin/env python3
"""Splice a drafted Plan into a fresh issue body, validate it, and optionally write it.

Folds the scope skill's mechanical finishing tail — splice, digest, title,
depends-on, host-path, target, and surface checks, staging, and the
file-backed ``PATCH`` — into one script call. Every check runs and every
failure is reported in one JSON report; the skill keys off ``outcome``, not
exit numerals.

This script adds no parser of its own: managed-span bounds, the digest, and
routing come from ``plan_digest.py``, and surface/target grounding comes from
``resolve_approval_tier.py``, both loaded from ``approve/scripts`` beside this
skill. Git runs through ``resolve_approval_tier._run_git``, which strips
``GIT_*`` from the environment.
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import re
import subprocess
import sys
import tempfile
from pathlib import Path
from types import ModuleType
from typing import Any, Sequence


REPOSITORY = "iamacoffeepot/aether"

EXIT_OK = 0
EXIT_INVALID = 1
EXIT_ERROR = 2
EXIT_ABORT = 3

_EXIT_BY_OUTCOME = {
    "validated": EXIT_OK,
    "written": EXIT_OK,
    "invalid": EXIT_INVALID,
    "error": EXIT_ERROR,
    "aborted": EXIT_ABORT,
}

STOPLIST = frozenset(
    {
        "about",
        "after",
        "every",
        "their",
        "there",
        "these",
        "those",
        "which",
        "while",
        "where",
        "without",
        "through",
    }
)

HOST_ROOTS = ("Users", "home", "mnt", "private")

DEPENDS_LINE = re.compile(r"^- #\d+ — \S")
CODE_SPAN = re.compile(r"``([^`]+)``|`([^`]+)`")
SURFACE_SENTINEL = "N/A — pure umbrella; no implementation PR"
FENCE = re.compile(r"\A```\r?\n(.*?)\r?\n```\Z", re.DOTALL)


class ScopeFinishError(RuntimeError):
    """A hard failure that halts finishing before a proposed body exists."""


class GitHubError(RuntimeError):
    """A ``gh`` invocation failed."""


def _load_helper(name: str) -> ModuleType:
    approve_scripts = Path(__file__).resolve().parents[2] / "approve" / "scripts"
    spec = importlib.util.spec_from_file_location(name, approve_scripts / f"{name}.py")
    if spec is None or spec.loader is None:
        raise ScopeFinishError(f"cannot load helper module {name!r} from {approve_scripts}")
    module = importlib.util.module_from_spec(spec)
    # dataclasses resolves annotations through sys.modules[cls.__module__], so
    # plan_digest's frozen PlanDigest needs this registered before exec runs.
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


plan_digest = _load_helper("plan_digest")
resolve_approval_tier = _load_helper("resolve_approval_tier")


class GitHubClient:
    """The only surface that shells out to ``gh api``."""

    def read_issue(self, issue: int) -> dict[str, Any]:
        completed = subprocess.run(
            ["gh", "api", f"repos/{REPOSITORY}/issues/{issue}", "--jq", "{number,title,body,state}"],
            check=False,
            capture_output=True,
            text=True,
        )
        if completed.returncode:
            raise GitHubError(completed.stderr.strip() or f"gh api exited {completed.returncode}")
        return json.loads(completed.stdout)

    def open_pull_head_refs(self) -> list[str]:
        completed = subprocess.run(
            [
                "gh",
                "api",
                "--paginate",
                f"repos/{REPOSITORY}/pulls?state=open&per_page=100",
                "--jq",
                ".[].head.ref",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        if completed.returncode:
            raise GitHubError(completed.stderr.strip() or f"gh api exited {completed.returncode}")
        return [line for line in completed.stdout.splitlines() if line]

    def patch_body(self, issue: int, request_file: Path) -> str:
        completed = subprocess.run(
            [
                "gh",
                "api",
                "-X",
                "PATCH",
                f"repos/{REPOSITORY}/issues/{issue}",
                "--input",
                str(request_file),
                "--jq",
                ".updated_at",
            ],
            check=False,
            capture_output=True,
            text=True,
        )
        if completed.returncode:
            raise GitHubError(completed.stderr.strip() or f"gh api exited {completed.returncode}")
        return completed.stdout.strip()


def _short_ref(refname: str) -> str:
    for prefix in ("refs/heads/", "refs/remotes/origin/"):
        if refname.startswith(prefix):
            return refname[len(prefix) :]
    return refname


def _local_refs(repo: Path) -> list[str]:
    output = resolve_approval_tier._run_git(
        repo,
        ["for-each-ref", "--format=%(refname)", "refs/heads", "refs/remotes/origin"],
        operation="enumerating local and remote-tracking refs",
    )
    return [line for line in output.decode("utf-8", errors="replace").splitlines() if line]


def _git_common_dir_root(repo: Path) -> Path:
    output = resolve_approval_tier._run_git(
        repo,
        ["rev-parse", "--path-format=absolute", "--git-common-dir"],
        operation="resolving the git common directory",
    )
    return Path(output.decode("utf-8", errors="replace").strip()).parent


def _tracked_top_level(repo: Path, base: str) -> set[str]:
    output = resolve_approval_tier._run_git(
        repo, ["ls-tree", "--name-only", base], operation=f"listing top-level tracked entries at {base}"
    )
    return {line for line in output.decode("utf-8", errors="replace").splitlines() if line}


def _tracked_paths(repo: Path, base: str) -> set[str]:
    output = resolve_approval_tier._run_git(
        repo, ["ls-tree", "-r", "--name-only", base], operation=f"listing tracked paths at {base}"
    )
    return {line for line in output.decode("utf-8", errors="replace").splitlines() if line}


def _implementation_artifacts(repo: Path, issue: int, github: GitHubClient) -> list[str]:
    pattern = re.compile(rf"(^|/)issue-{issue}(-|$)")
    reasons: list[str] = []

    worktree_dir = _git_common_dir_root(repo) / ".agents" / "worktrees" / f"issue-{issue}"
    if worktree_dir.is_dir():
        reasons.append(f"implementation worktree exists: {worktree_dir}")

    local_matches = sorted({_short_ref(ref) for ref in _local_refs(repo) if pattern.search(_short_ref(ref))})
    if local_matches:
        reasons.append("implementation ref exists: " + ", ".join(local_matches))

    pull_matches = sorted({ref for ref in github.open_pull_head_refs() if pattern.search(ref)})
    if pull_matches:
        reasons.append("open pull request head ref exists: " + ", ".join(pull_matches))

    return reasons


def _parse_sections_file(text: str) -> dict[str, str]:
    # A physical marker line, not merely prose that names or quotes the
    # marker syntax (this very Plan's own Design notes among it).
    if any(line.startswith("<!-- aether-approval") for line in text.splitlines()):
        raise ScopeFinishError("sections file must not contain an aether-approval record")

    all_headings = [match.group(1) for match in plan_digest.H2.finditer(text)]
    unmanaged = [name for name in all_headings if name not in plan_digest.MANAGED_ORDER]
    if unmanaged:
        raise ScopeFinishError(f"sections file contains a non-managed heading: ## {unmanaged[0]}")

    try:
        bounds = plan_digest.managed_span_bounds(text)
    except plan_digest.PlanDigestError as error:
        raise ScopeFinishError(f"sections file is malformed: {error}") from error

    expected_order = [name for name in plan_digest.MANAGED_ORDER if name in bounds]
    if all_headings != expected_order:
        raise ScopeFinishError("sections file headings are not in scope-owned order")

    return {name: text[start:end] for name, (start, end) in bounds.items()}


def _format_section(text: str, *, has_more: bool) -> str:
    stripped = text.rstrip()
    return stripped + ("\n\n" if has_more else "\n")


def _pad_to_blank_line(tail: str) -> str:
    """The layout bytes still missing to open a blank line before the next H2.

    Adds only what is missing so a junction against preserved unmanaged bytes
    (which may end with no newline at all, one, or already a blank line)
    never alters those bytes, only the new characters inserted after them.
    """

    if tail == "" or tail.endswith("\n\n") or tail.endswith("\r\n\r\n"):
        return ""
    if tail.endswith("\n") or tail.endswith("\r"):
        return "\n"
    return "\n\n"


def _assert_unmanaged_preserved(proposed_body: str, segments: Sequence[str]) -> None:
    if not proposed_body.startswith(segments[0]):
        raise ScopeFinishError("splice assertion failed: the prefix before the first managed heading changed")

    cursor = 0
    for segment in segments:
        if not segment:
            continue
        index = proposed_body.find(segment, cursor)
        if index == -1:
            raise ScopeFinishError("splice assertion failed: an unmanaged segment is missing or reordered")
        cursor = index + len(segment)


def _splice(
    fresh_body: str, fresh_bounds: dict[str, tuple[int, int]], sections: dict[str, str]
) -> tuple[str, list[str]]:
    rank = {name: index for index, name in enumerate(plan_digest.MANAGED_ORDER)}
    existing_sorted = sorted(fresh_bounds.items(), key=lambda item: item[1][0])
    dropped = sorted(name for name in fresh_bounds if name not in sections)
    final_order = [name for name in plan_digest.MANAGED_ORDER if name in sections]

    insertions_by_slot: dict[int, list[str]] = {}
    for name in final_order:
        if name in fresh_bounds:
            continue
        target_rank = rank[name]
        slot = len(existing_sorted)
        for index, (existing_name, _) in enumerate(existing_sorted):
            if rank[existing_name] > target_rank:
                slot = index
                break
        insertions_by_slot.setdefault(slot, []).append(name)

    flat: list[tuple[str, str]] = []
    for slot in range(len(existing_sorted) + 1):
        for name in insertions_by_slot.get(slot, []):
            flat.append((name, sections[name]))
        if slot < len(existing_sorted):
            existing_name = existing_sorted[slot][0]
            if existing_name in sections:
                flat.append((existing_name, sections[existing_name]))

    formatted_by_name = {
        name: _format_section(text, has_more=index + 1 < len(flat)) for index, (name, text) in enumerate(flat)
    }

    ends = [0] + [end for _, (_, end) in existing_sorted]
    starts = [start for _, (start, _) in existing_sorted] + [len(fresh_body)]
    segments = [fresh_body[ends[index] : starts[index]] for index in range(len(existing_sorted) + 1)]

    output = ""
    for slot in range(len(existing_sorted) + 1):
        # A segment after a managed span always opens with an unmanaged H2,
        # which needs the same blank line a managed heading gets.
        if segments[slot]:
            output += _pad_to_blank_line(output)
        output += segments[slot]
        for name in insertions_by_slot.get(slot, []):
            output += _pad_to_blank_line(output)
            output += formatted_by_name[name]
        if slot < len(existing_sorted):
            existing_name = existing_sorted[slot][0]
            if existing_name in formatted_by_name:
                output += _pad_to_blank_line(output)
                output += formatted_by_name[existing_name]

    _assert_unmanaged_preserved(output, segments)
    return output, dropped


def _title_candidates(title: str) -> list[str]:
    before, sep, after = title.partition(": ")
    text = after if sep else title
    words = re.findall(r"[A-Za-z]+", text)
    return [word for word in words if word.islower() and len(word) >= 5 and word not in STOPLIST]


def _check_depends_on(text: str) -> list[str]:
    if not text:
        return []
    heading = plan_digest.H2.match(text)
    body = text[heading.end() :] if heading else text
    return [
        f"Depends on line does not match the required format: {line!r}"
        for line in body.splitlines()
        if line.strip() and not DEPENDS_LINE.match(line)
    ]


def _check_host_paths(sections: dict[str, str]) -> list[str]:
    markers = tuple("/" + root + "/" for root in HOST_ROOTS)
    return [
        f"managed section {name!r} contains an absolute host path marker {marker!r}"
        for name, text in sections.items()
        for marker in markers
        if marker in text
    ]


def _extract_code_spans(text: str) -> list[str]:
    return [match.group(1) if match.group(1) is not None else match.group(2) for match in CODE_SPAN.finditer(text)]


def _check_targets(repo: Path, base: str, sections: dict[str, str]) -> tuple[set[str], set[str], set[str], list[str]]:
    raw_spans = _extract_code_spans(sections.get("Design notes", "")) + _extract_code_spans(
        sections.get("Implementation plan", "")
    )

    parsed: list[tuple[str, bool]] = []
    read_candidates: list[str] = []
    for span in raw_spans:
        candidate = span[: -len(" (create)")].strip() if span.endswith(" (create)") else ""
        read_candidate = span[: -len(" (read)")].strip() if span.endswith(" (read)") else ""
        if candidate:
            # A real creation citation, not prose quoting the bare " (create)"
            # marker itself (this Plan's own step 2 does exactly that).
            parsed.append((candidate.rstrip("/"), True))
        elif read_candidate:
            # A reference citation: it must exist at the base but is not a
            # target, so it never reaches the surface check.
            read_candidates.append(read_candidate.rstrip("/"))
        else:
            parsed.append((span.split(":", 1)[0].rstrip("/"), False))

    creation_paths = {path for path, is_creation in parsed if is_creation}

    try:
        tracked_top_level = _tracked_top_level(repo, base)
        tracked_paths = _tracked_paths(repo, base)
    except resolve_approval_tier.ResolverError as error:
        raise ScopeFinishError(f"cannot list tracked paths at {base}: {error}") from error

    def _kept(path: str) -> bool:
        if not path or "*" in path or resolve_approval_tier.SAFE_TARGET.fullmatch(path) is None:
            return False
        return path.split("/", 1)[0] in tracked_top_level

    existing: set[str] = set()
    create: set[str] = set()
    read: set[str] = set()
    failures: list[str] = []

    for path in read_candidates:
        if not _kept(path):
            failures.append(f"read path is not a safe repository-relative path: {path!r}")
        elif path in tracked_paths or any(tracked.startswith(path + "/") for tracked in tracked_paths):
            read.add(path)
        else:
            failures.append(f"read path absent at base: {path!r}")

    seen: set[str] = set()
    unique_paths = [path for path, _ in parsed if not (path in seen or seen.add(path))]

    for path in unique_paths:
        is_creation = path in creation_paths
        kept = _kept(path)

        if is_creation:
            # The author explicitly declared this a path, so it is scrutinized
            # even when it is not "kept" as a plausible bare path citation.
            if not kept:
                failures.append(f"creation path is not a safe repository-relative path: {path!r}")
            elif path in tracked_paths:
                failures.append(f"creation already exists at base: {path!r}")
            else:
                create.add(path)
            continue

        if not kept:
            # A code span that never looked like a repository path — a type
            # signature, a git ref, a JSON field name — is not a citation.
            continue

        if path in tracked_paths:
            existing.add(path)
        elif any(tracked.startswith(path + "/") for tracked in tracked_paths) or any(
            other.startswith(path + "/") for other in creation_paths
        ):
            continue
        else:
            failures.append(f"cited path absent at base: {path!r}")

    return existing, create, read, failures


def _check_surface(
    repo: Path, base: str, sections: dict[str, str], targets: set[str], stage_dir: Path
) -> tuple[dict[str, Any] | None, list[str]]:
    surface_text = sections.get("Declared surface", "")
    heading = plan_digest.H2.match(surface_text)
    body = surface_text[heading.end() :] if heading else surface_text
    stripped = body.strip()

    if stripped == SURFACE_SENTINEL:
        subissues = sections.get("Sub-issues", "")
        subissues_heading = plan_digest.H2.match(subissues)
        subissues_body = subissues[subissues_heading.end() :] if subissues_heading else subissues
        if not subissues_body.strip():
            return None, ["pure-umbrella Declared surface requires a non-empty Sub-issues section"]
        return {"pure_umbrella": True}, []

    match = FENCE.fullmatch(stripped)
    if match is None:
        return None, ["Declared surface must be exactly one fenced block or the pure-umbrella sentinel"]
    lines = match.group(1).splitlines()
    if not lines or any(not line.strip() for line in lines):
        return None, ["Declared surface fenced block must contain only non-empty lines"]

    if not targets:
        return None, ["Plan has no targets"]

    surface_file = stage_dir / "surface.txt"
    targets_file = stage_dir / "targets.txt"
    surface_file.write_text("\n".join(lines) + "\n", encoding="utf-8")
    targets_file.write_text("\n".join(sorted(targets)) + "\n", encoding="utf-8")

    try:
        resolution = resolve_approval_tier.resolve(str(repo), base, str(surface_file), str(targets_file))
    except resolve_approval_tier.ResolverError as error:
        return None, [f"surface resolution failed: {error}"]
    return {"tier": resolution["tier"], "surface_path_count": resolution["surface_path_count"]}, []


def _empty_report(issue: int, base: str) -> dict[str, Any]:
    return {
        "issue": issue,
        "base": base,
        "outcome": "error",
        "failures": [],
        "digest": None,
        "surface": None,
        "targets": {"existing": [], "create": [], "read": []},
        "dropped_sections": [],
        "unmanaged_refreshed": False,
        "staged_body": None,
        "staged_request": None,
        "patch_command": None,
        "written": False,
        "updated_at": None,
    }


def _finish(
    *,
    repo: Path,
    base: str,
    issue: int,
    snapshot: dict[str, Any],
    sections_text: str,
    stage_dir: Path,
    write: bool,
    github: GitHubClient,
) -> dict[str, Any]:
    report = _empty_report(issue, base)
    sections = _parse_sections_file(sections_text)

    try:
        fresh_issue = github.read_issue(issue)
    except GitHubError as error:
        raise ScopeFinishError(f"cannot re-read issue {issue}: {error}") from error

    fresh_body = fresh_issue.get("body") or ""
    snapshot_body = snapshot.get("body") or ""

    abort_reasons: list[str] = []
    if fresh_issue.get("number") != snapshot.get("number"):
        abort_reasons.append(
            f"issue number changed: snapshot {snapshot.get('number')!r}, current {fresh_issue.get('number')!r}"
        )
    if fresh_issue.get("title") != snapshot.get("title"):
        abort_reasons.append("issue title changed since the snapshot")
    if fresh_issue.get("state") != "open":
        abort_reasons.append(f"issue state is {fresh_issue.get('state')!r}, not open")

    try:
        snapshot_bounds = plan_digest.managed_span_bounds(snapshot_body)
        fresh_bounds = plan_digest.managed_span_bounds(fresh_body)
    except plan_digest.PlanDigestError as error:
        raise ScopeFinishError(f"cannot bound managed spans: {error}") from error

    changed_spans = []
    for name in sorted(set(snapshot_bounds) | set(fresh_bounds)):
        if (name in snapshot_bounds) != (name in fresh_bounds):
            changed_spans.append(name)
            continue
        snap_start, snap_end = snapshot_bounds[name]
        fresh_start, fresh_end = fresh_bounds[name]
        if snapshot_body[snap_start:snap_end] != fresh_body[fresh_start:fresh_end]:
            changed_spans.append(name)
    if changed_spans:
        abort_reasons.append("managed sections changed concurrently: " + ", ".join(changed_spans))

    try:
        abort_reasons.extend(_implementation_artifacts(repo, issue, github))
    except (resolve_approval_tier.ResolverError, GitHubError) as error:
        raise ScopeFinishError(f"cannot check implementation artifacts: {error}") from error

    proposed_body, dropped_sections = _splice(fresh_body, fresh_bounds, sections)
    report["dropped_sections"] = dropped_sections
    report["unmanaged_refreshed"] = True

    failures: list[str] = []
    digest = None
    try:
        digest = plan_digest.digest_body(proposed_body)
        report["digest"] = digest.as_dict()
    except plan_digest.PlanDigestError as error:
        failures.append(f"plan digest: {error}")

    title_candidates = _title_candidates(fresh_issue.get("title") or "")
    problem_text = sections.get("Problem statement", "")
    if title_candidates and not any(
        re.search(rf"\b{re.escape(word)}\b", problem_text, re.IGNORECASE) for word in title_candidates
    ):
        failures.append("no distinctive title word appears in the new Problem statement")

    failures.extend(_check_depends_on(sections.get("Depends on", "")))
    failures.extend(_check_host_paths(sections))

    existing_targets, create_targets, read_targets, target_failures = _check_targets(repo, base, sections)
    failures.extend(target_failures)
    report["targets"] = {
        "existing": sorted(existing_targets),
        "create": sorted(create_targets),
        "read": sorted(read_targets),
    }

    surface_result, surface_failures = _check_surface(
        repo, base, sections, existing_targets | create_targets, stage_dir
    )
    failures.extend(surface_failures)
    report["surface"] = surface_result

    body_path = stage_dir / "body.md"
    request_path = stage_dir / "request.json"
    body_path.write_text(proposed_body, encoding="utf-8")
    request_path.write_text(
        json.dumps({"body": proposed_body}, sort_keys=True, separators=(",", ":")), encoding="utf-8"
    )
    report["staged_body"] = str(body_path)
    report["staged_request"] = str(request_path)
    report["patch_command"] = (
        f"gh api -X PATCH repos/{REPOSITORY}/issues/{issue} --input {request_path} --jq .updated_at"
    )

    report["failures"] = [f"abort: {reason}" for reason in abort_reasons] + failures

    if abort_reasons:
        report["outcome"] = "aborted"
        return report
    if failures:
        report["outcome"] = "invalid"
        return report
    if not write:
        report["outcome"] = "validated"
        return report

    try:
        updated_at = github.patch_body(issue, request_path)
    except GitHubError as error:
        report["outcome"] = "error"
        report["failures"].append(f"gh api PATCH failed: {error}")
        return report

    try:
        rewritten_issue = github.read_issue(issue)
    except GitHubError as error:
        report["outcome"] = "error"
        report["failures"].append(f"gh api re-read after write failed: {error}")
        return report

    remote_body = rewritten_issue.get("body") or ""
    remote_digest = None
    if digest is not None:
        try:
            remote_digest = plan_digest.digest_body(remote_body).plan_sha256
        except plan_digest.PlanDigestError:
            remote_digest = None

    if remote_body == proposed_body and digest is not None and remote_digest == digest.plan_sha256:
        report["outcome"] = "written"
        report["written"] = True
        report["updated_at"] = updated_at
        return report

    report["outcome"] = "aborted"
    report["written"] = True
    report["failures"].append("post-write re-read did not match the staged body or digest")
    return report


def _parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", required=True, help="absolute repository root")
    parser.add_argument("--base", required=True, help="full commit SHA captured when drafting began")
    parser.add_argument("--issue", required=True, type=int)
    parser.add_argument("--snapshot-file", required=True, help="start-of-drafting {number,title,body,state} JSON")
    parser.add_argument("--sections-file", required=True, help="the drafted managed sections, markdown")
    parser.add_argument("--stage-dir", help="defaults to a fresh temporary directory")
    parser.add_argument("--write", action="store_true", help="send the file-backed PATCH and verify it")
    return parser


def main(argv: Sequence[str] | None = None, github: GitHubClient | None = None) -> int:
    arguments = _parser().parse_args(argv)
    client = github if github is not None else GitHubClient()

    try:
        snapshot = json.loads(Path(arguments.snapshot_file).read_bytes().decode("utf-8"))
        sections_text = Path(arguments.sections_file).read_bytes().decode("utf-8")
        stage_dir = (
            Path(arguments.stage_dir)
            if arguments.stage_dir
            else Path(tempfile.mkdtemp(prefix=f"aether-scope-{arguments.issue}-"))
        )
        stage_dir.mkdir(parents=True, exist_ok=True)

        report = _finish(
            repo=Path(arguments.repo),
            base=arguments.base,
            issue=arguments.issue,
            snapshot=snapshot,
            sections_text=sections_text,
            stage_dir=stage_dir,
            write=arguments.write,
            github=client,
        )
    except (
        ScopeFinishError,
        GitHubError,
        resolve_approval_tier.ResolverError,
        OSError,
        UnicodeError,
        json.JSONDecodeError,
    ) as error:
        report = _empty_report(arguments.issue, arguments.base)
        report["failures"] = [str(error)]

    print(json.dumps(report, sort_keys=True, indent=2, ensure_ascii=True))
    return _EXIT_BY_OUTCOME[report["outcome"]]


if __name__ == "__main__":
    raise SystemExit(main())
