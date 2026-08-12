#!/usr/bin/env python3
"""Plan, apply, and audit the one-time canonical crate-label migration."""

from __future__ import annotations

import argparse
import hashlib
import json
import subprocess
import sys
from pathlib import Path
from typing import Any
from urllib.parse import quote, urlencode


CRATE_PREFIX = "crate:"
PACKAGE_PREFIX = "aether-"
CANONICAL_COLOR = "bfdadc"
SCHEMA_VERSION = 1


class MigrationError(RuntimeError):
    """A validation, inventory, transport, or reconciliation failure."""


class CommandTransport:
    """Run local commands; injectable so tests never invoke cargo or GitHub."""

    def run(self, argv: list[str], stdin: str | None = None) -> str:
        completed = subprocess.run(
            argv,
            input=stdin,
            text=True,
            capture_output=True,
            check=False,
        )
        if completed.returncode != 0:
            detail = completed.stderr.strip() or completed.stdout.strip() or "no diagnostic"
            raise MigrationError(f"command failed ({completed.returncode}): {' '.join(argv)}: {detail}")
        return completed.stdout


class GhApi:
    """Small authenticated `gh api` JSON client with explicit pagination."""

    def __init__(self, transport: CommandTransport):
        self.transport = transport

    def request(self, method: str, endpoint: str, body: dict[str, Any] | None = None) -> Any:
        argv = ["gh", "api"]
        if method != "GET":
            argv.extend(["-X", method])
        argv.append(endpoint)
        stdin = None
        if body is not None:
            argv.extend(["--input", "-"])
            stdin = canonical_json(body)
        raw = self.transport.run(argv, stdin)
        if not raw.strip():
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError as error:
            raise MigrationError(f"gh api returned invalid JSON for {method} {endpoint}: {error}") from error

    def get(self, endpoint: str) -> Any:
        return self.request("GET", endpoint)

    def pages(self, endpoint: str, **query: Any) -> list[Any]:
        items: list[Any] = []
        page = 1
        while True:
            page_query = dict(query)
            page_query.update({"page": page, "per_page": 100})
            separator = "&" if "?" in endpoint else "?"
            payload = self.get(endpoint + separator + urlencode(sorted(page_query.items())))
            if not isinstance(payload, list):
                raise MigrationError(f"paginated GET {endpoint} returned {type(payload).__name__}, not an array")
            items.extend(payload)
            if len(payload) < 100:
                return items
            page += 1


def canonical_json(value: Any) -> str:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"), sort_keys=True)


def plan_sha256(plan: dict[str, Any]) -> str:
    return hashlib.sha256(canonical_json(plan).encode("utf-8")).hexdigest()


def reject_duplicate_keys(pairs: list[tuple[str, Any]]) -> dict[str, Any]:
    value: dict[str, Any] = {}
    for key, item in pairs:
        if key in value:
            raise MigrationError(f"manifest contains duplicate JSON key: {key}")
        value[key] = item
    return value


def normalize_package(package: str) -> str:
    """Remove exactly one leading `aether-`; retain all other package names."""

    return package[len(PACKAGE_PREFIX) :] if package.startswith(PACKAGE_PREFIX) else package


def package_names(metadata: Any) -> list[str]:
    if not isinstance(metadata, dict) or not isinstance(metadata.get("packages"), list):
        raise MigrationError("cargo metadata did not contain a packages array")
    names: list[str] = []
    for package in metadata["packages"]:
        if not isinstance(package, dict) or not isinstance(package.get("name"), str):
            raise MigrationError("cargo metadata contained a package without a string name")
        names.append(package["name"])
    if len(names) != len(set(names)):
        raise MigrationError("cargo metadata contained duplicate package names")
    scopes = [normalize_package(name) for name in names]
    if len(scopes) != len(set(scopes)):
        raise MigrationError("package normalization produced duplicate crate scopes")
    return sorted(names)


def read_workspace_packages(transport: CommandTransport) -> list[str]:
    raw = transport.run(["cargo", "metadata", "--no-deps", "--locked", "--format-version", "1"])
    try:
        metadata = json.loads(raw)
    except json.JSONDecodeError as error:
        raise MigrationError(f"cargo metadata returned invalid JSON: {error}") from error
    return package_names(metadata)


