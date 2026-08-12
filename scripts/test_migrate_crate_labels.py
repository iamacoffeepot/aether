#!/usr/bin/env python3
"""Regression tests for the one-time canonical crate-label migration."""

from __future__ import annotations

import importlib.util
import json
import sys
import tempfile
import unittest
from pathlib import Path
from urllib.parse import parse_qs, unquote, urlsplit


SCRIPT = Path(__file__).with_name("migrate-crate-labels.py")
SPEC = importlib.util.spec_from_file_location("migrate_crate_labels", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
migration_tool = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = migration_tool
SPEC.loader.exec_module(migration_tool)


REPOSITORY = "iamacoffeepot/aether"


def label(name: str, color: str = "ededed", description: str | None = "") -> dict[str, object]:
    return {"color": color, "description": description, "name": name}


def resource(number: int, labels: list[str], kind: str = "issue") -> dict[str, object]:
    value: dict[str, object] = {
        "labels": [{"name": name} for name in labels],
        "number": number,
    }
    if kind == "pull_request":
        value["pull_request"] = {"url": f"https://example.invalid/pulls/{number}"}
    return value


def manifest(packages: list[str], aliases: dict[str, str | None]) -> dict[str, object]:
    return {
        "aliases": [
            {"alias": alias, "destination": destination}
            for alias, destination in sorted(aliases.items())
        ],
        "canonical_packages": sorted(packages),
        "repository": REPOSITORY,
        "schema_version": 1,
    }


class FakeCommandTransport:
    """Stateful cargo/gh command transport with real page and mutation shapes."""

    def __init__(
        self,
        packages: list[str],
        labels: list[dict[str, object]],
        resources: list[dict[str, object]],
    ) -> None:
        self.packages = packages
        self.labels = {item["name"]: dict(item) for item in labels}
        self.resources = {item["number"]: json.loads(json.dumps(item)) for item in resources}
        self.calls: list[tuple[str, str, dict[str, object] | None]] = []
        self.uncertain: set[tuple[str, str]] = set()
        self.issue_inventory_reads = 0
        self.drift_on_issue_inventory_read: int | None = None

    def run(self, argv: list[str], stdin: str | None = None) -> str:
        if argv[:2] == ["cargo", "metadata"]:
            return json.dumps({"packages": [{"name": name} for name in self.packages]})
        if argv[:2] != ["gh", "api"]:
            raise AssertionError(f"unexpected command: {argv}")
        method = "GET"
        index = 2
        if argv[index : index + 1] == ["-X"]:
            method = argv[index + 1]
            index += 2
        endpoint = argv[index]
        body = json.loads(stdin) if stdin is not None else None
        self.calls.append((method, endpoint, body))
        result = self._request(method, endpoint, body)
        if (method, urlsplit(endpoint).path) in self.uncertain:
            self.uncertain.remove((method, urlsplit(endpoint).path))
            raise migration_tool.MigrationError("simulated lost response")
        return "" if result is None else json.dumps(result)

    def _request(self, method: str, endpoint: str, body: dict[str, object] | None) -> object:
        parsed = urlsplit(endpoint)
        path = parsed.path
        query = parse_qs(parsed.query)
        labels_path = f"repos/{REPOSITORY}/labels"
        issues_path = f"repos/{REPOSITORY}/issues"
        if method == "GET" and path == f"repos/{REPOSITORY}":
            return {"full_name": REPOSITORY}
        if method == "GET" and path == labels_path:
            return self._page(sorted(self.labels.values(), key=lambda item: item["name"]), query)
        if method == "GET" and path == issues_path:
            if "labels" not in query:
                self.issue_inventory_reads += 1
                if self.issue_inventory_reads == self.drift_on_issue_inventory_read:
                    first = self.resources[min(self.resources)]
                    first["labels"].append({"name": "priority:drift"})
            values = sorted(self.resources.values(), key=lambda item: item["number"])
            if "labels" in query:
                wanted = query["labels"][0]
                values = [
                    item
                    for item in values
                    if wanted in {entry["name"] for entry in item["labels"]}
                ]
            return self._page(values, query)
        if method == "GET" and path.startswith(issues_path + "/"):
            return self.resources[int(path.rsplit("/", 1)[1])]
        if method == "POST" and path == labels_path:
            assert body is not None
            self.labels[body["name"]] = dict(body)
            return dict(body)
        if method == "PATCH" and path.startswith(labels_path + "/"):
            assert body is not None
            old_name = unquote(path[len(labels_path) + 1 :])
            old = self.labels.pop(old_name)
            new_name = body["new_name"]
            updated = {
                "color": body.get("color", old["color"]),
                "description": body.get("description", old["description"]),
                "name": new_name,
            }
            self.labels[new_name] = updated
            for item in self.resources.values():
                names = [entry["name"] for entry in item["labels"]]
                if old_name in names:
                    names = [new_name if name == old_name else name for name in names]
                    item["labels"] = [{"name": name} for name in dict.fromkeys(names)]
            return updated
        if method == "PUT" and path.startswith(issues_path + "/") and path.endswith("/labels"):
            assert body is not None
            number = int(path.removeprefix(issues_path + "/").removesuffix("/labels"))
            self.resources[number]["labels"] = [{"name": name} for name in body["labels"]]
            return self.resources[number]["labels"]
        if method == "DELETE" and path.startswith(labels_path + "/"):
            name = unquote(path[len(labels_path) + 1 :])
            self.labels.pop(name, None)
            for item in self.resources.values():
                item["labels"] = [entry for entry in item["labels"] if entry["name"] != name]
            return None
        raise AssertionError(f"unhandled fake request: {method} {endpoint} {body}")

    @staticmethod
    def _page(values: list[object], query: dict[str, list[str]]) -> list[object]:
        page = int(query.get("page", ["1"])[0])
        per_page = int(query.get("per_page", ["100"])[0])
        start = (page - 1) * per_page
        return values[start : start + per_page]

    def mutation_calls(self) -> list[tuple[str, str, dict[str, object] | None]]:
        return [call for call in self.calls if call[0] != "GET"]


def make_migration(
    packages: list[str],
    aliases: dict[str, str | None],
    labels: list[dict[str, object]],
    resources: list[dict[str, object]],
) -> tuple[migration_tool.Migration, FakeCommandTransport]:
    transport = FakeCommandTransport(packages, labels, resources)
    return (
        migration_tool.Migration(
            migration_tool.GhApi(transport),
            transport,
            manifest(packages, aliases),
        ),
        transport,
    )


class NormalizationAndManifestTests(unittest.TestCase):
    def test_normalization_strips_exactly_one_prefix_and_retains_xtask(self) -> None:
        self.assertEqual(migration_tool.normalize_package("aether-actor"), "actor")
        self.assertEqual(migration_tool.normalize_package("aether-aether-actor"), "aether-actor")
        self.assertEqual(migration_tool.normalize_package("xtask"), "xtask")

    def test_checked_in_manifest_has_complete_reviewed_dispositions(self) -> None:
        loaded = migration_tool.load_manifest(SCRIPT.with_name("crate-label-migration.json"))
        self.assertEqual(len(loaded["canonical_packages"]), 61)
        self.assertEqual(len(loaded["aliases"]), 76)
        dispositions = {item["alias"]: item["destination"] for item in loaded["aliases"]}
        self.assertEqual(dispositions["crate:aether-bloomery-host"], "crate:chassis-bloomery")
        self.assertEqual(dispositions["crate:aether-fleet-bench"], "crate:harness-fleet")
        self.assertEqual(dispositions["crate:aether-substrate-bench"], "crate:harness-substrate")
        self.assertEqual(
            dispositions["crate:aether-substrate-bench-capture"],
            "crate:harness-substrate-capture",
        )
        self.assertEqual(dispositions["crate:aether-kit"], "crate:kit-commons")
        self.assertIsNone(dispositions["crate:repo"])
        self.assertIsNone(dispositions["crate:aether-input"])

    def test_manifest_rejects_duplicate_json_keys_and_aliases(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "manifest.json"
            path.write_text('{"schema_version":1,"schema_version":1}', encoding="utf-8")
            with self.assertRaisesRegex(migration_tool.MigrationError, "duplicate JSON key"):
                migration_tool.load_manifest(path)

            duplicate = manifest(["aether-alpha"], {})
            duplicate["aliases"] = [
                {"alias": "crate:old", "destination": None},
                {"alias": "crate:old", "destination": None},
            ]
            path.write_text(json.dumps(duplicate), encoding="utf-8")
            with self.assertRaisesRegex(migration_tool.MigrationError, "duplicate alias"):
                migration_tool.load_manifest(path)

    def test_unknown_live_label_and_unknown_destination_are_rejected(self) -> None:
        migration, _ = make_migration(["aether-alpha"], {}, [label("crate:unknown")], [])
        with self.assertRaisesRegex(migration_tool.MigrationError, "lack explicit dispositions"):
            migration.build_plan()

        invalid = manifest(["aether-alpha"], {"crate:old": "crate:missing"})
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "manifest.json"
            path.write_text(json.dumps(invalid), encoding="utf-8")
            with self.assertRaisesRegex(migration_tool.MigrationError, "unknown destination"):
                migration_tool.load_manifest(path)


class PlanningTests(unittest.TestCase):
    def test_plan_uses_create_rename_and_merge_paths(self) -> None:
        migration, _ = make_migration(
            ["aether-alpha", "aether-beta", "aether-gamma"],
            {
                "crate:aether-alpha": "crate:alpha",
                "crate:aether-gamma": "crate:gamma",
            },
            [
                label("crate:aether-alpha"),
                label("crate:aether-gamma"),
                label("crate:gamma", "bfdadc", "existing"),
            ],
            [resource(1, ["crate:aether-gamma"])],
        )
        plan = migration.build_plan()
        self.assertEqual([item["name"] for item in plan["operations"]["creates"]], ["crate:beta"])
        self.assertEqual(
            [(item["from"], item["to"]) for item in plan["operations"]["renames"]],
            [("crate:aether-alpha", "crate:alpha")],
        )
        self.assertEqual(plan["operations"]["resource_updates"][0]["after_labels"], ["crate:gamma"])
        self.assertEqual(plan["operations"]["deletes"], ["crate:aether-gamma"])

    def test_multi_alias_deduplicates_and_preserves_every_unrelated_label(self) -> None:
        migration, _ = make_migration(
            ["aether-alpha"],
            {
                "crate:aether-alpha": "crate:alpha",
                "crate:alpha-old": "crate:alpha",
                "crate:retired": None,
            },
            [
                label("crate:alpha", "bfdadc", "existing"),
                label("crate:aether-alpha"),
                label("crate:alpha-old"),
                label("crate:retired"),
            ],
            [
                resource(
                    7,
                    [
                        "crate:aether-alpha",
                        "crate:alpha-old",
                        "crate:retired",
                        "phase:ready",
                        "size:l",
                        "model:opus",
                        "type:chore",
                        "custom",
                    ],
                )
            ],
        )
        update = migration.build_plan()["operations"]["resource_updates"][0]
        self.assertEqual(
            update["after_labels"],
            ["crate:alpha", "custom", "model:opus", "phase:ready", "size:l", "type:chore"],
        )

    def test_issue_and_pull_request_resources_are_distinguished(self) -> None:
        migration, _ = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha"},
            [label("crate:alpha", "bfdadc", "existing"), label("crate:old")],
            [resource(2, ["crate:old"]), resource(3, ["crate:old"], "pull_request")],
        )
        updates = migration.build_plan()["operations"]["resource_updates"]
        self.assertEqual([(item["number"], item["kind"]) for item in updates], [(2, "issue"), (3, "pull_request")])

    def test_paginated_label_and_open_closed_resource_inventory(self) -> None:
        filler_labels = [label(f"unrelated:{index:03}") for index in range(100)]
        resources = [resource(index, ["crate:old"]) for index in range(1, 102)]
        migration, transport = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha"},
            filler_labels + [label("crate:alpha", "bfdadc", "existing"), label("crate:old")],
            resources,
        )
        plan = migration.build_plan()
        self.assertEqual(plan["rollup"]["resources_affected"], 101)
        second_pages = [endpoint for method, endpoint, _ in transport.calls if method == "GET" and "page=2" in endpoint]
        self.assertGreaterEqual(len(second_pages), 2)

    def test_plan_and_digest_are_deterministic(self) -> None:
        migration, _ = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha"},
            [label("crate:alpha", "bfdadc", "existing"), label("crate:old")],
            [resource(5, ["z", "crate:old", "a"])],
        )
        first = migration.build_plan()
        second = migration.build_plan()
        self.assertEqual(first, second)
        self.assertEqual(migration_tool.plan_sha256(first), migration_tool.plan_sha256(second))


