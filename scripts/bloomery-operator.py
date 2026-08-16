#!/usr/bin/env python3
"""Diagnose and recover a Bloomery coordinator from one command line.

The coordinator already exposes everything an operator needs: a REST control
API for every override, and a SQLite journal that holds the dispatch state the
API does not project. What it did not have was a way to reach either without
hand-writing a throwaway query or hand-encoding a request body -- which is how
an hour disappears while a bloom sits wedged.

Read commands (status / orders / why / evidence) answer "what is stopping this
member". Action commands (hold / release / repair / supersede / grant /
adjudicate) are the coordinator's override routes with their bodies built
correctly. Repair names a commit (`--from-commit`) and the chassis derives the
candidate; `--tree` / `--checkout` stay for a pair the coordinator cannot read.

Standard library only -- this runs on the coordinator host, which installs
nothing.
"""

from __future__ import annotations

import argparse
import json
import os
import sqlite3
import struct
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path
from typing import Any, Iterable, Sequence


# The line's stage vocabulary, in the declaration order the wire encoding
# indexes by. Mirrors `StageId` in crates/aether-bloomery/src/ids.rs, whose
# `stage_vocabulary!` macro generates both the enum and `StageId::ALL` from one
# variant list. The `stage` column is that variant's index as a little-endian
# u32 (aether_data::wire encodes a unit variant as its u32 variant_index), so
# this list's ORDER is load-bearing: a stage inserted anywhere but the end
# renumbers every stage after it. Re-read ids.rs before editing this.
STAGES = (
    "Sketch",
    "Scope",
    "Approve",
    "Construct",
    "Verify",
    "Refine",
    "Review",
    "Integrate",
    "AggregateVerify",
    "AggregateReview",
    "Land",
    "Study",
    "Reconcile",
)

# The REST control API when nothing overrides it. The coordinator binds this on
# localhost from AETHER_HTTP_PORT.
DEFAULT_BASE = "http://127.0.0.1:8789"

# How long to wait on the coordinator before calling it unreachable. An override
# route admits a fact and answers from the reducer, so it is not instant.
HTTP_TIMEOUT_SECS = 30.0

# A digest is 32 bytes, spelled as 64 hex characters everywhere the API touches
# one -- in a path segment and in a body alike.
DIGEST_HEX_LEN = 64
DIGEST_BYTES = 32


class OperatorError(Exception):
    """A failure the operator should read as a sentence, not a traceback."""


def die(message: str) -> None:
    raise OperatorError(message)


def is_digest_hex(value: str) -> bool:
    """Is `value` the 64-character hex spelling of a 32-byte digest?

    The API refuses anything else with a 400, so catching it here turns a round
    trip and an opaque refusal into an immediate, specific message.
    """
    if len(value) != DIGEST_HEX_LEN:
        return False
    return all(character in "0123456789abcdefABCDEF" for character in value)


def require_digest(value: str, what: str) -> str:
    if not is_digest_hex(value):
        die(
            f"{what} must be {DIGEST_HEX_LEN} hex characters (a 32-byte digest); "
            f"got {len(value)} character(s): {value!r}"
        )
    return value.lower()


def stage_name(blob: bytes | None) -> str:
    """Render a stored `stage` column as its StageId name.

    The column holds the stage's canonical wire bytes: a unit enum variant, so a
    little-endian u32 of its index into `STAGES`. Anything else -- a short blob,
    an index past the vocabulary -- is rendered rather than raised, because a
    surprising stage should not cost the operator the rest of the table.
    """
    if blob is None:
        return "unknown"
    if len(blob) != 4:
        return f"unknown(len={len(blob)})"
    index = struct.unpack("<I", blob)[0]
    if index < len(STAGES):
        return STAGES[index]
    return f"unknown({index})"


def decode_transformation(blob: bytes | None) -> dict[str, Any] | None:
    """Recover `command`, `inputs`, and `checkout` from a stored transformation.

    The column is the Transformation as canonical wire bytes, whose leading
    fields are, in declaration order:

        u32 length + command bytes
        u32 count  + count * 32-byte input digests
        32-byte checkout

    Everything past `checkout` (diff_base, outputs, image, limits, ...) is left
    unread -- those fields are not what an operator recovering a candidate
    needs, and not reading them means a change to them cannot break this.

    Returns None on any layout surprise. The caller degrades to "unavailable":
    a transformation this cannot parse is a reason to fall back to `--tree` and
    `--checkout` by hand, never a reason to crash the command that was going to
    tell the operator what is wrong.
    """
    if not blob:
        return None
    try:
        offset = 0

        if len(blob) < offset + 4:
            return None
        (command_len,) = struct.unpack_from("<I", blob, offset)
        offset += 4
        if len(blob) < offset + command_len:
            return None
        command = blob[offset : offset + command_len].decode("utf-8", errors="replace")
        offset += command_len

        if len(blob) < offset + 4:
            return None
        (input_count,) = struct.unpack_from("<I", blob, offset)
        offset += 4
        # A plausibility bound before trusting a count into an allocation: an
        # order carries a handful of pinned inputs, so a huge count means the
        # layout drifted, not that a lane pinned a million digests.
        if input_count > 1024:
            return None
        if len(blob) < offset + input_count * DIGEST_BYTES:
            return None
        inputs = [
            blob[offset + index * DIGEST_BYTES : offset + (index + 1) * DIGEST_BYTES].hex()
            for index in range(input_count)
        ]
        offset += input_count * DIGEST_BYTES

        if len(blob) < offset + DIGEST_BYTES:
            return None
        checkout = blob[offset : offset + DIGEST_BYTES].hex()
    except (struct.error, UnicodeError):
        return None

    return {"command": command, "inputs": inputs, "checkout": checkout}