def load_manifest(path: Path) -> dict[str, Any]:
    try:
        manifest = json.loads(path.read_text(encoding="utf-8"), object_pairs_hook=reject_duplicate_keys)
    except OSError as error:
        raise MigrationError(f"cannot read manifest {path}: {error}") from error
    except json.JSONDecodeError as error:
        raise MigrationError(f"manifest is not valid JSON: {error}") from error
    if not isinstance(manifest, dict):
        raise MigrationError("manifest root must be an object")
    if manifest.get("schema_version") != SCHEMA_VERSION:
        raise MigrationError(f"manifest schema_version must be {SCHEMA_VERSION}")
    if not isinstance(manifest.get("repository"), str) or "/" not in manifest["repository"]:
        raise MigrationError("manifest repository must be an owner/name string")
    packages = manifest.get("canonical_packages")
    if not isinstance(packages, list) or not packages or not all(isinstance(item, str) and item for item in packages):
        raise MigrationError("manifest canonical_packages must be a non-empty string array")
    if packages != sorted(packages) or len(packages) != len(set(packages)):
        raise MigrationError("manifest canonical_packages must be sorted and unique")
    scopes = [normalize_package(package) for package in packages]
    if len(scopes) != len(set(scopes)):
        raise MigrationError("manifest packages normalize to duplicate scopes")
    aliases = manifest.get("aliases")
    if not isinstance(aliases, list):
        raise MigrationError("manifest aliases must be an array")
    canonical_labels = {CRATE_PREFIX + scope for scope in scopes}
    seen: set[str] = set()
    previous = ""
    for entry in aliases:
        if not isinstance(entry, dict) or set(entry) != {"alias", "destination"}:
            raise MigrationError("every alias entry must contain only alias and destination")
        alias = entry["alias"]
        destination = entry["destination"]
        if not isinstance(alias, str) or not alias.startswith(CRATE_PREFIX):
            raise MigrationError("every alias must be a crate:* string")
        if alias in seen:
            raise MigrationError(f"duplicate alias disposition: {alias}")
        if alias <= previous:
            raise MigrationError("manifest aliases must be sorted by alias")
        if alias in canonical_labels:
            raise MigrationError(f"canonical label cannot be an alias: {alias}")
        if destination is not None and destination not in canonical_labels:
            raise MigrationError(f"alias {alias} has unknown destination {destination!r}")
        if destination == alias:
            raise MigrationError(f"alias {alias} cannot map to itself")
        seen.add(alias)
        previous = alias
    return manifest


def label_record(label: Any) -> dict[str, Any]:
    if not isinstance(label, dict) or not isinstance(label.get("name"), str):
        raise MigrationError("repository label inventory contained a malformed label")
    color = label.get("color")
    description = label.get("description")
    if not isinstance(color, str) or (description is not None and not isinstance(description, str)):
        raise MigrationError(f"repository label {label['name']} has malformed metadata")
    return {"color": color.lower(), "description": description, "name": label["name"]}


def resource_record(resource: Any) -> dict[str, Any]:
    if not isinstance(resource, dict) or not isinstance(resource.get("number"), int):
        raise MigrationError("issue inventory contained a malformed resource")
    raw_labels = resource.get("labels")
    if not isinstance(raw_labels, list):
        raise MigrationError(f"resource #{resource['number']} did not contain a label array")
    labels: list[str] = []
    for label in raw_labels:
        name = label.get("name") if isinstance(label, dict) else None
        if not isinstance(name, str):
            raise MigrationError(f"resource #{resource['number']} contained a malformed label")
        labels.append(name)
    if len(labels) != len(set(labels)):
        raise MigrationError(f"resource #{resource['number']} contained duplicate labels")
    return {
        "kind": "pull_request" if "pull_request" in resource else "issue",
        "labels": sorted(labels),
        "number": resource["number"],
    }


