#!/usr/bin/env python3
"""Diagnose and recover a Bloomery coordinator from one command line.

The coordinator already exposes everything an operator needs: a REST control
API for every override, and a SQLite journal that holds the dispatch state the
API does not project. What it did not have was a way to reach either without
hand-writing a throwaway query or hand-encoding a request body -- which is how
an hour disappears while a bloom sits wedged.

Read commands (status / orders / why / evidence / flakes) answer "what is
stopping this member" and, for flakes, which stage/cause signatures recur
beside live queue/slot/filesystem pressure. Action commands (hold / release /
repair / supersede / grant / adjudicate) are the coordinator's override routes
with their bodies built correctly. Repair names a commit (`--from-commit`) and
the chassis derives the candidate; `--tree` / `--checkout` stay for a pair the
coordinator cannot read.

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

# Fact and Outcome variant names in declaration order. The journal stores each
# as aether_data::wire: a little-endian u32 variant index, then the selected
# body's positional fields. ORDER is load-bearing the same way STAGES is — a
# variant inserted anywhere but the tail renumbers every name after it. Re-read
# event.rs / outcome.rs before editing either list.
FACT_NAMES = (
    "Seal",
    "Supersede",
    "Integrate",
    "AdmitEvidence",
    "Resolve",
    "Land",
    "AdoptAnswer",
    "AttemptCompleted",
    "AggregateReviewCompleted",
    "ObserveMainline",
    "AggregateVerifyCompleted",
    "LandingRejected",
    "GrantAttempts",
    "VerifyFailed",
    "RequestOrphanClaimRelease",
    "CompleteOrphanClaimRelease",
    "AggregateReviewExecutorFault",
    "FoldConflict",
    "ObserveMainlineDiverged",
    "OperatorAdjudication",
    "OperatorRepair",
    "OperatorHold",
    "OperatorRelease",
    "SurfaceOverlap",
    "GraphSeal",
    "VerifyHostFault",
    "ResumeHostFault",
    "SpliceAssembled",
    "MemberExecutorFault",
)

OUTCOME_NAMES = (
    "Duplicate",
    "Sealed",
    "SealRejected",
    "Superseded",
    "SupersedeRejected",
    "Integrated",
    "IntegrateRejected",
    "EvidenceAdmitted",
    "AdmitEvidenceRejected",
    "Resolved",
    "ResolveRejected",
    "Landed",
    "LandRejected",
    "AnswerAdopted",
    "AdoptAnswerRejected",
    "AttemptAdvanced",
    "AttemptRetried",
    "AttemptWedged",
    "AttemptCompletedRejected",
    "RefineReentered",
    "AggregateReviewDispatched",
    "AggregateReviewReentered",
    "AggregateReviewParked",
    "AggregateReviewRejected",
    "MainlineAdvanced",
    "MainlineUnchanged",
    "MainlineHeld",
    "AggregateVerifyDispatched",
    "AggregateVerifyPassed",
    "AggregateVerifyReentered",
    "AggregateVerifyParked",
    "AggregateVerifyRejected",
    "LandingReentered",
    "LandingParked",
    "LandingRejectedRefused",
    "AttemptsGranted",
    "GrantAttemptsRejected",
    "VerifyFailedRejected",
    "OrphanClaimReleaseRequested",
    "OrphanClaimReleaseCompleted",
    "OrphanClaimReleaseRejected",
    "AggregateReviewExecutorFaulted",
    "AggregateReviewExecutorWedged",
    "VerifyReused",
    "AggregateVerifyReused",
    "FoldConflictDispatched",
    "FoldConflictRejected",
    "MainlineDiverged",
    "CompositionRewoven",
    "CompositionWedged",
    "CompositionRepaired",
    "FindingsAdjudicated",
    "AdjudicationRejected",
    "OperatorRepairAccepted",
    "OperatorRepairRejected",
    "BloomHeld",
    "BloomReleased",
    "OperatorHoldRejected",
    "SurfaceOverlap",
    "SealQuiesced",
    "VerifyHostFaultHeld",
    "HostFaultResumed",
    "HostFaultRejected",
    "SpliceAssembled",
    "SpliceRejected",
    "MachineryRetried",
    "MachineryWedged",
    "MemberExecutorFaultRejected",
)

# Canonical verifier identities, in the bit order VerifyFailure::ALL uses. A
# set travels on the wire as the sorted sequence of these names, never as the
# in-memory mask, so grouping must sort by this order rather than by whatever
# order a fixture happened to list.
VERIFY_FAILURE_NAMES = (
    "verify.preflight",
    "verify.fmt",
    "verify.clippy",
    "verify.docs",
    "verify.test",
    "verify.dup",
    "verify.deps",
    "verify.suppress",
)

# Typed machinery facts/outcomes. The aggregate-review pair already exists in
# this tree; MemberExecutorFault / MachineryRetried / MachineryWedged are the
# #5091 member-stage lifecycle (tail-appended on the journal, JSON-shaped in
# fixtures and in GET /journal).
MACHINERY_FACT_NAMES = frozenset(
    {
        "AggregateReviewExecutorFault",
        "MemberExecutorFault",
    }
)
MACHINERY_RETRY_OUTCOMES = frozenset(
    {
        "AggregateReviewExecutorFaulted",
        "MachineryRetried",
    }
)
MACHINERY_WEDGE_OUTCOMES = frozenset(
    {
        "AggregateReviewExecutorWedged",
        "MachineryWedged",
    }
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

    def correspondence_hex(self, digest: str) -> str | None:
        """The backend object hex `backend_correspondence` records for `digest`.

        The column is opaque bytes: a 20-byte git sha-1 today, a 32-byte sha-256
        on a future object format. Either way the operator pastes the hex. A
        missing table, a missing row, or a digest this is not, is None — the
        ladder names the miss rather than crashing the diagnosis.
        """
        if not is_digest_hex(digest):
            return None
        try:
            row = self.conn.execute(
                "SELECT backend_object FROM backend_correspondence WHERE digest = ?",
                (bytes.fromhex(digest),),
            ).fetchone()
        except sqlite3.Error:
            return None
        if row is None or row[0] is None:
            return None
        return bytes(row[0]).hex()

    def records(self) -> list[dict[str, Any]]:
        """Every journaled event, oldest first, with its recorded outcome.

        Sequence comes from the `sequence` column — not from rowid or Python
        insertion order — so a report survives coordinator restart and a
        vacuum. A missing table or an undecodable row is skipped rather than
        taking the rest of the report down: flakes is a diagnosis, and one
        surprising blob must not hide the signatures that did decode.
        """
        try:
            rows = list(
                self.conn.execute("SELECT sequence, event, decisions FROM journal ORDER BY sequence")
            )
        except sqlite3.Error:
            return []
        decoded = []
        for row in rows:
            record = decode_journal_row(row["sequence"], row["event"], row["decisions"])
            if record is not None:
                decoded.append(record)
        return decoded


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


def live_occupant(
    claims: Sequence[tuple[str, dict[str, Any] | None]],
) -> tuple[str | None, dict[str, Any] | None, str]:
    """The sole live identity among `claims`, or why there isn't one.

    Occupancy is a `/proc` match, never directory order or nonce age. Several
    live matches, or a claim whose identity cannot be read when nothing live
    matches, is `unknown` — not a guessed owner. Every readable identity being
    dead is `free`.
    """
    live: list[tuple[str, dict[str, Any]]] = []
    unreadable = False
    for nonce, identity in claims:
        if identity is None or not isinstance(identity.get("pid"), int):
            unreadable = True
            continue
        observed = observe_process(identity["pid"])
        if observed is not None and identity_matches(identity, observed):
            live.append((nonce, identity))
    if len(live) == 1:
        nonce, identity = live[0]
        return nonce, identity, "occupied"
    if len(live) > 1 or unreadable:
        return None, None, "unknown"
    return None, None, "free"


def list_slots(worktree_base: str) -> list[dict[str, Any]]:
    """Every slot visible under the scratch root, with occupancy and quarantine.

    Occupied means exactly one retained evidence identity still matches a live
    `/proc` process. Retained dead evidence is not occupancy. Quarantined means
    a sibling `slot-<n>.quarantine` file is present — the child could not be
    terminated and the next dispatch must not build there. A slot can be both:
    a live re-adopted run whose cancel just failed. Ambiguous live matches or
    an unreadable identity with nothing live to select are unknown, never a
    guessed current owner.
    """
    base = Path(worktree_base)
    if not base.is_dir():
        die(
            f"no worktree base at {base}. Pass --worktree-base or set "
            "AETHER_GITHUB_LOCAL_WORKTREE_BASE to the directory the coordinator "
            "checks lanes out under."
        )

    claims: dict[int, list[tuple[str, dict[str, Any] | None]]] = {}
    for entry in base.iterdir():
        if not entry.is_dir() or not entry.name.endswith(EVIDENCE_SUFFIX):
            continue
        nonce = entry.name[: -len(EVIDENCE_SUFFIX)]
        if not nonce:
            continue
        slot_body = (entry / SLOT_RECORD).read_text(encoding="utf-8").strip() if (entry / SLOT_RECORD).is_file() else ""
        if not slot_body.isdigit():
            continue
        claims.setdefault(int(slot_body), []).append((nonce, read_json_object(entry / IDENTITY_RECORD)))

    for slot_claims in claims.values():
        slot_claims.sort(key=lambda claim: claim[0])

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

    slots = sorted(checkout_slots | set(claims) | set(quarantines))
    listed = []
    for slot in slots:
        quarantine = quarantines.get(slot)
        occupant, identity, occupancy = live_occupant(claims.get(slot, []))
        if quarantine is not None:
            state = "quarantined"
        else:
            state = occupancy
        if quarantine is not None and isinstance(quarantine.get("identity"), dict):
            identity = quarantine["identity"]
        elif occupancy != "occupied":
            identity = None
        nonce = (quarantine or {}).get("nonce")
        if not nonce and occupancy == "occupied":
            nonce = occupant
        listed.append(
            {
                "slot": slot,
                "state": state,
                "nonce": nonce,
                "identity": identity,
                "quarantine": quarantine,
            }
        )
    return listed


def filesystem_capacity(path: str) -> dict[str, Any] | None:
    """Capacity and free bytes for one path, via statvfs.

    The path is stated, never walked. A cargo target tree holds millions of
    files; sizing it by walking is the race the janitor already documents.
    """
    try:
        stat = os.stat(path)
        vfs = os.statvfs(path)
    except OSError:
        return None
    fragment = vfs.f_frsize
    return {
        "path": str(Path(path)),
        "device": stat.st_dev,
        "total_bytes": fragment * vfs.f_blocks,
        "free_bytes": fragment * vfs.f_bavail,
    }


def _unmeasured_fs(reason: str, **extra: Any) -> dict[str, Any]:
    report = {"measured": False, "reason": reason}
    report.update(extra)
    return report


def _measured_fs(snapshot: dict[str, Any]) -> dict[str, Any]:
    return {
        "measured": True,
        "path": snapshot["path"],
        "total_bytes": snapshot["total_bytes"],
        "free_bytes": snapshot["free_bytes"],
    }


def live_filesystems(worktree_base: str, target_base: str) -> dict[str, Any]:
    """Worktree and lane-target capacity, with absent/shared axes unmeasured."""
    if not worktree_base:
        worktree = _unmeasured_fs("worktree base not supplied")
        worktree_snap = None
    else:
        worktree_snap = filesystem_capacity(worktree_base)
        worktree = (
            _measured_fs(worktree_snap)
            if worktree_snap
            else _unmeasured_fs(f"cannot stat worktree base {worktree_base}")
        )

    if not target_base:
        target = _unmeasured_fs("lane target base not supplied")
    else:
        target_snap = filesystem_capacity(target_base)
        if target_snap is None:
            target = _unmeasured_fs(f"cannot stat lane target base {target_base}", path=str(Path(target_base)))
        elif worktree_snap is not None and target_snap["device"] == worktree_snap["device"]:
            target = _unmeasured_fs(
                "shares the worktree filesystem",
                shared_with="worktree",
                path=target_snap["path"],
            )
        else:
            target = _measured_fs(target_snap)
    return {"worktree": worktree, "lane_target": target}


def slot_pressure(worktree_base: str) -> dict[str, Any]:
    """Occupied/free/quarantined/unknown counts, or an unmeasured axis."""
    if not worktree_base:
        return _unmeasured_fs("worktree base not supplied")
    if not Path(worktree_base).is_dir():
        return _unmeasured_fs(f"no worktree base at {worktree_base}")
    listed = list_slots(worktree_base)
    counts = {"occupied": 0, "free": 0, "quarantined": 0, "unknown": 0}
    for entry in listed:
        state = entry.get("state")
        if state in counts:
            counts[state] += 1
    return {"measured": True, "total": len(listed), **counts}


def order_pressure(orders: Sequence[dict[str, Any]]) -> dict[str, Any]:
    """Outstanding and overdue orders, by stage, plus queue depth."""
    by_stage: dict[str, dict[str, int]] = {}
    overdue = 0
    for order in orders:
        stage = order.get("stage") or "unknown"
        bucket = by_stage.setdefault(stage, {"outstanding": 0, "overdue": 0})
        bucket["outstanding"] += 1
        if order.get("overdue_secs", 0) > 0:
            bucket["overdue"] += 1
            overdue += 1
    return {
        "outstanding": len(orders),
        "overdue": overdue,
        "by_stage": [
            {"stage": stage, "outstanding": bucket["outstanding"], "overdue": bucket["overdue"]}
            for stage, bucket in sorted(by_stage.items())
        ],
    }


def live_pressure(
    orders: Sequence[dict[str, Any]],
    worktree_base: str,
    target_base: str,
    now_millis: int,
) -> dict[str, Any]:
    """Point-in-time host pressure: queue, slots, deadlines, filesystems."""
    queued = order_pressure(orders)
    return {
        "observed_unix_millis": now_millis,
        "orders": queued,
        "queue_depth": queued["outstanding"],
        "slots": slot_pressure(worktree_base),
        "filesystems": live_filesystems(worktree_base, target_base),
    }


def flakes_report(
    records: Sequence[dict[str, Any]],
    orders: Sequence[dict[str, Any]],
    worktree_base: str,
    target_base: str,
    now_millis: int,
) -> dict[str, Any]:
    """The flakes document: durable signatures beside live pressure."""
    return {
        "signatures": group_flake_signatures(records),
        "pressure": live_pressure(orders, worktree_base, target_base, now_millis),
    }


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


OPERATOR_SCRIPT = "bloomery-operator.py"
SUGGESTED_GRANT_ATTEMPTS = 1
TREE_PRODUCER = "AttemptCompleted.candidate.tree (construct/refine capture)"
CHECKOUT_PRODUCER = "AttemptCompleted.candidate.checkout (construct/refine capture)"
FROM_COMMIT_PRODUCER = "backend_correspondence lookup of CandidateRef.checkout"
BASE_PRODUCER = "Fact::Seal.base / BloomSpec.base"


def digest_hex(value: Any) -> str | None:
    """A digest as 64 lowercase hex, or None when `value` is not one.

    The API hex-renders digests; a fixture or a raw journal walk may still
    hand over the 32-byte array serde's default would produce. Both spellings
    have to resolve, because a ladder that only accepted one would drop a
    known digest and print the missing-input line over a value it already had.
    """
    if isinstance(value, str) and is_digest_hex(value):
        return value.lower()
    if isinstance(value, (bytes, bytearray)) and len(value) == DIGEST_BYTES:
        return bytes(value).hex()
    if isinstance(value, (list, tuple)) and len(value) == DIGEST_BYTES:
        try:
            return bytes(value).hex()
        except (TypeError, ValueError):
            return None
    return None


def fact_of(record: dict[str, Any]) -> dict[str, Any] | None:
    """The admitted Fact object inside a `GET /journal` record, if present."""
    event = record.get("event")
    if not isinstance(event, dict):
        return None
    fact = event.get("fact")
    return fact if isinstance(fact, dict) else event


class _WireCursor:
    """A one-shot walk over an aether_data::wire blob."""

    def __init__(self, blob: bytes) -> None:
        self.blob = blob
        self.offset = 0

    def remaining(self) -> int:
        return len(self.blob) - self.offset

    def take(self, count: int) -> bytes | None:
        if count < 0 or self.remaining() < count:
            return None
        start = self.offset
        self.offset += count
        return self.blob[start : self.offset]

    def u32(self) -> int | None:
        raw = self.take(4)
        return None if raw is None else struct.unpack("<I", raw)[0]

    def digest(self) -> str | None:
        raw = self.take(DIGEST_BYTES)
        return None if raw is None else raw.hex()

    def string(self) -> str | None:
        length = self.u32()
        if length is None or length > self.remaining():
            return None
        raw = self.take(length)
        if raw is None:
            return None
        try:
            return raw.decode("utf-8")
        except UnicodeDecodeError:
            return None

    def strings(self) -> list[str] | None:
        count = self.u32()
        if count is None or count > 1024:
            return None
        values = []
        for _ in range(count):
            value = self.string()
            if value is None:
                return None
            values.append(value)
        return values


def _variant_name(index: int | None, names: Sequence[str]) -> str | None:
    if index is None:
        return None
    if 0 <= index < len(names):
        return names[index]
    return f"unknown({index})"


def _looks_like_json(blob: bytes) -> bool:
    for byte in blob:
        if byte in b" \t\r\n":
            continue
        return byte in b"{[\""
    return False


def _enum_payload(value: Any) -> tuple[str | None, Any]:
    """An externally-tagged serde enum as (variant, payload).

    Unit variants arrive as a bare string (`"Duplicate"`). Struct variants
    arrive as a one-key object. Anything else is not a variant.
    """
    if isinstance(value, str) and value:
        return value, None
    if isinstance(value, dict) and len(value) == 1:
        name, payload = next(iter(value.items()))
        return name, payload
    return None, None


def _wire_evidence(cursor: _WireCursor) -> dict[str, Any] | None:
    subject = cursor.digest()
    kind_index = cursor.u32()
    detail = cursor.digest()
    if subject is None or kind_index is None or detail is None:
        return None
    kinds = (
        "Approval",
        "VerificationResult",
        "ReviewFinding",
        "ResolutionClaim",
        "StudyRecord",
        "Question",
        "ExecutorFault",
        "FoldConflict",
        "RepairTriage",
        "ReviewAdvisory",
    )
    kind = kinds[kind_index] if kind_index < len(kinds) else f"unknown({kind_index})"
    return {"subject": subject, "kind": kind, "detail": detail}


def _decode_known_fact(name: str, cursor: _WireCursor) -> dict[str, Any] | None:
    """The fields flakes groups, for the facts it recognizes on the wire."""
    if name == "VerifyFailed":
        bloom = cursor.digest()
        workpiece = cursor.string()
        evidence = _wire_evidence(cursor)
        verifiers = cursor.strings()
        if bloom is None or workpiece is None or evidence is None or verifiers is None:
            return None
        return {"bloom": bloom, "workpiece": workpiece, "evidence": evidence, "failed_verifiers": verifiers}
    if name == "AggregateReviewExecutorFault":
        bloom = cursor.digest()
        evidence = _wire_evidence(cursor)
        if bloom is None or evidence is None:
            return None
        return {"bloom": bloom, "evidence": evidence, "stage": "AggregateReview"}
    if name == "MemberExecutorFault":
        bloom = cursor.digest()
        workpiece = cursor.string()
        stage = _variant_name(cursor.u32(), STAGES)
        evidence = _wire_evidence(cursor)
        if bloom is None or workpiece is None or stage is None or evidence is None:
            return None
        return {"bloom": bloom, "workpiece": workpiece, "stage": stage, "evidence": evidence}
    return {}


def _decode_known_outcome(name: str, cursor: _WireCursor) -> dict[str, Any] | None:
    """The fields flakes counts, for the outcomes it recognizes on the wire."""
    if name in {"RefineReentered"}:
        bloom = cursor.digest()
        workpiece = cursor.string()
        rolls = cursor.u32()
        if bloom is None or workpiece is None or rolls is None:
            return None
        return {"bloom": bloom, "workpiece": workpiece, "rolls": rolls}
    if name == "AttemptWedged":
        bloom = cursor.digest()
        workpiece = cursor.string()
        stage = _variant_name(cursor.u32(), STAGES)
        verifiers = cursor.strings()
        if bloom is None or workpiece is None or stage is None or verifiers is None:
            return None
        return {"bloom": bloom, "workpiece": workpiece, "stage": stage, "repeated_verifiers": verifiers}
    if name in {"AggregateReviewExecutorFaulted", "AggregateReviewExecutorWedged"}:
        bloom = cursor.digest()
        subject = cursor.digest()
        rolls = cursor.u32()
        evidence = cursor.digest()
        budget = cursor.u32()
        if bloom is None or subject is None or rolls is None or evidence is None or budget is None:
            return None
        return {
            "bloom": bloom,
            "stage": "AggregateReview",
            "rolls": rolls,
            "budget": budget,
            "evidence": {"detail": evidence, "kind": "ExecutorFault", "subject": subject},
        }
    if name in {"MachineryRetried", "MachineryWedged"}:
        bloom = cursor.digest()
        workpiece = cursor.string()
        stage = _variant_name(cursor.u32(), STAGES)
        rolls = cursor.u32()
        budget = cursor.u32()
        if bloom is None or workpiece is None or stage is None or rolls is None or budget is None:
            return None
        return {"bloom": bloom, "workpiece": workpiece, "stage": stage, "rolls": rolls, "budget": budget}
    return {}


def decode_event_blob(blob: bytes | None) -> dict[str, Any] | None:
    """An Event as `{idempotency_key, fact}` — JSON fixture or wire bytes."""
    if not blob:
        return None
    if _looks_like_json(blob):
        try:
            value = json.loads(blob)
        except json.JSONDecodeError:
            return None
        if not isinstance(value, dict):
            return None
        fact = value.get("fact") if isinstance(value.get("fact"), dict) else value
        key = value.get("idempotency_key")
        return {"idempotency_key": key, "fact": fact}
    cursor = _WireCursor(blob)
    key = cursor.string()
    name = _variant_name(cursor.u32(), FACT_NAMES)
    if key is None or name is None:
        return None
    payload = _decode_known_fact(name, cursor)
    if payload is None:
        return None
    return {"idempotency_key": key, "fact": {name: payload}}


def decode_decisions_blob(blob: bytes | None) -> dict[str, Any] | None:
    """A Decisions outcome — JSON fixture or the leading wire Outcome.

    Effects are unread: flakes groups facts and outcomes, never the outbox,
    and not reading the tail means a Decision append cannot break this.
    """
    if not blob:
        return None
    if _looks_like_json(blob):
        try:
            value = json.loads(blob)
        except json.JSONDecodeError:
            return None
        if isinstance(value, dict) and "outcome" in value:
            return value["outcome"]
        return value
    cursor = _WireCursor(blob)
    name = _variant_name(cursor.u32(), OUTCOME_NAMES)
    if name is None:
        return None
    payload = _decode_known_outcome(name, cursor)
    if payload is None:
        return None
    return {name: payload} if payload else name


def decode_journal_row(sequence: Any, event: bytes | None, decisions: bytes | None) -> dict[str, Any] | None:
    """One journal row as the record shape `fact_of` / grouping already walk."""
    decoded_event = decode_event_blob(event)
    if decoded_event is None:
        return None
    return {
        "sequence": sequence,
        "event": decoded_event,
        "outcome": decode_decisions_blob(decisions),
    }


def _payload_field(payload: Any, *names: str) -> Any:
    if not isinstance(payload, dict):
        return None
    for name in names:
        if name in payload:
            return payload[name]
    return None


def _payload_stage(payload: Any, default: str | None = None) -> str | None:
    stage = _payload_field(payload, "stage")
    if isinstance(stage, str) and stage:
        return stage
    if isinstance(stage, int):
        return _variant_name(stage, STAGES)
    if isinstance(stage, (bytes, bytearray)):
        return stage_name(bytes(stage))
    return default


def _payload_workpiece(payload: Any) -> str | None:
    value = _payload_field(payload, "workpiece")
    if isinstance(value, str) and value:
        return value
    if isinstance(value, dict):
        inner = value.get("0") if "0" in value else value.get("id")
        if isinstance(inner, str) and inner:
            return inner
    return None


def _payload_bloom(payload: Any) -> str | None:
    return digest_hex(_payload_field(payload, "bloom"))


def _payload_rolls(payload: Any) -> int | None:
    rolls = _payload_field(payload, "rolls", "attempt")
    return rolls if isinstance(rolls, int) else None


def _payload_evidence(payload: Any) -> dict[str, Any]:
    evidence = _payload_field(payload, "evidence")
    if not isinstance(evidence, dict):
        return {"kind": None, "detail": None}
    kind = evidence.get("kind")
    if isinstance(kind, dict):
        kind, _ = _enum_payload(kind)
    return {
        "kind": kind if isinstance(kind, str) and kind else None,
        "detail": digest_hex(evidence.get("detail")),
    }


def canonical_verifiers(value: Any) -> list[str]:
    """A VerifyFailureSet as the canonical name list, order-stable.

    Unknown names are kept (sorted after the vocabulary) so a future identity
    still groups with itself instead of vanishing into an empty cause.
    """
    if value is None:
        return []
    if isinstance(value, str):
        names = [value]
    elif isinstance(value, (list, tuple)):
        names = [item for item in value if isinstance(item, str) and item]
    else:
        return []
    rank = {name: index for index, name in enumerate(VERIFY_FAILURE_NAMES)}
    unique = sorted(set(names), key=lambda name: (rank.get(name, len(rank)), name))
    return unique


def outcome_name_of(outcome: Any) -> str | None:
    name, _ = _enum_payload(outcome)
    return name


def _is_refused_or_duplicate(outcome_name: str | None) -> bool:
    if outcome_name is None:
        return False
    return outcome_name == "Duplicate" or outcome_name.endswith("Rejected")


def flake_observation(record: dict[str, Any]) -> dict[str, Any] | None:
    """One admitted verifier or machinery failure, or None.

    Refused and duplicate outcomes are not observations: they never entered
    the durable ledger as a failure. Free-form stderr and timestamps are
    unread — they are unstable text, not a signature.
    """
    outcome = record.get("outcome")
    outcome_name = outcome_name_of(outcome)
    if _is_refused_or_duplicate(outcome_name):
        return None
    _, outcome_payload = _enum_payload(outcome)
    fact = fact_of(record)
    fact_name, fact_payload = _enum_payload(fact) if fact else (None, None)

    sequence = record.get("sequence")
    if not isinstance(sequence, int):
        return None

    if fact_name == "VerifyFailed":
        verifiers = canonical_verifiers(_payload_field(fact_payload, "failed_verifiers"))
        evidence = _payload_evidence(fact_payload)
        return {
            "kind": "verifier",
            "stage": _payload_stage(fact_payload, "Verify") or "Verify",
            "cause": ",".join(verifiers) if verifiers else "verify",
            "verifiers": verifiers,
            "evidence_kind": evidence["kind"] or "VerificationResult",
            "evidence_detail": evidence["detail"],
            "bloom": _payload_bloom(fact_payload),
            "workpiece": _payload_workpiece(fact_payload),
            "sequence": sequence,
            "machinery_retry": False,
            "machinery_wedge": False,
            "rolls": _payload_rolls(outcome_payload),
        }

    machinery_from_fact = fact_name in MACHINERY_FACT_NAMES
    machinery_from_outcome = outcome_name in MACHINERY_RETRY_OUTCOMES or outcome_name in MACHINERY_WEDGE_OUTCOMES
    if not machinery_from_fact and not machinery_from_outcome:
        return None

    payload = fact_payload if machinery_from_fact else outcome_payload
    stage = _payload_stage(payload) or _payload_stage(outcome_payload) or _payload_stage(fact_payload)
    if fact_name == "AggregateReviewExecutorFault" or (
        outcome_name in {"AggregateReviewExecutorFaulted", "AggregateReviewExecutorWedged"}
    ):
        stage = stage or "AggregateReview"
    evidence = _payload_evidence(payload)
    if evidence["kind"] is None:
        evidence = _payload_evidence(outcome_payload)
    return {
        "kind": "machinery",
        "stage": stage or "unknown",
        "cause": "executor_fault",
        "verifiers": [],
        "evidence_kind": evidence["kind"] or "ExecutorFault",
        "evidence_detail": evidence["detail"],
        "bloom": _payload_bloom(payload) or _payload_bloom(outcome_payload),
        "workpiece": _payload_workpiece(payload) or _payload_workpiece(outcome_payload),
        "sequence": sequence,
        "machinery_retry": outcome_name in MACHINERY_RETRY_OUTCOMES,
        "machinery_wedge": outcome_name in MACHINERY_WEDGE_OUTCOMES,
        "rolls": _payload_rolls(outcome_payload) or _payload_rolls(payload),
    }


def _signature_key(observation: dict[str, Any]) -> tuple[Any, ...]:
    return (
        observation["kind"],
        observation["stage"],
        observation["cause"],
        tuple(observation["verifiers"]),
        observation["evidence_kind"],
    )


def group_flake_signatures(records: Sequence[dict[str, Any]]) -> list[dict[str, Any]]:
    """Stable stage/cause signatures with counts, spans, and machinery tallies.

    Evidence.detail is reported from the newest observation, not keyed: the
    detail is a per-run artifact digest, and grouping on it would make every
    recurrence look unique. The typed evidence kind stays in the key.
    """
    grouped: dict[tuple[Any, ...], dict[str, Any]] = {}
    for record in records:
        observation = flake_observation(record)
        if observation is None:
            continue
        key = _signature_key(observation)
        entry = grouped.get(key)
        if entry is None:
            entry = {
                "kind": observation["kind"],
                "stage": observation["stage"],
                "cause": observation["cause"],
                "verifiers": list(observation["verifiers"]),
                "evidence_kind": observation["evidence_kind"],
                "evidence_detail": observation["evidence_detail"],
                "count": 0,
                "blooms": [],
                "workpieces": [],
                "first_sequence": observation["sequence"],
                "last_sequence": observation["sequence"],
                "machinery_retries": 0,
                "machinery_wedges": 0,
                "_retry_events": 0,
                "_max_rolls": 0,
            }
            grouped[key] = entry
        entry["count"] += 1
        entry["last_sequence"] = observation["sequence"]
        if observation["evidence_detail"]:
            entry["evidence_detail"] = observation["evidence_detail"]
        bloom = observation["bloom"]
        if bloom and bloom not in entry["blooms"]:
            entry["blooms"].append(bloom)
        workpiece = observation["workpiece"]
        if workpiece and workpiece not in entry["workpieces"]:
            entry["workpieces"].append(workpiece)
        if observation["machinery_retry"]:
            entry["_retry_events"] += 1
        if observation["machinery_wedge"]:
            entry["machinery_wedges"] += 1
        rolls = observation["rolls"]
        if observation["kind"] == "machinery" and isinstance(rolls, int):
            entry["_max_rolls"] = max(entry["_max_rolls"], rolls)

    signatures = []
    for entry in grouped.values():
        entry["machinery_retries"] = max(entry.pop("_retry_events"), entry.pop("_max_rolls"))
        entry["blooms"] = sorted(entry["blooms"])
        entry["workpieces"] = sorted(entry["workpieces"])
        signatures.append(entry)
    signatures.sort(key=lambda item: (-item["count"], -item["last_sequence"], item["kind"], item["stage"], item["cause"]))
    return signatures


def bloom_sealed_base(records: Sequence[dict[str, Any]], bloom_id: str) -> str | None:
    """The sealed `base` the journal recorded for `bloom_id`.

    BloomView does not project the spec, so the only place to recover the
    compare-and-swap base is the Seal / Supersede fact that minted the bloom.
    Newest match wins: a successor's own seal is the one its mainline is
    judged against, not the predecessor's.
    """
    if not bloom_id:
        return None
    for record in reversed(records):
        fact = fact_of(record)
        if not isinstance(fact, dict):
            continue
        outcome = record.get("outcome")
        seal = fact.get("Seal")
        if isinstance(seal, dict) and _outcome_names_sealed(outcome, bloom_id):
            return digest_hex(seal.get("base"))
        supersede = fact.get("Supersede")
        if isinstance(supersede, dict) and _outcome_names_successor(outcome, bloom_id):
            successor = supersede.get("successor")
            if isinstance(successor, dict):
                return digest_hex(successor.get("base"))
    return None


def _outcome_names_sealed(outcome: Any, bloom_id: str) -> bool:
    return isinstance(outcome, dict) and digest_hex(outcome.get("Sealed")) == bloom_id


def _outcome_names_successor(outcome: Any, bloom_id: str) -> bool:
    if not isinstance(outcome, dict) or not isinstance(outcome.get("Superseded"), dict):
        return False
    return digest_hex(outcome["Superseded"].get("successor")) == bloom_id


def candidate_pair(value: Any, source: str) -> dict[str, Any] | None:
    """A CandidateRef's tree/checkout pair, or None when neither field is a digest."""
    if not isinstance(value, dict):
        return None
    tree = digest_hex(value.get("tree"))
    checkout = digest_hex(value.get("checkout"))
    if tree is None and checkout is None:
        return None
    return {"tree": tree, "checkout": checkout, "source": source}