def overdue_secs(deadline_unix_millis: int, now_unix_millis: int) -> float:
    """Seconds this order is past its deadline.

    Positive is overdue -- the attempt outlived its sealed wall-clock allowance
    and is waiting on the reaper. Negative is time remaining, so the order is
    still inside its window and the right move is to wait, not to intervene.
    The sign is the whole diagnosis, which is why this returns one signed number
    rather than a flag plus a magnitude.
    """
    return (now_unix_millis - deadline_unix_millis) / 1000.0


def human_secs(seconds: float) -> str:
    """A signed seconds count as something readable at 2am."""
    sign = "-" if seconds < 0 else "+"
    remaining = abs(seconds)
    if remaining < 90:
        return f"{sign}{remaining:.0f}s"
    if remaining < 5400:
        return f"{sign}{remaining / 60:.1f}m"
    return f"{sign}{remaining / 3600:.1f}h"


def now_unix_millis() -> int:
    return int(time.time() * 1000)


class Api:
    """The coordinator's REST control API."""

    def __init__(self, base: str) -> None:
        self.base = base.rstrip("/")

    def _request(self, method: str, path: str, body: dict[str, Any] | None = None) -> Any:
        url = f"{self.base}{path}"
        data = None
        headers = {"Accept": "application/json"}
        if body is not None:
            data = json.dumps(body).encode("utf-8")
            headers["Content-Type"] = "application/json"
        request = urllib.request.Request(url, data=data, headers=headers, method=method)
        try:
            with urllib.request.urlopen(request, timeout=HTTP_TIMEOUT_SECS) as response:
                raw = response.read()
        except urllib.error.HTTPError as error:
            detail = error.read().decode("utf-8", errors="replace").strip()
            die(f"{method} {url} answered {error.code}: {detail or error.reason}")
        except urllib.error.URLError as error:
            die(
                f"cannot reach the coordinator at {self.base} ({error.reason}). "
                f"Is it running, and is --base / AETHER_HTTP_PORT pointed at its REST port?"
            )
        except TimeoutError:
            die(f"{method} {url} timed out after {HTTP_TIMEOUT_SECS:.0f}s")
        if not raw:
            return None
        try:
            return json.loads(raw)
        except json.JSONDecodeError:
            die(f"{method} {url} answered content that is not JSON: {raw[:200]!r}")

    def view(self) -> Any:
        return self._request("GET", "/view")

    def journal(self) -> Any:
        return self._request("GET", "/journal")

    def post(self, path: str, body: dict[str, Any]) -> Any:
        return self._request("POST", path, body)


class Journal:
    """Read-only access to the coordinator's SQLite journal.

    Opened through a `mode=ro` URI so this tool cannot write to the file the
    coordinator is actively journaling into -- not even by creating it, which a
    plain path would do silently and leave the operator reading an empty store
    while the real one sat elsewhere.
    """

    def __init__(self, path: str) -> None:
        self.path = path
        if path != ":memory:" and not Path(path).exists():
            die(
                f"no journal at {path}. Point --journal (or AETHER_STORE_PATH) at the "
                f"coordinator's store file."
            )
        uri = f"file:{Path(path).as_posix()}?mode=ro"
        try:
            self.conn = sqlite3.connect(uri, uri=True, timeout=5.0)
        except sqlite3.Error as error:
            die(f"cannot open the journal at {path} read-only: {error}")
        self.conn.row_factory = sqlite3.Row

    def outstanding_orders(self) -> list[sqlite3.Row]:
        try:
            return list(
                self.conn.execute(
                    "SELECT nonce, bloom, workpiece, scope_revision, candidate, displayed_digest, "
                    "stage, transformation, configs, profile, deadline_unix_millis "
                    "FROM outstanding_orders ORDER BY deadline_unix_millis"
                )
            )
        except sqlite3.Error as error:
            die(f"cannot read outstanding_orders from {self.path}: {error}")

    def orders_for(self, workpiece: str) -> list[sqlite3.Row]:
        return [row for row in self.outstanding_orders() if row["workpiece"] == workpiece]


def order_summary(row: sqlite3.Row, now_millis: int) -> dict[str, Any]:
    """One outstanding order as the fields an operator reads."""
    transformation = decode_transformation(row["transformation"])
    overdue = overdue_secs(row["deadline_unix_millis"], now_millis)
    return {
        "nonce": row["nonce"],
        "bloom": bytes(row["bloom"]).hex(),
        "workpiece": row["workpiece"],
        "stage": stage_name(row["stage"]),
        "candidate": bytes(row["candidate"]).hex(),
        "deadline_unix_millis": row["deadline_unix_millis"],
        "overdue_secs": round(overdue, 1),
        "command": transformation["command"] if transformation else "unavailable",
        "checkout": transformation["checkout"] if transformation else "unavailable",
    }


SLOT_PREFIX = "slot-"
EVIDENCE_SUFFIX = "-evidence"
QUARANTINE_SUFFIX = ".quarantine"
IDENTITY_RECORD = "identity"
SLOT_RECORD = "slot"


def evidence_path(worktree_base: str, nonce: str) -> Path:
    """Where a dispatch's lane wrote its evidence.

    The coordinator writes each run's evidence to `<base>/<nonce>-evidence`, and
    a nonce is itself spelled `dispatch-<sequence>`. An operator reading a nonce
    off the orders table has the full spelling; one reading a sequence out of a
    log has the number, so a bare number is accepted and completed.
    """
    base = Path(worktree_base)
    if not nonce.startswith("dispatch-"):
        nonce = f"dispatch-{nonce}"
    return base / f"{nonce}{EVIDENCE_SUFFIX}" / "evidence.json"