def replace_aliases(labels: list[str], dispositions: dict[str, str | None]) -> list[str]:
    replaced: set[str] = set()
    for label in labels:
        if label in dispositions:
            destination = dispositions[label]
            if destination is not None:
                replaced.add(destination)
        else:
            replaced.add(label)
    return sorted(replaced)


def canonical_description(package: str) -> str:
    return f"{package} workspace package"


class Migration:
    def __init__(self, api: GhApi, transport: CommandTransport, manifest: dict[str, Any]):
        self.api = api
        self.transport = transport
        self.manifest = manifest
        self.repository = manifest["repository"]
        self.packages: list[str] = manifest["canonical_packages"]
        self.package_by_scope = {normalize_package(package): package for package in self.packages}
        self.canonical_labels = sorted(CRATE_PREFIX + scope for scope in self.package_by_scope)
        self.dispositions = {entry["alias"]: entry["destination"] for entry in manifest["aliases"]}

    def validate_repository_and_packages(self) -> None:
        repository = self.api.get(f"repos/{self.repository}")
        if not isinstance(repository, dict) or repository.get("full_name") != self.repository:
            actual = repository.get("full_name") if isinstance(repository, dict) else None
            raise MigrationError(f"repository identity mismatch: expected {self.repository}, got {actual!r}")
        actual_packages = read_workspace_packages(self.transport)
        if actual_packages != self.packages:
            missing = sorted(set(self.packages) - set(actual_packages))
            unexpected = sorted(set(actual_packages) - set(self.packages))
            raise MigrationError(f"canonical package inventory drift: missing={missing}, unexpected={unexpected}")

    def inventory(self) -> tuple[list[dict[str, Any]], list[dict[str, Any]]]:
        labels = [label_record(label) for label in self.api.pages(f"repos/{self.repository}/labels")]
        names = [label["name"] for label in labels]
        if len(names) != len(set(names)):
            raise MigrationError("repository inventory contained duplicate label names")
        crate_labels = sorted(
            (label for label in labels if label["name"].startswith(CRATE_PREFIX)),
            key=lambda label: label["name"],
        )
        live_names = {label["name"] for label in crate_labels}
        unknown = sorted(live_names - set(self.canonical_labels) - set(self.dispositions))
        if unknown:
            raise MigrationError(f"crate labels lack explicit dispositions: {unknown}")

        resources = [
            resource_record(resource)
            for resource in self.api.pages(f"repos/{self.repository}/issues", state="all")
        ]
        numbers = [resource["number"] for resource in resources]
        if len(numbers) != len(set(numbers)):
            raise MigrationError("paginated issue inventory contained duplicate resource numbers")
        return crate_labels, resources

    def build_plan(self) -> dict[str, Any]:
        self.validate_repository_and_packages()
        crate_labels, resources = self.inventory()
        live_names = {label["name"] for label in crate_labels}
        canonical_live = live_names & set(self.canonical_labels)
        aliases_live = sorted(live_names & set(self.dispositions))

        owners: dict[str, list[str]] = {}
        for alias in aliases_live:
            destination = self.dispositions[alias]
            if destination is not None:
                owners.setdefault(destination, []).append(alias)

        renames: list[dict[str, Any]] = []
        rename_sources: set[str] = set()
        for destination in sorted(set(self.canonical_labels) - canonical_live):
            candidates = owners.get(destination, [])
            if len(candidates) == 1:
                source = candidates[0]
                scope = destination[len(CRATE_PREFIX) :]
                renames.append(
                    {
                        "color": CANONICAL_COLOR,
                        "description": canonical_description(self.package_by_scope[scope]),
                        "from": source,
                        "to": destination,
                    }
                )
                rename_sources.add(source)

        renamed_destinations = {rename["to"] for rename in renames}
        creates = []
        for name in sorted(set(self.canonical_labels) - canonical_live - renamed_destinations):
            scope = name[len(CRATE_PREFIX) :]
            creates.append(
                {
                    "color": CANONICAL_COLOR,
                    "description": canonical_description(self.package_by_scope[scope]),
                    "name": name,
                }
            )

        affected_resources: list[dict[str, Any]] = []
        updates: list[dict[str, Any]] = []
        alias_reference_count = 0
        aliases_live_set = set(aliases_live)
        for resource in sorted(resources, key=lambda item: item["number"]):
            references = sorted(set(resource["labels"]) & aliases_live_set)
            if not references:
                continue
            alias_reference_count += len(references)
            after = replace_aliases(resource["labels"], self.dispositions)
            record = {
                "after_labels": after,
                "before_labels": resource["labels"],
                "kind": resource["kind"],
                "number": resource["number"],
            }
            affected_resources.append(record)
            if set(references) - rename_sources:
                updates.append(record)

        deletes = [alias for alias in aliases_live if alias not in rename_sources]
        dispositions = [
            {"alias": alias, "destination": self.dispositions[alias]}
            for alias in sorted(self.dispositions)
        ]
        return {
            "canonical_labels": self.canonical_labels,
            "dispositions": dispositions,
            "inventory": {
                "crate_labels": crate_labels,
                "resources": affected_resources,
            },
            "operations": {
                "creates": creates,
                "deletes": deletes,
                "renames": renames,
                "resource_updates": updates,
            },
            "repository": self.repository,
            "rollup": {
                "alias_references": alias_reference_count,
                "aliases_present": len(aliases_live),
                "canonical_labels_present": len(canonical_live),
                "canonical_scopes": len(self.canonical_labels),
                "creates": len(creates),
                "deletes": len(deletes),
                "renames": len(renames),
                "resource_updates": len(updates),
                "resources_affected": len(affected_resources),
            },
            "schema_version": SCHEMA_VERSION,
        }

    def _live_labels_by_name(self) -> dict[str, dict[str, Any]]:
        labels = [label_record(label) for label in self.api.pages(f"repos/{self.repository}/labels")]
        return {label["name"]: label for label in labels}

    def _label_matches(self, expected: dict[str, Any]) -> bool:
        live = self._live_labels_by_name().get(expected["name"])
        return live is not None and live["color"] == expected["color"] and live["description"] == expected["description"]

    def _resource(self, number: int) -> dict[str, Any]:
        return resource_record(self.api.get(f"repos/{self.repository}/issues/{number}"))

    def _create_label(self, label: dict[str, Any]) -> None:
        try:
            created = self.api.request("POST", f"repos/{self.repository}/labels", label)
            if not isinstance(created, dict) or created.get("name") != label["name"]:
                raise MigrationError(f"create label returned an unexpected response for {label['name']}")
        except MigrationError:
            if not self._label_matches(label):
                raise

    def _rename_label(self, rename: dict[str, Any]) -> None:
        body = {
            "color": rename["color"],
            "description": rename["description"],
            "new_name": rename["to"],
        }
        try:
            updated = self.api.request(
                "PATCH",
                f"repos/{self.repository}/labels/{quote(rename['from'], safe='')}",
                body,
            )
            if not isinstance(updated, dict) or updated.get("name") != rename["to"]:
                raise MigrationError(f"rename label returned an unexpected response for {rename['from']}")
        except MigrationError:
            labels = self._live_labels_by_name()
            expected = {"color": rename["color"], "description": rename["description"], "name": rename["to"]}
            if rename["from"] in labels or labels.get(rename["to"]) != expected:
                raise

    def _update_resource(self, update: dict[str, Any], renames: list[dict[str, Any]]) -> None:
        current = self._resource(update["number"])
        if current["kind"] != update["kind"]:
            raise MigrationError(f"resource #{update['number']} changed kind")
        expected_intermediate = replace_aliases(
            update["before_labels"],
            {rename["from"]: rename["to"] for rename in renames},
        )
        if current["labels"] == update["after_labels"]:
            return
        if current["labels"] != expected_intermediate:
            raise MigrationError(
                f"resource #{update['number']} labels raced: expected {expected_intermediate}, got {current['labels']}"
            )
        try:
            self.api.request(
                "PUT",
                f"repos/{self.repository}/issues/{update['number']}/labels",
                {"labels": update["after_labels"]},
            )
        except MigrationError:
            if self._resource(update["number"])["labels"] != update["after_labels"]:
                raise
        verified = self._resource(update["number"])
        if verified["labels"] != update["after_labels"]:
            raise MigrationError(f"resource #{update['number']} label replacement did not verify")

    def _alias_references(self, alias: str) -> list[dict[str, Any]]:
        return [
            resource_record(resource)
            for resource in self.api.pages(
                f"repos/{self.repository}/issues",
                labels=alias,
                state="all",
            )
        ]

    def _delete_label(self, alias: str) -> None:
        references = self._alias_references(alias)
        if references:
            numbers = [resource["number"] for resource in references]
            raise MigrationError(f"refusing to delete {alias}; still attached to resources {numbers}")
        try:
            self.api.request(
                "DELETE",
                f"repos/{self.repository}/labels/{quote(alias, safe='')}",
            )
        except MigrationError:
            if alias in self._live_labels_by_name():
                raise

    def apply(self, plan: dict[str, Any], confirmed_sha256: str) -> dict[str, Any]:
        digest = plan_sha256(plan)
        if confirmed_sha256 != digest:
            raise MigrationError(f"confirmation digest mismatch: expected {digest}")
        fresh = self.build_plan()
        if plan_sha256(fresh) != digest or fresh != plan:
            raise MigrationError("inventory changed after planning; refusing all writes")

        operations = plan["operations"]
        for label in operations["creates"]:
            self._create_label(label)
        for rename in operations["renames"]:
            self._rename_label(rename)
        for update in operations["resource_updates"]:
            self._update_resource(update, operations["renames"])
        for alias in operations["deletes"]:
            self._delete_label(alias)
        return self.audit()

    def audit(self) -> dict[str, Any]:
        self.validate_repository_and_packages()
        crate_labels, resources = self.inventory()
        names = sorted(label["name"] for label in crate_labels)
        if names != self.canonical_labels:
            missing = sorted(set(self.canonical_labels) - set(names))
            unexpected = sorted(set(names) - set(self.canonical_labels))
            raise MigrationError(f"final crate-label inventory mismatch: missing={missing}, unexpected={unexpected}")
        alias_references = sorted(
            {
                label
                for resource in resources
                for label in resource["labels"]
                if label in self.dispositions
            }
        )
        if alias_references:
            raise MigrationError(f"final inventory retains alias references: {alias_references}")
        independently_attached = {
            alias: [resource["number"] for resource in self._alias_references(alias)]
            for alias in sorted(self.dispositions)
        }
        independently_attached = {
            alias: numbers for alias, numbers in independently_attached.items() if numbers
        }
        if independently_attached:
            raise MigrationError(f"former aliases retain independently queried references: {independently_attached}")
        return {
            "aliases_with_references": 0,
            "canonical_labels": len(self.canonical_labels),
            "repository": self.repository,
            "status": "ok",
        }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--manifest",
        type=Path,
        default=Path(__file__).with_name("crate-label-migration.json"),
    )
    mode = parser.add_mutually_exclusive_group()
    mode.add_argument("--apply", action="store_true", help="apply a freshly confirmed plan")
    mode.add_argument("--audit", action="store_true", help="require the exact final canonical inventory")
    parser.add_argument("--confirm-plan-sha256", metavar="SHA256")
    args = parser.parse_args(argv)
    if args.apply and args.confirm_plan_sha256 is None:
        parser.error("--apply requires --confirm-plan-sha256")
    if not args.apply and args.confirm_plan_sha256 is not None:
        parser.error("--confirm-plan-sha256 is valid only with --apply")
    return args


def main(argv: list[str] | None = None) -> int:
    try:
        args = parse_args(sys.argv[1:] if argv is None else argv)
        transport = CommandTransport()
        migration = Migration(GhApi(transport), transport, load_manifest(args.manifest))
        if args.audit:
            output: dict[str, Any] = {"audit": migration.audit()}
        else:
            plan = migration.build_plan()
            digest = plan_sha256(plan)
            output = {"plan": plan, "plan_sha256": digest}
            if args.apply:
                output["audit"] = migration.apply(plan, args.confirm_plan_sha256)
        print(json.dumps(output, ensure_ascii=False, indent=2, sort_keys=True))
        return 0
    except MigrationError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