def last_captured_candidate(records: Sequence[dict[str, Any]], workpiece: str) -> dict[str, Any] | None:
    """The newest journaled CandidateRef for `workpiece`.

    A wedged member has no outstanding order, so the capture lives on the
    AttemptCompleted (or OperatorRepair) fact that wrote it — not on the view,
    which only projects a candidate after integrate. Newest wins: a later
    repair or refine capture is the one an operator would re-offer.
    """
    for record in reversed(records):
        fact = fact_of(record)
        if not isinstance(fact, dict):
            continue
        completed = fact.get("AttemptCompleted")
        if isinstance(completed, dict) and completed.get("workpiece") == workpiece:
            pair = candidate_pair(completed.get("candidate"), "AttemptCompleted.candidate")
            if pair is not None:
                return pair
        repair = fact.get("OperatorRepair")
        if isinstance(repair, dict):
            body = repair.get("repair") if isinstance(repair.get("repair"), dict) else repair
            if body.get("workpiece") == workpiece:
                pair = candidate_pair(body.get("candidate"), "OperatorRepair.candidate")
                if pair is not None:
                    return pair
    return None


def last_attempt_count(records: Sequence[dict[str, Any]], workpiece: str) -> int | None:
    """The newest journaled attempt count for `workpiece`.

    `AttemptWedged` does not carry `attempt`, so a newest-first scan that
    stopped at the wedge would report none — the exact moment the operator is
    asking how many attempts were spent. Skip outcomes that have no count.
    """
    for record in reversed(records):
        outcome = record.get("outcome")
        if not isinstance(outcome, dict):
            continue
        for payload in outcome.values():
            if not isinstance(payload, dict) or payload.get("workpiece") != workpiece:
                continue
            attempt = payload.get("attempt")
            if isinstance(attempt, int):
                return attempt
    return None