def slot_quarantine_path(worktree_base: str, slot: int) -> Path:
    """The sibling of `slot-<n>` that withholds it from allocation."""
    return Path(worktree_base) / f"{SLOT_PREFIX}{slot}{QUARANTINE_SUFFIX}"


def slot_index_from_name(name: str) -> int | None:
    """`slot-12` or `slot-12.quarantine` as its index, else None."""
    rest = name.removeprefix(SLOT_PREFIX)
    rest = rest.removesuffix(QUARANTINE_SUFFIX)
    if not rest or not rest.isdigit():
        return None
    return int(rest)


def read_json_object(path: Path) -> dict[str, Any] | None:
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return None
    return value if isinstance(value, dict) else None


def observe_process(pid: int) -> dict[str, Any] | None:
    """Live `/proc` identity for `pid`, or None when the pid is gone."""
    try:
        stat = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
        boot_id = Path("/proc/sys/kernel/random/boot_id").read_text(encoding="utf-8").strip()
    except OSError:
        return None
    closing = stat.rfind(")")
    if closing < 0:
        return None
    fields = stat[closing + 1 :].split()
    if len(fields) < 20:
        return None
    try:
        pgid = int(fields[2])
        starttime = int(fields[19])
    except ValueError:
        return None
    return {"pid": pid, "pgid": pgid, "starttime": starttime, "boot_id": boot_id}


def identity_matches(recorded: dict[str, Any], live: dict[str, Any]) -> bool:
    return recorded.get("starttime") == live.get("starttime") and recorded.get("boot_id") == live.get("boot_id")


def list_slots(worktree_base: str) -> list[dict[str, Any]]:
    """Every slot visible under the scratch root, with occupancy and quarantine.

    Occupied means a dispatch's `slot` record still names it. Quarantined
    means a sibling `slot-<n>.quarantine` file is present — the child could
    not be terminated and the next dispatch must not build there. A slot can
    be both: a live re-adopted run whose cancel just failed.
    """
    base = Path(worktree_base)
    if not base.is_dir():
        die(
            f"no worktree base at {base}. Pass --worktree-base or set "
            "AETHER_GITHUB_LOCAL_WORKTREE_BASE to the directory the coordinator "
            "checks lanes out under."
        )

    occupants: dict[int, str] = {}
    identities: dict[int, dict[str, Any]] = {}
    for entry in base.iterdir():
        if not entry.is_dir() or not entry.name.endswith(EVIDENCE_SUFFIX):
            continue
        nonce = entry.name[: -len(EVIDENCE_SUFFIX)]
        if not nonce:
            continue
        slot_body = (entry / SLOT_RECORD).read_text(encoding="utf-8").strip() if (entry / SLOT_RECORD).is_file() else ""
        if not slot_body.isdigit():
            continue
        slot = int(slot_body)
        occupants[slot] = nonce
        identity = read_json_object(entry / IDENTITY_RECORD)
        if identity is not None:
            identities[slot] = identity

    quarantines: dict[int, dict[str, Any]] = {}
    checkout_slots: set[int] = set()
    for entry in base.iterdir():
        index = slot_index_from_name(entry.name)
        if index is None:
            continue
        if entry.name.endswith(QUARANTINE_SUFFIX) and entry.is_file():
            record = read_json_object(entry) or {}
            quarantines[index] = record
        elif entry.is_dir():
            checkout_slots.add(index)

    slots = sorted(checkout_slots | set(occupants) | set(quarantines))
    listed = []
    for slot in slots:
        quarantine = quarantines.get(slot)
        occupant = occupants.get(slot)
        if quarantine is not None:
            state = "quarantined"
        elif occupant is not None:
            state = "occupied"
        else:
            state = "free"
        identity = None
        if quarantine is not None and isinstance(quarantine.get("identity"), dict):
            identity = quarantine["identity"]
        elif slot in identities:
            identity = identities[slot]
        listed.append(
            {
                "slot": slot,
                "state": state,
                "nonce": (quarantine or {}).get("nonce") or occupant,
                "identity": identity,
                "quarantine": quarantine,
            }
        )
    return listed


def clear_quarantine(worktree_base: str, slot: int) -> dict[str, Any]:
    """Remove a named slot's quarantine after stating what was checked.

    Clearing is an operator assertion, not an inference: a matching process
    that is still live is reported and the file is still removed. The
    coordinator reads the file on the next reserve, so the slot returns to
    the allocator on the operator's word.
    """
    if not worktree_base:
        die(
            "no worktree base. Pass --worktree-base or set AETHER_GITHUB_LOCAL_WORKTREE_BASE "
            "to the directory the coordinator checks lanes out under."
        )
    path = slot_quarantine_path(worktree_base, slot)
    if not path.is_file():
        die(f"slot {slot} is not quarantined (no file at {path})")
    record = read_json_object(path) or {}
    identity = record.get("identity") if isinstance(record.get("identity"), dict) else None
    matching_process_live = False
    observed = None
    if identity is not None and isinstance(identity.get("pid"), int):
        observed = observe_process(identity["pid"])
        matching_process_live = observed is not None and identity_matches(identity, observed)
    try:
        path.unlink()
    except OSError as error:
        die(f"could not remove {path}: {error}")
    return {
        "slot": slot,
        "cleared": True,
        "path": str(path),
        "nonce": record.get("nonce"),
        "recorded_identity": identity,
        "observed_identity": observed,
        "matching_process_live": matching_process_live,
        "checked": (
            f"/proc/{identity['pid']}/stat starttime+boot_id"
            if identity is not None and "pid" in identity
            else "no recorded identity to compare"
        ),
        "on_operator_word": (
            "a matching process is still live; the slot is released on your word"
            if matching_process_live
            else "no matching process is live; the slot is released on your word"
        ),
    }


