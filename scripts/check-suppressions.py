#!/usr/bin/env python3
"""Reject suppressions introduced by one git diff.

The mechanical scan is deliberately local and dependency-free.  Pull-request
authorization is a separate final verdict: findings remain visible, and only
an owner-edited, commit-bound marker in the current PR body can clear them.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import urllib.error
import urllib.request
from collections import Counter
from dataclasses import dataclass
from pathlib import Path
from typing import Callable


SIGNOFF_PREFIX = "<!-- aether-suppression-signoff:v1 "
SIGNOFF_RE = re.compile(r"<!-- aether-suppression-signoff:v1 (\{[^\r\n]*\}) -->")
HEX_SHA_RE = re.compile(r"[0-9a-f]{40}")
HUNK_RE = re.compile(r"^@@ -\d+(?:,\d+)? \+(\d+)(?:,\d+)? @@")
RUST_ATTRIBUTE_RE = re.compile(r"^\s*#!?\[\s*(allow|expect)\s*\(")
RUST_IGNORE_RE = re.compile(r'^\s*#\[\s*ignore(?:\s*=\s*"(?:\\.|[^"\\])*")?\s*\]\s*$')
TOML_STRING_RE = re.compile(r'"(?:\\.|[^"\\])*"|\'(?:[^\']|\'\')*\'')


class OperationalError(RuntimeError):
    """The scanner could not compute an authoritative verdict."""


@dataclass(frozen=True, order=True)
class Suppression:
    path: str
    line: int
    token: str
    source: str

    def render(self) -> str:
        return f"{self.path}:{self.line} — {self.token} — {self.source.rstrip()}"


@dataclass
class DiffAdditions:
    lines: dict[str, dict[int, str]]
    base_paths: dict[str, str | None]


def git(root: Path, *args: str, allow_missing: bool = False) -> str | None:
    completed = subprocess.run(
        ["git", *args],
        cwd=root,
        check=False,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    if completed.returncode == 0:
        return completed.stdout
    if allow_missing:
        return None
    detail = completed.stderr.strip() or completed.stdout.strip() or f"git exited {completed.returncode}"
    raise OperationalError(detail)


def resolve_diff(root: Path, base_ref: str, head_ref: str) -> tuple[str, str]:
    base = git(root, "rev-parse", "--verify", f"{base_ref}^{{commit}}")
    head = git(root, "rev-parse", "--verify", f"{head_ref}^{{commit}}")
    assert base is not None and head is not None
    head = head.strip()
    merge_base = git(root, "merge-base", base.strip(), head)
    assert merge_base is not None
    merge_base = merge_base.strip()
    if not HEX_SHA_RE.fullmatch(merge_base) or not HEX_SHA_RE.fullmatch(head):
        raise OperationalError("git did not resolve the diff endpoints to full commit SHAs")
    return merge_base, head


def diff_path(header: str, prefix: str) -> str | None:
    value = header[len(prefix) :]
    if value == "/dev/null":
        return None
    expected = "a/" if prefix == "--- " else "b/"
    if not value.startswith(expected) or value.startswith('"'):
        raise OperationalError(f"unsupported quoted or malformed diff path: {header}")
    return value[2:]


def collect_added_lines(root: Path, base: str, head: str) -> DiffAdditions:
    rendered = git(
        root,
        "-c",
        "core.quotePath=false",
        "diff",
        "--find-renames",
        "--unified=0",
        "--no-color",
        "--no-ext-diff",
        base,
        head,
        "--",
    )
    assert rendered is not None

    additions: dict[str, dict[int, str]] = {}
    base_paths: dict[str, str | None] = {}
    old_path: str | None = None
    new_path: str | None = None
    head_line: int | None = None

    for line in rendered.splitlines():
        if line.startswith("diff --git "):
            old_path = None
            new_path = None
            head_line = None
            continue
        if line.startswith("@@ "):
            match = HUNK_RE.match(line)
            if match is None:
                raise OperationalError(f"malformed zero-context diff hunk: {line}")
            head_line = int(match.group(1)) if new_path is not None else None
            continue
        if head_line is not None:
            if line.startswith("+"):
                additions.setdefault(new_path, {})[head_line] = line[1:]
                head_line += 1
            elif line.startswith("-"):
                continue
            elif line.startswith(" "):
                head_line += 1
            elif line.startswith("\\ No newline at end of file"):
                continue
            else:
                raise OperationalError(f"malformed line inside diff hunk: {line}")
            continue
        if line.startswith("--- "):
            old_path = diff_path(line, "--- ")
            head_line = None
            continue
        if line.startswith("+++ "):
            new_path = diff_path(line, "+++ ")
            if new_path is not None:
                base_paths[new_path] = old_path
            head_line = None
            continue

    return DiffAdditions(additions, base_paths)


def blob(root: Path, commit: str, path: str | None) -> str:
    if path is None:
        return ""
    rendered = git(root, "show", f"{commit}:{path}", allow_missing=True)
    if rendered is None:
        raise OperationalError(f"cannot read {path} at {commit}")
    return rendered


def sanitize_rust(source: str) -> list[str]:
    """Mask comments and string bodies while preserving physical lines."""

    output: list[str] = []
    block_depth = 0
    in_string = False
    raw_hashes: int | None = None
    escaped = False

    for raw_line in source.splitlines():
        chars: list[str] = []
        index = 0
        while index < len(raw_line):
            pair = raw_line[index : index + 2]
            char = raw_line[index]
            if raw_hashes is not None:
                terminator = '"' + "#" * raw_hashes
                if raw_line.startswith(terminator, index):
                    chars.extend(" " * len(terminator))
                    index += len(terminator)
                    raw_hashes = None
                else:
                    chars.append(" ")
                    index += 1
                continue
            if block_depth:
                if pair == "/*":
                    block_depth += 1
                    chars.extend("  ")
                    index += 2
                elif pair == "*/":
                    block_depth -= 1
                    chars.extend("  ")
                    index += 2
                else:
                    chars.append(" ")
                    index += 1
                continue
            if in_string:
                chars.append('"' if char == '"' else " ")
                if escaped:
                    escaped = False
                elif char == "\\":
                    escaped = True
                elif char == '"':
                    in_string = False
                index += 1
                continue
            if pair == "//":
                chars.extend(" " * (len(raw_line) - index))
                break
            if pair == "/*":
                block_depth = 1
                chars.extend("  ")
                index += 2
                continue
            raw = re.match(r'(?:b)?r(#{0,255})"', raw_line[index:])
            if raw is not None:
                raw_hashes = len(raw.group(1))
                chars.extend(" " * len(raw.group(0)))
                index += len(raw.group(0))
                continue
            if char == '"':
                in_string = True
                chars.append('"')
                index += 1
                continue
            chars.append(char)
            index += 1
        output.append("".join(chars))
    return output


def rust_attribute_spans(sanitized: list[str]) -> list[tuple[int, int, str]]:
    spans: list[tuple[int, int, str]] = []
    line = 0
    while line < len(sanitized):
        match = RUST_ATTRIBUTE_RE.match(sanitized[line])
        if match is None:
            line += 1
            continue
        start = line + 1
        bracket_depth = sanitized[line].count("[") - sanitized[line].count("]")
        while bracket_depth > 0 and line + 1 < len(sanitized):
            line += 1
            bracket_depth += sanitized[line].count("[") - sanitized[line].count("]")
        if bracket_depth != 0:
            raise OperationalError(f"unterminated Rust suppression attribute at line {start}")
        spans.append((start, line + 1, match.group(1)))
        line += 1
    return spans


def lint_token(kind: str, sanitized_line: str) -> str:
    identifiers = re.findall(r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*", sanitized_line)
    lint = next((identifier for identifier in identifiers if identifier not in {"allow", "expect"}), None)
    return f"{kind}({lint})" if lint else f"{kind}(...)"


def rust_suppressions(path: str, added: dict[int, str], head_source: str) -> list[Suppression]:
    sanitized = sanitize_rust(head_source)
    spans = rust_attribute_spans(sanitized)
    findings: list[Suppression] = []

    for line, source in added.items():
        masked = sanitized[line - 1] if 0 < line <= len(sanitized) else ""
        if RUST_IGNORE_RE.match(masked):
            findings.append(Suppression(path, line, "ignore", source))
            continue
        for start, end, kind in spans:
            if not start <= line <= end:
                continue
            if start in added:
                if line == start:
                    findings.append(Suppression(path, line, lint_token(kind, masked), source))
                break
            if re.search(r"[A-Za-z_][A-Za-z0-9_]*(?:::[A-Za-z_][A-Za-z0-9_]*)*", masked):
                findings.append(Suppression(path, line, lint_token(kind, masked), source))
            break
    return findings


@dataclass(frozen=True)
class JsonToken:
    kind: str
    value: object
    line: int


def json_tokens(source: str) -> list[JsonToken]:
    decoder = json.JSONDecoder()
    tokens: list[JsonToken] = []
    index = 0
    line = 1
    punctuation = {"{", "}", "[", "]", ":", ","}
    while index < len(source):
        char = source[index]
        if char.isspace():
            if char == "\n":
                line += 1
            index += 1
            continue
        if char in punctuation:
            tokens.append(JsonToken(char, char, line))
            index += 1
            continue
        try:
            value, end = decoder.raw_decode(source, index)
        except json.JSONDecodeError as error:
            raise OperationalError(f"malformed JSON near line {line}: {error.msg}") from error
        tokens.append(JsonToken("string" if isinstance(value, str) else "scalar", value, line))
        line += source[index:end].count("\n")
        index = end
    return tokens


def skip_json_value(tokens: list[JsonToken], index: int) -> int:
    if index >= len(tokens):
        raise OperationalError("missing JSON value")
    opening = tokens[index].kind
    closing = {"{": "}", "[": "]"}.get(opening)
    if closing is None:
        return index + 1
    depth = 1
    index += 1
    while index < len(tokens) and depth:
        if tokens[index].kind == opening:
            depth += 1
        elif tokens[index].kind == closing:
            depth -= 1
        index += 1
    if depth:
        raise OperationalError("unterminated JSON collection")
    return index


def top_level_json_ignore_locations(source: str) -> list[tuple[str, int]]:
    tokens = json_tokens(source)
    if not tokens or tokens[0].kind != "{":
        raise OperationalError(".jscpd.json must contain a top-level object")
    index = 1
    while index < len(tokens) and tokens[index].kind != "}":
        key = tokens[index]
        if key.kind != "string" or index + 1 >= len(tokens) or tokens[index + 1].kind != ":":
            raise OperationalError("malformed top-level .jscpd.json member")
        index += 2
        if key.value != "ignore":
            index = skip_json_value(tokens, index)
        else:
            if index >= len(tokens) or tokens[index].kind != "[":
                raise OperationalError(".jscpd.json top-level ignore must be an array")
            index += 1
            values: list[tuple[str, int]] = []
            while index < len(tokens) and tokens[index].kind != "]":
                token = tokens[index]
                if token.kind != "string":
                    raise OperationalError(".jscpd.json ignore members must be strings")
                values.append((str(token.value), token.line))
                index += 1
                if index < len(tokens) and tokens[index].kind == ",":
                    index += 1
            if index >= len(tokens):
                raise OperationalError("unterminated .jscpd.json ignore array")
            return values
        if index < len(tokens) and tokens[index].kind == ",":
            index += 1
    return []


def semantic_json_ignore(source: str) -> list[str]:
    try:
        parsed = json.loads(source or "{}")
    except json.JSONDecodeError as error:
        raise OperationalError(f"malformed .jscpd.json: {error.msg}") from error
    values = parsed.get("ignore", []) if isinstance(parsed, dict) else None
    if not isinstance(values, list) or not all(isinstance(value, str) for value in values):
        raise OperationalError(".jscpd.json top-level ignore must be an array of strings")
    return values


def semantic_machete_ignored(source: str) -> list[str]:
    if not source:
        return []
    try:
        import tomllib

        parsed = tomllib.loads(source)
    except (ImportError, ValueError) as error:
        raise OperationalError(f"malformed Cargo.toml: {error}") from error
    package = parsed.get("package", {})
    metadata = package.get("metadata", {}) if isinstance(package, dict) else {}
    machete = metadata.get("cargo-machete", {}) if isinstance(metadata, dict) else {}
    values = machete.get("ignored", []) if isinstance(machete, dict) else None
    if not isinstance(values, list) or not all(isinstance(value, str) for value in values):
        raise OperationalError("package.metadata.cargo-machete.ignored must be an array of strings")
    return values


def toml_expression(source: str) -> tuple[str, int] | None:
    lines = source.splitlines()
    in_target = False
    for index, raw in enumerate(lines):
        stripped = raw.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            in_target = stripped == "[package.metadata.cargo-machete]"
            continue
        match = None
        if in_target:
            match = re.match(r"^\s*ignored\s*=\s*(.*)$", raw)
        if match is None:
            match = re.match(r"^\s*package\.metadata\.cargo-machete\.ignored\s*=\s*(.*)$", raw)
        if match is None:
            continue
        parts = [match.group(1)]
        balance = match.group(1).count("[") - match.group(1).count("]")
        cursor = index + 1
        while balance > 0 and cursor < len(lines):
            parts.append(lines[cursor])
            balance += lines[cursor].count("[") - lines[cursor].count("]")
            cursor += 1
        if balance != 0:
            raise OperationalError("unterminated cargo-machete ignored array")
        return "\n".join(parts), index + 1
    return None


def machete_ignore_locations(source: str) -> list[tuple[str, int]]:
    expression = toml_expression(source)
    if expression is None:
        return []
    rendered, start_line = expression
    locations: list[tuple[str, int]] = []
    for match in TOML_STRING_RE.finditer(rendered):
        literal = match.group(0)
        try:
            import tomllib

            value = tomllib.loads(f"x = {literal}")["x"]
        except (ImportError, ValueError, KeyError) as error:
            raise OperationalError(f"cannot locate cargo-machete ignored member: {error}") from error
        locations.append((value, start_line + rendered[: match.start()].count("\n")))
    return locations


def added_members(before: list[str], after: list[str]) -> list[str]:
    remaining = Counter(after) - Counter(before)
    selected: list[str] = []
    for value in after:
        if remaining[value]:
            selected.append(value)
            remaining[value] -= 1
    return selected


def config_suppressions(
    path: str,
    added: dict[int, str],
    base_source: str,
    head_source: str,
    kind: str,
) -> list[Suppression]:
    if kind == "jscpd-ignore":
        before = semantic_json_ignore(base_source)
        after = semantic_json_ignore(head_source)
        locations = top_level_json_ignore_locations(head_source)
    else:
        before = semantic_machete_ignored(base_source)
        after = semantic_machete_ignored(head_source)
        locations = machete_ignore_locations(head_source)

    new_values = Counter(added_members(before, after))
    findings: list[Suppression] = []
    for value, line in locations:
        if not new_values[value]:
            continue
        if line not in added:
            raise OperationalError(f"new {kind} member in {path} is not located on an added diff line")
        findings.append(Suppression(path, line, f'{kind}("{value}")', added[line]))
        new_values[value] -= 1
    if any(new_values.values()):
        raise OperationalError(f"could not locate every new {kind} member in {path}")
    return findings


def scan_repository(root: Path, base_ref: str, head_ref: str) -> tuple[str, str, list[Suppression]]:
    base, head = resolve_diff(root, base_ref, head_ref)
    diff = collect_added_lines(root, base, head)
    findings: list[Suppression] = []
    for path, added in diff.lines.items():
        head_source = blob(root, head, path)
        if path.endswith(".rs"):
            findings.extend(rust_suppressions(path, added, head_source))
        elif Path(path).name == ".jscpd.json":
            findings.extend(
                config_suppressions(
                    path,
                    added,
                    blob(root, base, diff.base_paths.get(path)),
                    head_source,
                    "jscpd-ignore",
                )
            )
        elif Path(path).name == "Cargo.toml":
            findings.extend(
                config_suppressions(
                    path,
                    added,
                    blob(root, base, diff.base_paths.get(path)),
                    head_source,
                    "cargo-machete-ignored",
                )
            )
    return base, head, sorted(findings)


def canonical_marker(payload: dict[str, object]) -> str:
    return f"{SIGNOFF_PREFIX}{json.dumps(payload, sort_keys=True, separators=(',', ':'))} -->"


def parse_signoff(body: str) -> dict[str, object] | None:
    if body.count("aether-suppression-signoff:v1") != 1:
        return None
    match = SIGNOFF_RE.search(body)
    if match is None:
        return None
    try:
        payload = json.loads(match.group(1))
    except json.JSONDecodeError:
        return None
    if not isinstance(payload, dict) or set(payload) != {"base_sha", "head_sha", "pull_request"}:
        return None
    if not isinstance(payload["pull_request"], int) or isinstance(payload["pull_request"], bool):
        return None
    if not isinstance(payload["base_sha"], str) or not HEX_SHA_RE.fullmatch(payload["base_sha"]):
        return None
    if not isinstance(payload["head_sha"], str) or not HEX_SHA_RE.fullmatch(payload["head_sha"]):
        return None
    if canonical_marker(payload) != match.group(0):
        return None
    return payload


GraphqlCall = Callable[[str, dict[str, object], str], dict[str, object]]


def github_graphql(query: str, variables: dict[str, object], token: str) -> dict[str, object]:
    request = urllib.request.Request(
        "https://api.github.com/graphql",
        data=json.dumps({"query": query, "variables": variables}).encode(),
        headers={
            "Accept": "application/vnd.github+json",
            "Authorization": f"Bearer {token}",
            "Content-Type": "application/json",
            "User-Agent": "aether-suppression-gate",
            "X-GitHub-Api-Version": "2022-11-28",
        },
        method="POST",
    )
    try:
        with urllib.request.urlopen(request, timeout=20) as response:
            return json.load(response)
    except (OSError, urllib.error.HTTPError, json.JSONDecodeError) as error:
        raise OperationalError(f"GitHub GraphQL request failed: {error}") from error


def trusted_signoff(
    pull_request: int,
    repository: str,
    base: str,
    head: str,
    token: str,
    graphql: GraphqlCall = github_graphql,
) -> bool:
    parts = repository.split("/")
    if len(parts) != 2 or not all(parts):
        raise OperationalError("repository must be OWNER/NAME")
    query = """