def stranded_members(bloom: dict[str, Any]) -> list[str]:
    """Workpieces on `bloom` that already hold an integrated candidate."""
    names = []
    for member in bloom.get("members") or []:
        if member.get("resolution") and member.get("workpiece"):
            names.append(member["workpiece"])
    return names


def _missing_note(flag: str, producer: str) -> str:
    return f"{flag} unknown to the journal — produced by {producer}"


def _filled_note(flag: str, value: str, source: str) -> str:
    return f"{flag} {value} from {source}"


def grant_command(bloom_id: str, workpiece: str, stage: str | None, attempts: int) -> str:
    parts = [OPERATOR_SCRIPT, "grant", bloom_id, workpiece]
    if stage:
        parts.extend(["--stage", stage])
    parts.extend(["--attempts", str(attempts)])
    return " ".join(parts)


def repair_command(
    bloom_id: str,
    workpiece: str,
    tree: str | None,
    checkout: str | None,
    from_commit: str | None,
) -> str:
    """The repair invocation with every known input filled.

    `--tree`/`--checkout` is this diagnosis's filled-digest form. `--from-commit`
    is the chassis-derived alternative when correspondence already names the
    capture commit. Placeholders for `--reason` / `--operator` stay so the line
    parses; those two are the operator's words, never the journal's.
    """
    parts = [OPERATOR_SCRIPT, "repair", bloom_id, workpiece]
    if tree and checkout:
        parts.extend(["--tree", tree, "--checkout", checkout])
    elif from_commit:
        parts.extend(["--from-commit", from_commit])
    else:
        if tree:
            parts.extend(["--tree", tree])
        if checkout:
            parts.extend(["--checkout", checkout])
    parts.extend(["--reason", "<reason>", "--operator", "<operator>"])
    return " ".join(parts)