def read_evidence(worktree_base: str, nonce: str) -> dict[str, Any]:
    path = evidence_path(worktree_base, nonce)
    if not path.exists():
        die(
            f"no evidence at {path}. The lane may still be running, its directory may have "
            f"been reaped, or --worktree-base (AETHER_GITHUB_LOCAL_WORKTREE_BASE) points elsewhere."
        )
    try:
        value = json.loads(path.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError) as error:
        die(f"cannot read {path}: {error}")
    if not isinstance(value, dict):
        die(f"{path} is not a JSON object")
    return value


def evidence_summary(value: dict[str, Any]) -> dict[str, Any]:
    """The lane-result columns, from the run's own evidence.

    `cost_usd` is the runner's claim about itself and is reported as such --
    Bloomery prices an attempt from a sealed table, never from this field.
    """
    record = value.get("result_record")
    record = record if isinstance(record, dict) else {}
    result = record.get("result")
    result = result if isinstance(result, dict) else {}
    duration_millis = record.get("duration_ms")
    return {
        "command": value.get("command", "unstated"),
        "status": value.get("status", "unstated"),
        "produced_candidate": bool(value.get("produced_candidate", False)),
        "is_error": record.get("is_error"),
        "turns": record.get("num_turns"),
        "duration_secs": round(duration_millis / 1000.0, 1) if isinstance(duration_millis, (int, float)) else None,
        "output_tokens": record.get("output"),
        "reported_cost_usd": record.get("cost_usd"),
        "result_text": result.get("result"),
    }


def members_of(view: Any) -> Iterable[tuple[dict[str, Any], dict[str, Any]]]:
    """Every (bloom, member) pair in a view document."""
    for bloom in (view or {}).get("blooms", []):
        for member in bloom.get("members", []):
            yield bloom, member


def find_member(view: Any, workpiece: str) -> tuple[dict[str, Any], dict[str, Any]]:
    for bloom, member in members_of(view):
        if member.get("workpiece") == workpiece:
            return bloom, member
    known = sorted({member.get("workpiece", "?") for _, member in members_of(view)})
    die(
        f"no member named {workpiece!r} in any live bloom. Known members: "
        + (", ".join(known) if known else "(none -- no bloom is live)")
    )


def member_status_state(member: dict[str, Any], *, has_order: bool) -> str:
    """The one-word state the status table prints for a member.

    A dependent that has not entered the line carries `blocked_by` on the
    bloom view (ADR-0196). Painting that as `idle` is the mysterious
    idleness the readiness scheduler exists to name: the member is held,
    not forgotten.
    """
    if member.get("wedge"):
        return "WEDGED"
    if member.get("pending_decision"):
        return "held"
    if member.get("resolution"):
        return "integrated"
    if has_order:
        return "running"
    if member.get("blocked_by"):
        return "blocked"
    return "idle"


def blocked_by_of(member: dict[str, Any]) -> str | None:
    """The ancestor the bloom view names as holding this member out of the line."""
    blocked = member.get("blocked_by")
    if isinstance(blocked, str) and blocked:
        return blocked
    return None


# The reducer outcomes that move a member along the line, and the field each
# names its stage with. Read newest-first, the first hit is where the member
# actually is -- the cursor is reducer state that the view document does not
# project, so the journal is the only place to recover it from.
CURSOR_OUTCOMES = {
    "AttemptAdvanced": "to",
    "AttemptRetried": "stage",
    "AttemptWedged": "stage",
}


def last_movement(journal_records: Sequence[dict[str, Any]], workpiece: str) -> dict[str, Any] | None:
    """The newest journaled outcome that moved `workpiece`, with its stage."""
    for record in reversed(journal_records):
        outcome = record.get("outcome")
        if not isinstance(outcome, dict):
            continue
        for name, payload in outcome.items():
            if not isinstance(payload, dict) or payload.get("workpiece") != workpiece:
                continue
            movement = {
                "sequence": record.get("sequence"),
                "outcome": name,
                "attempt": payload.get("attempt"),
                "rolls": payload.get("rolls"),
            }
            field = CURSOR_OUTCOMES.get(name)
            movement["stage"] = payload.get(field) if field else None
            return movement
    return None


def print_json(value: Any) -> None:
    json.dump(value, sys.stdout, indent=2, sort_keys=False)
    sys.stdout.write("\n")