class ApplyAndAuditTests(unittest.TestCase):
    def test_apply_refuses_stale_inventory_before_any_write(self) -> None:
        migration, transport = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha"},
            [label("crate:alpha", "bfdadc", "existing"), label("crate:old")],
            [resource(1, ["crate:old", "unrelated"])],
        )
        plan = migration.build_plan()
        transport.drift_on_issue_inventory_read = 2
        with self.assertRaisesRegex(migration_tool.MigrationError, "inventory changed"):
            migration.apply(plan, migration_tool.plan_sha256(plan))
        self.assertEqual(transport.mutation_calls(), [])

    def test_apply_refuses_wrong_digest(self) -> None:
        migration, transport = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha"},
            [label("crate:old")],
            [],
        )
        plan = migration.build_plan()
        with self.assertRaisesRegex(migration_tool.MigrationError, "confirmation digest mismatch"):
            migration.apply(plan, "0" * 64)
        self.assertEqual(transport.mutation_calls(), [])

    def test_apply_rereads_uncertain_write_and_deletes_only_after_zero_references(self) -> None:
        migration, transport = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha", "crate:retired": None},
            [label("crate:alpha", "bfdadc", "existing"), label("crate:old"), label("crate:retired")],
            [resource(8, ["crate:old", "crate:retired", "phase:ready"])],
        )
        transport.uncertain.add(("PUT", f"repos/{REPOSITORY}/issues/8/labels"))
        plan = migration.build_plan()
        audit = migration.apply(plan, migration_tool.plan_sha256(plan))
        self.assertEqual(audit["status"], "ok")
        self.assertEqual(
            [entry["name"] for entry in transport.resources[8]["labels"]],
            ["crate:alpha", "phase:ready"],
        )
        mutations = transport.mutation_calls()
        put_index = next(index for index, call in enumerate(mutations) if call[0] == "PUT")
        delete_indices = [index for index, call in enumerate(mutations) if call[0] == "DELETE"]
        self.assertTrue(delete_indices and min(delete_indices) > put_index)
        reference_reads = [
            endpoint
            for method, endpoint, _ in transport.calls
            if method == "GET" and "labels=crate%3A" in endpoint
        ]
        self.assertGreaterEqual(len(reference_reads), 2)

    def test_uncertain_create_rename_and_delete_are_verified_without_retry(self) -> None:
        migration, transport = make_migration(
            ["aether-alpha", "aether-beta"],
            {"crate:aether-alpha": "crate:alpha", "crate:retired": None},
            [label("crate:aether-alpha"), label("crate:retired")],
            [],
        )
        transport.uncertain.update(
            {
                ("POST", f"repos/{REPOSITORY}/labels"),
                ("PATCH", f"repos/{REPOSITORY}/labels/crate%3Aaether-alpha"),
                ("DELETE", f"repos/{REPOSITORY}/labels/crate%3Aretired"),
            }
        )
        plan = migration.build_plan()
        audit = migration.apply(plan, migration_tool.plan_sha256(plan))
        self.assertEqual(audit["canonical_labels"], 2)
        methods = [call[0] for call in transport.mutation_calls()]
        self.assertEqual(methods.count("POST"), 1)
        self.assertEqual(methods.count("PATCH"), 1)
        self.assertEqual(methods.count("DELETE"), 1)

    def test_restart_after_partial_success_plans_only_remaining_work(self) -> None:
        migration, transport = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha"},
            [label("crate:alpha", "bfdadc", "aether-alpha workspace package"), label("crate:old")],
            [resource(1, ["crate:alpha"]), resource(2, ["crate:old", "keep"])],
        )
        plan = migration.build_plan()
        self.assertEqual(plan["operations"]["creates"], [])
        self.assertEqual([item["number"] for item in plan["operations"]["resource_updates"]], [2])
        migration.apply(plan, migration_tool.plan_sha256(plan))
        self.assertEqual([entry["name"] for entry in transport.resources[1]["labels"]], ["crate:alpha"])
        self.assertEqual([entry["name"] for entry in transport.resources[2]["labels"]], ["crate:alpha", "keep"])

    def test_resource_race_after_preflight_aborts_before_replacement(self) -> None:
        migration, transport = make_migration(
            ["aether-alpha"],
            {"crate:old": "crate:alpha"},
            [label("crate:alpha", "bfdadc", "existing"), label("crate:old")],
            [resource(4, ["crate:old", "keep"])],
        )
        plan = migration.build_plan()
        original_build = migration.build_plan

        def fresh_then_race() -> dict[str, object]:
            fresh = original_build()
            transport.resources[4]["labels"].append({"name": "concurrent"})
            return fresh

        migration.build_plan = fresh_then_race
        with self.assertRaisesRegex(migration_tool.MigrationError, "labels raced"):
            migration.apply(plan, migration_tool.plan_sha256(plan))
        puts = [call for call in transport.mutation_calls() if call[0] == "PUT"]
        self.assertEqual(puts, [])

    def test_audit_requires_exact_final_inventory(self) -> None:
        migration, _ = make_migration(
            ["aether-alpha"],
            {"crate:retired": None},
            [label("crate:alpha", "bfdadc", "existing"), label("crate:retired")],
            [],
        )
        with self.assertRaisesRegex(migration_tool.MigrationError, "final crate-label inventory mismatch"):
            migration.audit()

        clean, _ = make_migration(
            ["aether-alpha", "xtask"],
            {"crate:retired": None},
            [label("crate:alpha", "bfdadc", "existing"), label("crate:xtask", "bfdadc", "existing")],
            [],
        )
        self.assertEqual(clean.audit()["status"], "ok")


if __name__ == "__main__":
    unittest.main()