def supersede_command(bloom_id: str) -> str:
    return f"{OPERATOR_SCRIPT} supersede {bloom_id} --draft <draft>"


def review_park_of(bloom: dict[str, Any]) -> dict[str, Any] | None:
    """The bloom-scoped aggregate-review park, or None when the bloom is not parked.

    Distinct from a member `pending_decision` and from an executor fault: the
    park is bloom-level, and its digest is the finding `adjudicate --finding`
    names. A reduced REST rendering (digest only) is still a park.
    """
    park = bloom.get("review_park")
    if not isinstance(park, dict):
        return None
    question = digest_hex(park.get("question"))
    if not question:
        return None
    prompt = park.get("prompt")
    blocked = park.get("blocked")
    options = park.get("options")
    return {
        "question": question,
        "stage": park.get("stage"),
        "prompt": prompt if isinstance(prompt, str) and prompt else None,
        "options": options if isinstance(options, list) else [],
        "blocked": blocked if isinstance(blocked, str) and blocked else None,
    }


def adjudicate_command(bloom_id: str, finding: str) -> str:
    """The adjudicate invocation naming the parked question.

    Placeholders for `--reason` / `--operator` stay so the line parses; those
    two are the operator's words. `--defer` is omitted so the default is
    Accepted — the operator adds it when the disposition is a filed issue.
    """
    return (
        f"{OPERATOR_SCRIPT} adjudicate {bloom_id} --finding {finding} "
        "--reason <reason> --operator <operator>"
    )