def cmd_status(args: argparse.Namespace) -> None:
    view = Api(args.base).view()
    journal = Journal(args.journal)
    now = now_unix_millis()
    orders = {}
    for row in journal.outstanding_orders():
        orders.setdefault(row["workpiece"], order_summary(row, now))

    report = {
        "mainline": view.get("mainline"),
        "observed": view.get("observed"),
        "blooms": [],
    }
    for bloom in view.get("blooms", []):
        entry = {
            "id": bloom.get("id"),
            "status": bloom.get("status"),
            "superseded_by": bloom.get("superseded_by"),
            "landing_blocked": bloom.get("landing_blocked"),
            "executor_fault": bloom.get("executor_fault"),
            "members": [],
        }
        for member in bloom.get("members", []):
            workpiece = member.get("workpiece")
            order = orders.get(workpiece)
            wedge = member.get("wedge")
            resolution = member.get("resolution")
            pending = member.get("pending_decision")
            blocked_by = blocked_by_of(member)
            entry["members"].append(
                {
                    "workpiece": workpiece,
                    "cursor": order["stage"] if order else (wedge or {}).get("stage"),
                    "cursor_source": "outstanding order" if order else ("wedge" if wedge else "none"),
                    "candidate": (resolution or {}).get("candidate"),
                    "integrated": resolution is not None,
                    "wedged_at": (wedge or {}).get("stage"),
                    "wedge_evidence": (wedge or {}).get("evidence"),
                    "held_on_question": (pending or {}).get("question"),
                    "outstanding_nonce": order["nonce"] if order else None,
                    "blocked_by": blocked_by,
                    "state": member_status_state(member, has_order=order is not None),
                }
            )
        report["blooms"].append(entry)

    if args.json:
        print_json(report)
        return

    print(f"mainline {report['mainline']}")
    print(f"observed {report['observed']}")
    if not report["blooms"]:
        print("no blooms are live")
        return
    for bloom in report["blooms"]:
        print()
        line = f"bloom {bloom['id']}  status={bloom['status']}"
        if bloom["superseded_by"]:
            line += f"  superseded_by={bloom['superseded_by']}"
        print(line)
        if bloom["landing_blocked"]:
            block = bloom["landing_blocked"]
            print(f"  landing blocked: {block.get('rolls')}/{block.get('budget')} refused")
        if bloom["executor_fault"]:
            fault = bloom["executor_fault"]
            terminal = " TERMINAL" if fault.get("terminal") else ""
            print(f"  executor fault: {fault.get('rolls')}/{fault.get('budget')}{terminal}")
        print(f"  {'MEMBER':<28} {'CURSOR':<17} {'STATE':<12} CANDIDATE")
        for member in bloom["members"]:
            candidate = member["candidate"] or "-"
            cursor = member["cursor"] or "-"
            line = f"  {member['workpiece']:<28} {cursor:<17} {member['state']:<12} {candidate}"
            if member["blocked_by"]:
                line += f"  blocked by {member['blocked_by']}"
            print(line)


def cmd_orders(args: argparse.Namespace) -> None:
    journal = Journal(args.journal)
    now = now_unix_millis()
    orders = [order_summary(row, now) for row in journal.outstanding_orders()]

    if args.json:
        print_json(orders)
        return

    if not orders:
        print("no outstanding orders")
        return
    print(f"{'NONCE':<16} {'WORKPIECE':<26} {'STAGE':<17} {'OVERDUE':>9}  CANDIDATE")
    for order in orders:
        overdue = human_secs(order["overdue_secs"])
        print(
            f"{order['nonce']:<16} {order['workpiece']:<26} {order['stage']:<17} "
            f"{overdue:>9}  {order['candidate']}"
        )
    print()
    print("OVERDUE is signed: + is past its deadline (waiting on the reaper), - is time still left.")


def cmd_why(args: argparse.Namespace) -> None:
    api = Api(args.base)
    bloom, member = find_member(api.view(), args.workpiece)
    journal = Journal(args.journal)
    now = now_unix_millis()
    orders = [order_summary(row, now) for row in journal.orders_for(args.workpiece)]
    records = (api.journal() or {}).get("records", [])
    movement = last_movement(records, args.workpiece)

    wedge = member.get("wedge")
    resolution = member.get("resolution")
    pending = member.get("pending_decision")
    blocked_by = blocked_by_of(member)
    order = orders[0] if orders else None

    lane = None
    if order and args.worktree_base:
        path = evidence_path(args.worktree_base, order["nonce"])
        if path.exists():
            lane = evidence_summary(read_evidence(args.worktree_base, order["nonce"]))

    diagnosis = {
        "workpiece": args.workpiece,
        "bloom": bloom.get("id"),
        "bloom_status": bloom.get("status"),
        "cursor": order["stage"] if order else (movement or {}).get("stage"),
        "cursor_source": "outstanding order" if order else "journal",
        "attempt": (movement or {}).get("attempt"),
        # The per-stage retry budget lives in the bloom's sealed stage catalog,
        # which no read route projects -- so the attempt count is reported
        # without the ceiling it is counting toward rather than guessed at.
        "attempt_budget": "unavailable (the sealed catalog is not served by the API)",
        "last_movement": movement,
        "candidate": (resolution or {}).get("candidate"),
        "has_candidate": resolution is not None,
        "outstanding_order": order,
        "wedge": wedge,
        "held_on_question": pending,
        "blocked_by": blocked_by,
        "last_lane_result": lane,
    }

    if args.json:
        print_json(diagnosis)
        return

    print(f"workpiece {args.workpiece}")
    print(f"  bloom      {diagnosis['bloom']} ({diagnosis['bloom_status']})")
    print(f"  cursor     {diagnosis['cursor'] or 'unknown'}  (from the {diagnosis['cursor_source']})")
    attempt = diagnosis["attempt"]
    print(f"  attempts   {attempt if attempt is not None else 'none recorded'} / {diagnosis['attempt_budget']}")
    if movement:
        print(f"  last move  #{movement['sequence']} {movement['outcome']} at {movement['stage'] or '-'}")
    else:
        print("  last move  nothing in the journal names this member")
    print(f"  candidate  {diagnosis['candidate'] or 'none -- no candidate has been captured'}")

    if order:
        print(
            f"  order      {order['nonce']} at {order['stage']}, "
            f"deadline {human_secs(order['overdue_secs'])} ({order['command']})"
        )
        if order["overdue_secs"] > 0:
            print("             ^ PAST its deadline: the attempt outlived its allowance.")
        else:
            print("             ^ still inside its window: this member is waiting, not stuck.")
    else:
        print("  order      none outstanding -- nothing is dispatched for this member right now")

    if wedge:
        print(f"  WEDGE      at {wedge.get('stage')}, evidence {wedge.get('evidence')}")
        print("             recovery: `grant` for another attempt, or `repair` with a candidate you built.")
    if blocked_by:
        ancestor = next((other for other in bloom.get("members", []) if other.get("workpiece") == blocked_by), None)
        if ancestor and ancestor.get("wedge"):
            reason = f"which is wedged at {(ancestor.get('wedge') or {}).get('stage')}"
        else:
            reason = "which has not resolved yet"
        print(f"  BLOCKED    by {blocked_by}, {reason}")
        print("             construct waits for that ancestor; this member has not entered the line.")
    if pending:
        print(f"  HELD       on question {pending.get('question')} at {pending.get('stage')}")
        print(f"             {pending.get('prompt')}")
    if lane:
        print(
            f"  last lane  {lane['command']} status={lane['status']} turns={lane['turns']} "
            f"candidate={lane['produced_candidate']}"
        )
        if lane["result_text"]:
            print(f"             {str(lane['result_text']).strip().splitlines()[0][:160]}")
    elif order:
        print(f"  last lane  no evidence.json yet for {order['nonce']} (still running, or reaped)")