query SuppressionSignoff($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    owner { login }
    pullRequest(number: $number) {
      number
      body
      editor { login }
      headRefOid
    }
  }
}
"""
    response = graphql(query, {"owner": parts[0], "name": parts[1], "number": pull_request}, token)
    if response.get("errors"):
        raise OperationalError("GitHub GraphQL returned errors")
    try:
        repo = response["data"]["repository"]
        owner = repo["owner"]["login"]
        pr = repo["pullRequest"]
        body = pr["body"] or ""
        editor = pr["editor"]
        current_head = pr["headRefOid"]
        current_number = pr["number"]
    except (KeyError, TypeError) as error:
        raise OperationalError("GitHub GraphQL omitted pull-request authorization data") from error

    marker = parse_signoff(body)
    return bool(
        marker is not None
        and isinstance(editor, dict)
        and editor.get("login") == owner
        and current_number == pull_request
        and current_head == head
        and marker == {"base_sha": base, "head_sha": head, "pull_request": pull_request}
    )


def arguments(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", default="origin/main", help="base ref; its merge base with --head is scanned")
    parser.add_argument("--head", default="HEAD", help="head ref to scan")
    parser.add_argument("--pull-request", type=int, help="PR number whose current hidden owner sign-off may authorize findings")
    parser.add_argument("--repository", default=os.environ.get("GITHUB_REPOSITORY"), help="OWNER/NAME for --pull-request")
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = arguments(argv if argv is not None else sys.argv[1:])
    try:
        base, head, findings = scan_repository(Path.cwd(), args.base, args.head)
        for finding in findings:
            print(finding.render())
        if not findings:
            return 0
        if args.pull_request is None:
            return 1
        if not args.repository:
            raise OperationalError("--repository or GITHUB_REPOSITORY is required with --pull-request")
        token = os.environ.get("GITHUB_TOKEN")
        if not token:
            raise OperationalError("GITHUB_TOKEN is required to verify pull-request body provenance")
        return 0 if trusted_signoff(args.pull_request, args.repository, base, head, token) else 1
    except OperationalError as error:
        print(f"suppression scan error: {error}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