def print_review_park(park: dict[str, Any], bloom_id: str | None) -> None:
    """Print the bloom-scoped park and the runnable recovery line."""
    print(f"  REVIEW PARK question {park['question']}")
    if park.get("stage"):
        print(f"             at {park['stage']} (bloom-scoped; not a member hold)")
    else:
        print("             bloom-scoped aggregate review; not a member hold")
    if park.get("prompt"):
        print(f"             {park['prompt']}")
    if park.get("blocked"):
        print(f"             blocks {park['blocked']}")
    bloom = bloom_id or "<bloom>"
    print(f"  adjudicate  {adjudicate_command(bloom, park['question'])}")


def recovery_ladder(
    *,
    bloom_id: str | None,
    workpiece: str,
    wedged_at: str | None,
    overdue: bool,
    force: bool,
    attempt_count: int | None,
    sealed_base: str | None,
    mainline: str | None,
    captured: dict[str, Any] | None,
    produced_candidate: bool,
    correspondence: dict[str, str],
    stranded: Sequence[str],
) -> list[dict[str, Any]]:
    """The next recovery rungs, arguments filled from the journal.

    Prints commands and provenance. Never runs them. Grant when the member is
    wedged or overdue (a retryable lane failure). Repair whenever this is a
    recovery (or a capture is already on record), with every known digest
    filled and each miss named. Supersede when the sealed base is behind
    mainline, naming the integrated candidates a successor would strand.
    """
    recovering = bool(wedged_at) or overdue
    if not recovering and not force:
        return []

    rungs: list[dict[str, Any]] = []
    bloom = bloom_id or "<bloom>"

    if recovering:
        attempts = SUGGESTED_GRANT_ATTEMPTS
        if wedged_at:
            because = f"retryable lane failure: wedged at {wedged_at}"
        else:
            because = "retryable lane failure: the outstanding order is overdue (grant is refused until the member wedges)"
        if attempt_count is not None:
            because += f", {attempt_count} attempt(s) recorded"
        rungs.append(
            {
                "verb": "grant",
                "command": grant_command(bloom, workpiece, wedged_at, attempts),
                "because": because,
                "notes": [f"suggested --attempts {attempts} (one more roll; the sealed catalog ceiling is not served)"],
            }
        )

        source = (captured or {}).get("source") or "the journal"
        tree = (captured or {}).get("tree")
        checkout = (captured or {}).get("checkout")
        from_commit = correspondence.get(checkout) if checkout else None
        notes = []
        if tree:
            notes.append(_filled_note("--tree", tree, source))
        else:
            notes.append(_missing_note("--tree", TREE_PRODUCER))
        if checkout:
            notes.append(_filled_note("--checkout", checkout, source))
        else:
            notes.append(_missing_note("--checkout", CHECKOUT_PRODUCER))
        if from_commit:
            notes.append(_filled_note("--from-commit", from_commit, FROM_COMMIT_PRODUCER))
        else:
            notes.append(_missing_note("--from-commit", FROM_COMMIT_PRODUCER))
        if produced_candidate and captured is None:
            notes.append("the last lane reported produced_candidate, but no CandidateRef is in the journal")
        rungs.append(
            {
                "verb": "repair",
                "command": repair_command(bloom, workpiece, tree, checkout, from_commit),
                "because": (
                    "a captured candidate is on record"
                    if captured or produced_candidate
                    else "no captured candidate; name the commit you built or wait for a capture"
                ),
                "notes": notes,
            }
        )

    base_stale = (
        sealed_base is not None and mainline is not None and sealed_base != mainline
    )
    if base_stale or (recovering and sealed_base is None):
        notes = []
        if sealed_base is None:
            notes.append(_missing_note("sealed base", BASE_PRODUCER))
        else:
            notes.append(f"sealed base {sealed_base}")
        if mainline:
            notes.append(f"mainline    {mainline}")
        if stranded:
            notes.append("would strand integrated candidates on: " + ", ".join(stranded))
        else:
            notes.append("would strand no integrated candidates")
        notes.append("--draft is not in the journal: open a successor draft first")
        rungs.append(
            {
                "verb": "supersede",
                "command": supersede_command(bloom) if bloom_id else None,
                "because": (
                    "the sealed base is stale (mainline has moved)"
                    if base_stale
                    else "cannot tell whether the base is stale"
                ),
                "notes": notes,
            }
        )

    if force and not rungs:
        rungs.append(
            {
                "verb": "none",
                "command": None,
                "because": (
                    "no recovery rungs: the member is not wedged or overdue, "
                    "and the sealed base is not known to be stale"
                ),
                "notes": [],
            }
        )
    return rungs