def cmd_evidence(args: argparse.Namespace) -> None:
    if not args.worktree_base:
        die(
            "no worktree base. Pass --worktree-base or set AETHER_GITHUB_LOCAL_WORKTREE_BASE "
            "to the directory the coordinator checks lanes out under."
        )
    summary = evidence_summary(read_evidence(args.worktree_base, args.nonce))

    if args.json:
        print_json(summary)
        return

    print(f"nonce      {args.nonce}")
    print(f"command    {summary['command']}")
    print(f"status     {summary['status']}  is_error={summary['is_error']}")
    print(f"turns      {summary['turns']}")
    print(f"duration   {summary['duration_secs']}s")
    print(f"output     {summary['output_tokens']} tokens")
    print(f"cost       {summary['reported_cost_usd']} usd (the runner's own claim, not a priced figure)")
    print(f"candidate  {summary['produced_candidate']}")
    print("result:")
    text = summary["result_text"]
    if text is None:
        print("  (the run recorded no result text)")
    else:
        for line in str(text).splitlines() or [""]:
            print(f"  {line}")


def cmd_slots(args: argparse.Namespace) -> None:
    if not args.worktree_base:
        die(
            "no worktree base. Pass --worktree-base or set AETHER_GITHUB_LOCAL_WORKTREE_BASE "
            "to the directory the coordinator checks lanes out under."
        )
    listed = list_slots(args.worktree_base)
    if args.json:
        print_json(listed)
        return
    if not listed:
        print("no lane slots under this scratch root")
        return
    print(f"{'SLOT':<8} {'STATE':<12} {'NONCE':<20} IDENTITY")
    for entry in listed:
        identity = entry["identity"] or {}
        if identity:
            rendered = (
                f"pid={identity.get('pid')} pgid={identity.get('pgid')} "
                f"starttime={identity.get('starttime')} boot={identity.get('boot_id')}"
            )
        else:
            rendered = "-"
        print(f"{entry['slot']:<8} {entry['state']:<12} {str(entry['nonce'] or '-'):<20} {rendered}")


def cmd_clear_quarantine(args: argparse.Namespace) -> None:
    result = clear_quarantine(args.worktree_base, args.slot)
    if args.json:
        print_json(result)
        return
    print(f"cleared quarantine on slot {result['slot']}")
    print(f"  path     {result['path']}")
    print(f"  nonce    {result['nonce'] or '-'}")
    print(f"  checked  {result['checked']}")
    print(f"  live     {result['matching_process_live']}")
    print(f"  {result['on_operator_word']}")


def override_body(args: argparse.Namespace, **extra: Any) -> dict[str, Any]:
    """A body for a route that refuses a blank reason or operator."""
    if not args.reason.strip():
        die("--reason must say something; the API refuses a blank one rather than defaulting it")
    if not args.operator.strip():
        die("--operator must name who is making this override")
    body: dict[str, Any] = {"reason": args.reason, "operator": args.operator}
    if getattr(args, "idempotency_key", None):
        body["idempotency_key"] = args.idempotency_key
    body.update(extra)
    return body


def emit_outcome(result: Any) -> None:
    """Print an action route's JSON outcome. A non-2xx already raised."""
    print_json(result)


def cmd_hold(args: argparse.Namespace) -> None:
    bloom = require_digest(args.bloom, "the bloom id")
    emit_outcome(Api(args.base).post(f"/blooms/{bloom}/hold", override_body(args)))


def cmd_release(args: argparse.Namespace) -> None:
    bloom = require_digest(args.bloom, "the bloom id")
    emit_outcome(Api(args.base).post(f"/blooms/{bloom}/release", override_body(args)))


def repair_payload(
    tree: str | None,
    checkout: str | None,
    from_commit: str | None,
    from_worktree: str | None,
) -> dict[str, Any]:
    """The extra fields a repair body carries, or die naming the contract.

    The chassis derives digests from a commit; this script must not re-state
    that scheme. The low-level pair stays for a candidate the coordinator
    cannot read.
    """
    commit = (from_commit or "").strip() or None
    worktree = (from_worktree or "").strip() or None
    has_tree = bool(tree)
    has_checkout = bool(checkout)
    named = sum([bool(commit), bool(worktree), has_tree or has_checkout])
    if named != 1:
        die(
            "repair needs exactly one of --from-commit, --from-worktree, or "
            "both --tree and --checkout"
        )
    if has_tree or has_checkout:
        if not (has_tree and has_checkout):
            die("the low-level form needs both --tree and --checkout")
        # The candidate is a CandidateRef STRUCT, never a bare digest string: the
        # returned evidence binds the tree and the verifying lane checks out the
        # commit, so a string leaves the gate unable to do half its job and the
        # route answers `400 invalid repair body: candidate: invalid type: string`.
        return {
            "candidate": {
                "tree": require_digest(tree or "", "--tree"),
                "checkout": require_digest(checkout or "", "--checkout"),
            }
        }
    if commit:
        return {"from_commit": commit}
    return {"from_worktree": worktree or ""}


def cmd_repair(args: argparse.Namespace) -> None:
    bloom = require_digest(args.bloom, "the bloom id")
    extra = repair_payload(args.tree, args.checkout, args.from_commit, args.from_worktree)
    if "candidate" not in extra:
        require_repairable(Api(args.base), args.workpiece)
    emit_outcome(Api(args.base).post(f"/blooms/{bloom}/members/{args.workpiece}/repair", override_body(args, **extra)))


COMPOSITION_WORKPIECE = "aether.bloomery.composition"


def require_repairable(api: Api, workpiece: str) -> None:
    """Refuse a from-commit repair of a member that is not wedged.

    The chassis reducer still refuses the same case after host-side work; this
    check is the one that names the precondition *before* a candidate ref is
    force-pushed. The composition is not a member in the view, so it is left
    to the reducer — the only authority that can see its wedge.
    """
    if workpiece == COMPOSITION_WORKPIECE:
        return
    _, member = find_member(api.view(), workpiece)
    if not member.get("wedge"):
        die(
            f"{workpiece} is not wedged, so it is not repairable. "
            "A running member already holds a dispatched attempt."
        )


def cmd_supersede(args: argparse.Namespace) -> None:
    bloom = require_digest(args.bloom, "the predecessor bloom id")
    body: dict[str, Any] = {"successor_draft": args.draft}
    if args.idempotency_key:
        body["idempotency_key"] = args.idempotency_key
    emit_outcome(Api(args.base).post(f"/blooms/{bloom}/supersede", body))


def cmd_grant(args: argparse.Namespace) -> None:
    bloom = require_digest(args.bloom, "the bloom id")
    stage = args.stage
    if stage is None:
        # The reducer refuses a grant naming any stage but the one the member is
        # wedged at, so resolving it from the wedge beats making the operator
        # retype what the view already knows.
        _, member = find_member(Api(args.base).view(), args.workpiece)
        wedge = member.get("wedge")
        if not wedge:
            die(
                f"{args.workpiece} is not wedged, so there is no stage to grant against. "
                f"Pass --stage explicitly if you mean to."
            )
        stage = wedge.get("stage")
    if stage not in STAGES:
        die(f"--stage must be one of: {', '.join(STAGES)}")
    body: dict[str, Any] = {"workpiece": args.workpiece, "stage": stage, "attempts": args.attempts}
    if args.idempotency_key:
        body["idempotency_key"] = args.idempotency_key
    emit_outcome(Api(args.base).post(f"/blooms/{bloom}/grant", body))


def cmd_adjudicate(args: argparse.Namespace) -> None:
    bloom = require_digest(args.bloom, "the bloom id")
    findings = [require_digest(finding, "--finding") for finding in args.finding]
    if args.defer is not None:
        if args.defer == 0:
            die("--defer must name a real issue number; issue 0 is no issue and the API refuses it")
        disposition: Any = {"Deferred": {"issue": args.defer}}
    else:
        disposition = "Accepted"
    body = override_body(args, findings=findings, disposition=disposition)
    emit_outcome(Api(args.base).post(f"/blooms/{bloom}/adjudicate", body))