def print_ladder(rungs: Sequence[dict[str, Any]]) -> None:
    print("  ladder")
    for rung in rungs:
        verb = rung["verb"]
        command = rung.get("command")
        if command:
            print(f"    {verb:<9} {command}")
        else:
            print(f"    {verb:<9} (not assembled — see notes)")
        if rung.get("because"):
            print(f"              {rung['because']}")
        for note in rung.get("notes") or []:
            print(f"              {note}")


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
        park = review_park_of(bloom)
        bloom_id = digest_hex(bloom.get("id"))
        entry = {
            "id": bloom.get("id"),
            "status": bloom.get("status"),
            "superseded_by": bloom.get("superseded_by"),
            "landing_blocked": bloom.get("landing_blocked"),
            "executor_fault": bloom.get("executor_fault"),
            "review_park": park,
            "adjudicate": adjudicate_command(bloom_id or "<bloom>", park["question"]) if park else None,
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
        print_status_bloom(bloom)


def print_status_bloom(bloom: dict[str, Any]) -> None:
    """One bloom's human status block, including a bloom-scoped review park."""
    line = f"bloom {bloom['id']}  status={bloom['status']}"
    if bloom["superseded_by"]:
        line += f"  superseded_by={bloom['superseded_by']}"
    if bloom["review_park"]:
        line += "  REVIEW PARK"
    print(line)
    if bloom["landing_blocked"]:
        block = bloom["landing_blocked"]
        print(f"  landing blocked: {block.get('rolls')}/{block.get('budget')} refused")
    if bloom["executor_fault"]:
        fault = bloom["executor_fault"]
        terminal = " TERMINAL" if fault.get("terminal") else ""
        print(f"  executor fault: {fault.get('rolls')}/{fault.get('budget')}{terminal}")
    if bloom["review_park"]:
        print_review_park(bloom["review_park"], digest_hex(bloom["id"]))
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
    view = api.view()
    bloom, member = find_member(view, args.workpiece)
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

    captured = last_captured_candidate(records, args.workpiece)
    if captured is None and order:
        tree = digest_hex(order.get("candidate"))
        checkout = digest_hex(order.get("checkout"))
        if tree or checkout:
            captured = {"tree": tree, "checkout": checkout, "source": "outstanding_orders"}
    correspondence: dict[str, str] = {}
    if captured:
        for digest in (captured.get("tree"), captured.get("checkout")):
            if not digest:
                continue
            found = journal.correspondence_hex(digest)
            if found:
                correspondence[digest] = found

    wedged_at = (wedge or {}).get("stage")
    overdue = bool(order and order["overdue_secs"] > 0)
    bloom_id = digest_hex(bloom.get("id"))
    ladder = recovery_ladder(
        bloom_id=bloom_id,
        workpiece=args.workpiece,
        wedged_at=wedged_at if isinstance(wedged_at, str) else None,
        overdue=overdue,
        force=bool(getattr(args, "ladder", False)),
        attempt_count=last_attempt_count(records, args.workpiece),
        sealed_base=bloom_sealed_base(records, bloom_id or ""),
        mainline=digest_hex(view.get("mainline")),
        captured=captured,
        produced_candidate=bool(lane and lane.get("produced_candidate")),
        correspondence=correspondence,
        stranded=stranded_members(bloom),
    )

    park = review_park_of(bloom)
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
        "review_park": park,
        "adjudicate": adjudicate_command(bloom_id or "<bloom>", park["question"]) if park else None,
        "last_lane_result": lane,
        "ladder": ladder,
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
    if park:
        print_review_park(park, bloom_id)
    if lane:
        print(
            f"  last lane  {lane['command']} status={lane['status']} turns={lane['turns']} "
            f"candidate={lane['produced_candidate']}"
        )
        if lane["result_text"]:
            print(f"             {str(lane['result_text']).strip().splitlines()[0][:160]}")
    elif order:
        print(f"  last lane  no evidence.json yet for {order['nonce']} (still running, or reaped)")

    if ladder:
        print_ladder(ladder)


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


def human_bytes(value: int) -> str:
    """A byte count an operator can read next to a volume size."""
    remaining = float(value)
    for unit in ("B", "KiB", "MiB", "GiB", "TiB"):
        if remaining < 1024.0 or unit == "TiB":
            if unit == "B":
                return f"{int(remaining)}{unit}"
            return f"{remaining:.1f}{unit}"
        remaining /= 1024.0
    return f"{value}B"


def _print_fs_axis(label: str, axis: dict[str, Any]) -> None:
    if axis.get("measured"):
        print(
            f"  {label:<12} {axis.get('path', '-')}  "
            f"{human_bytes(axis.get('free_bytes', 0))} free / {human_bytes(axis.get('total_bytes', 0))}"
        )
        return
    extra = f" (shared with {axis['shared_with']})" if axis.get("shared_with") else ""
    path = f" {axis['path']}" if axis.get("path") else ""
    print(f"  {label:<12} unmeasured{extra}{path}  {axis.get('reason', '')}")


def print_flakes_table(report: dict[str, Any]) -> None:
    signatures = report["signatures"]
    if not signatures:
        print("no durable signatures have yet been observed")
    else:
        print(f"{'KIND':<10} {'STAGE':<17} {'CAUSE':<28} {'N':>4} {'RETRY':>6} {'WEDGE':>6} {'FIRST':>6} {'LAST':>6}  WHERE")
        for item in signatures:
            where = ",".join(item["workpieces"]) or "-"
            blooms = item["blooms"]
            if blooms:
                where = f"{where}  blooms={len(blooms)}"
            print(
                f"{item['kind']:<10} {item['stage']:<17} {item['cause']:<28} "
                f"{item['count']:>4} {item['machinery_retries']:>6} {item['machinery_wedges']:>6} "
                f"{item['first_sequence']:>6} {item['last_sequence']:>6}  {where}"
            )

    pressure = report["pressure"]
    orders = pressure["orders"]
    print()
    print(
        f"orders     outstanding={orders['outstanding']} overdue={orders['overdue']}  "
        + " ".join(
            f"{bucket['stage']}:{bucket['outstanding']}"
            + (f"({bucket['overdue']} overdue)" if bucket["overdue"] else "")
            for bucket in orders["by_stage"]
        )
    )
    slots = pressure["slots"]
    if slots.get("measured"):
        print(
            f"slots      occupied={slots['occupied']} free={slots['free']} "
            f"quarantined={slots['quarantined']} unknown={slots['unknown']}"
        )
    else:
        print(f"slots      unmeasured  {slots.get('reason', '')}")
    print(f"queue      {pressure['queue_depth']}")
    filesystems = pressure["filesystems"]
    _print_fs_axis("worktree", filesystems["worktree"])
    _print_fs_axis("lane-target", filesystems["lane_target"])


def cmd_flakes(args: argparse.Namespace) -> None:
    journal = Journal(args.journal)
    now = now_unix_millis()
    try:
        orders = [order_summary(row, now) for row in journal.outstanding_orders()]
    except OperatorError:
        orders = []
    report = flakes_report(
        journal.records(),
        orders,
        args.worktree_base,
        getattr(args, "target_base", "") or "",
        now,
    )
    if args.json:
        print_json(report)
        return
    print_flakes_table(report)


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
    why.add_argument(
        "--ladder",
        action="store_true",
        help="print the recovery ladder even when the member is not wedged or overdue "
        "(otherwise the ladder is printed for those states)",
    )
    why.set_defaults(handler=cmd_why)

    evidence = subparsers.add_parser("evidence", help="the lane result summary for one dispatch nonce")
    evidence.add_argument("nonce", help="e.g. dispatch-1746, or just 1746")
    evidence.set_defaults(handler=cmd_evidence)

    slots = subparsers.add_parser(
        "slots", help="lane slots: occupied, free, quarantined, or unknown, with the identity that caused it"
    )
    slots.set_defaults(handler=cmd_slots)

    flakes = subparsers.add_parser(
        "flakes",
        help="recurring verifier and machinery signatures beside live queue/slot/filesystem pressure",
    )
    flakes.add_argument(
        "--target-base",
        default=os.environ.get("AETHER_BLOOMERY_LANE_TARGET_BASE", ""),
        help="per-slot cargo target root (env AETHER_BLOOMERY_LANE_TARGET_BASE); "
        "omit to leave that pressure axis unmeasured rather than assuming it shares the worktree volume",
    )
    flakes.set_defaults(handler=cmd_flakes)

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
    needs_journal = args.handler in (cmd_status, cmd_orders, cmd_why, cmd_flakes)
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