EPILOG = """\
worked example -- recovering a wedged member
--------------------------------------------------------------------------
  # 1. what is stopping it?
  bloomery-operator.py why issue-4931

  # 2. freeze the bloom while you take the lap yourself
  bloomery-operator.py hold <bloom-id> \\
      --reason "taking the construct lap by hand" --operator eve

  # 3. name the commit that holds the fix. The chassis derives both digests,
  #    pushes the candidate ref, and records correspondence. --tree/--checkout
  #    stay for a candidate the coordinator cannot read.
  bloomery-operator.py repair <bloom-id> issue-4931 \\
      --from-commit <sha> \\
      --reason "hand-built the fix; model lane could not" --operator eve

  # 4. let it run again
  bloomery-operator.py release <bloom-id> --reason "repair supplied" --operator eve

is it wedged, or just slow?
--------------------------------------------------------------------------
  bloomery-operator.py orders

  OVERDUE is signed. `-4.2m` means the attempt still has four minutes of its
  sealed allowance left: wait. `+51.0m` means it outlived the allowance and is
  waiting on the reaper: that is the one worth acting on.
"""


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        prog="bloomery-operator.py",
        description="Diagnose and recover a Bloomery coordinator: read its state, then override it.",
        epilog=EPILOG,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    default_port = os.environ.get("AETHER_HTTP_PORT")
    default_base = os.environ.get("AETHER_BLOOMERY_API") or (
        f"http://127.0.0.1:{default_port}" if default_port else DEFAULT_BASE
    )
    parser.add_argument(
        "--base",
        default=default_base,
        help=f"REST control API base URL (env AETHER_BLOOMERY_API, or AETHER_HTTP_PORT on localhost) [{default_base}]",
    )
    parser.add_argument(
        "--journal",
        default=os.environ.get("AETHER_STORE_PATH", ""),
        help="path to the coordinator's SQLite journal, opened read-only (env AETHER_STORE_PATH)",
    )
    parser.add_argument(
        "--worktree-base",
        default=os.environ.get("AETHER_GITHUB_LOCAL_WORKTREE_BASE", ""),
        help="directory the coordinator checks lanes out under, where lane evidence lives "
        "(env AETHER_GITHUB_LOCAL_WORKTREE_BASE)",
    )
    parser.add_argument("--json", action="store_true", help="emit raw JSON instead of the operator table")

    subparsers = parser.add_subparsers(dest="command", required=True)

    status = subparsers.add_parser("status", help="every bloom and member: status, cursor, candidate, wedge")
    status.set_defaults(handler=cmd_status)

    orders = subparsers.add_parser("orders", help="outstanding orders with their deadlines and overdue seconds")
    orders.set_defaults(handler=cmd_orders)

    why = subparsers.add_parser("why", help="one-shot diagnosis of a member that is not moving")
    why.add_argument("workpiece")
    why.set_defaults(handler=cmd_why)

    evidence = subparsers.add_parser("evidence", help="the lane result summary for one dispatch nonce")
    evidence.add_argument("nonce", help="e.g. dispatch-1746, or just 1746")
    evidence.set_defaults(handler=cmd_evidence)

    slots = subparsers.add_parser("slots", help="lane slots: occupied, free, or quarantined, with the identity that caused it")
    slots.set_defaults(handler=cmd_slots)

    clear_q = subparsers.add_parser(
        "clear-quarantine",
        help="clear a named slot quarantine after you have confirmed its child is gone",
    )
    clear_q.add_argument("slot", type=int, help="the slot index, e.g. 0 for slot-0")
    clear_q.set_defaults(handler=cmd_clear_quarantine)

    def with_override(subparser: argparse.ArgumentParser) -> argparse.ArgumentParser:
        subparser.add_argument("--reason", required=True, help="why (the API refuses a blank reason)")
        subparser.add_argument("--operator", required=True, help="who (the API refuses a blank operator)")
        subparser.add_argument("--idempotency-key", default=None, help="override the content-derived admit key")
        return subparser

    hold = with_override(subparsers.add_parser("hold", help="freeze a bloom's dispatch"))
    hold.add_argument("bloom")
    hold.set_defaults(handler=cmd_hold)

    release = with_override(subparsers.add_parser("release", help="take a bloom off the brake"))
    release.add_argument("bloom")
    release.set_defaults(handler=cmd_release)

    repair = with_override(
        subparsers.add_parser(
            "repair",
            help="hand a wedged member a candidate you built yourself",
            description="Name the fix as --from-commit <sha> (or --from-worktree <path>) and the "
            "chassis derives the candidate, pushes the ref, and records correspondence. "
            "The low-level form still takes BOTH --tree and --checkout for a candidate "
            "the coordinator cannot read. Sending one digest is refused with "
            "`400 invalid repair body`.",
        )
    )
    repair.add_argument("bloom")
    repair.add_argument("workpiece")
    repair.add_argument("--from-commit", default=None, help="commit reachable from the coordinator's repository")
    repair.add_argument("--from-worktree", default=None, help="worktree whose HEAD is that commit")
    repair.add_argument("--tree", default=None, help="64-hex git tree digest the evidence binds (low-level form)")
    repair.add_argument(
        "--checkout", default=None, help="64-hex capture commit the verifying lane checks out (low-level form)"
    )
    repair.set_defaults(handler=cmd_repair)

    supersede = subparsers.add_parser("supersede", help="seal an open draft as a bloom's successor")
    supersede.add_argument("bloom", help="the predecessor bloom id")
    supersede.add_argument("--draft", required=True, help="the open draft handle to seal as the successor")
    supersede.add_argument("--idempotency-key", default=None)
    supersede.set_defaults(handler=cmd_supersede)

    grant = subparsers.add_parser("grant", help="hand a wedged member more attempts and resume it")
    grant.add_argument("bloom")
    grant.add_argument("workpiece")
    grant.add_argument("--stage", default=None, help="defaults to the stage the member is wedged at")
    grant.add_argument("--attempts", type=int, default=1, help="how many more attempts to allow [1]")
    grant.add_argument(
        "--idempotency-key",
        default=None,
        help="required to grant the SAME shape twice; the default key is content-derived, so a "
        "repeat without one is discarded as a duplicate",
    )
    grant.set_defaults(handler=cmd_grant)

    adjudicate = with_override(subparsers.add_parser("adjudicate", help="close composition findings you have read"))
    adjudicate.add_argument("bloom")
    adjudicate.add_argument(
        "--finding",
        action="append",
        required=True,
        metavar="HEX",
        help="a finding's verdict-artifact digest; repeat for several",
    )
    adjudicate.add_argument(
        "--defer",
        type=int,
        default=None,
        metavar="ISSUE",
        help="defer the findings to this filed issue instead of accepting them",
    )
    adjudicate.set_defaults(handler=cmd_adjudicate)

    return parser


def main(argv: Sequence[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    needs_journal = args.handler in (cmd_status, cmd_orders, cmd_why)
    if needs_journal and not args.journal:
        print(
            "error: no journal path. Pass --journal or set AETHER_STORE_PATH to the "
            "coordinator's SQLite store.",
            file=sys.stderr,
        )
        return 2
    try:
        args.handler(args)
    except OperatorError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    except BrokenPipeError:
        return 0
    return 0


if __name__ == "__main__":
    sys.exit(main())
