#!/usr/bin/env python3
"""Regression tests for the Bloomery operator CLI's decoders.

Only the logic this script owns: the two wire decoders it re-implements against
the coordinator's storage format, the overdue arithmetic an operator reads a
sign off, the digest guard that keeps a malformed repair from reaching the
API, the status-table state that names a graph-held dependent, the
scratch-root slot listing / quarantine-clearing the operator uses to recover a
re-adopted lane child, the recovery ladder `why` prints for a wedged
member, and the flakes report that groups durable verifier/machinery
signatures beside live host pressure. Nothing here exercises urllib or
argparse's own parsing -- those are the standard library's to get right.
The ladder tests do feed a printed line back through this script's parser,
because "runnable verbatim" is the contract the printer owns.
"""

from __future__ import annotations

import importlib.util
import io
import json
import os
import shlex
import sqlite3
import struct
import tempfile
import unittest
from contextlib import redirect_stdout
from pathlib import Path
from unittest import mock


SCRIPT = Path(__file__).with_name("bloomery-operator.py")
SPEC = importlib.util.spec_from_file_location("bloomery_operator", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
operator = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(operator)


def wire_transformation(command: str, inputs: list[bytes], checkout: bytes, tail: bytes = b"") -> bytes:
    """A Transformation's leading fields in aether_data::wire encoding."""
    encoded = struct.pack("<I", len(command)) + command.encode("utf-8")
    encoded += struct.pack("<I", len(inputs)) + b"".join(inputs)
    encoded += checkout
    return encoded + tail


class StageIdDecoding(unittest.TestCase):
    """The `stage` column is a u32 variant index; the name is positional."""

    def test_the_vocabulary_matches_the_rust_declaration_order(self):
        # Tripwire: catches this list drifting from `stage_vocabulary!` in
        # crates/aether-bloomery/src/ids.rs. The index is positional, so a stage
        # inserted in the middle of the Rust enum silently renames every stage
        # after it here -- an operator would read "Refine" off a row that is
        # actually sitting at "Verify" and reach for the wrong recovery.
        self.assertEqual(
            operator.STAGES,
            (
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
            ),
        )

    def test_a_stage_blob_decodes_little_endian(self):
        # Tripwire: catches a big-endian read. Byte order is invisible for
        # index 0 (Sketch encodes identically either way) and wrong for every
        # other stage, so the bug would survive any test that only checked the
        # first variant.
        self.assertEqual(operator.stage_name(struct.pack("<I", 0)), "Sketch")
        self.assertEqual(operator.stage_name(struct.pack("<I", 4)), "Verify")
        self.assertEqual(operator.stage_name(struct.pack("<I", 12)), "Reconcile")
        # The same four bytes laid out big-endian must NOT resolve to Verify:
        # that is what a byte-order slip would produce, and it would produce it
        # for a stage the coordinator never dispatched.
        self.assertNotEqual(operator.stage_name(b"\x00\x00\x00\x04"), "Verify")

    def test_an_unrecognized_stage_renders_rather_than_raising(self):
        # Tripwire: catches an IndexError on a stage this vocabulary has not
        # caught up with. `orders` renders every row in one table -- one future
        # stage must not take the other rows down with it, because the table is
        # what the operator is reading to find the stuck one.
        self.assertEqual(operator.stage_name(struct.pack("<I", 99)), "unknown(99)")
        self.assertEqual(operator.stage_name(b"\x01\x02"), "unknown(len=2)")
        self.assertEqual(operator.stage_name(None), "unknown")


class TransformationDecoding(unittest.TestCase):
    """Recovering a CandidateRef's checkout from the stored transformation."""

    def test_it_recovers_command_inputs_and_checkout(self):
        # Tripwire: catches an offset slip in the three-field walk -- the
        # decoder's whole purpose is handing the operator a `checkout` to paste
        # into `repair`, and a checkout read one field early is a valid-looking
        # 64-hex string that names the wrong object.
        decoded = operator.decode_transformation(
            wire_transformation("construct.implement", [b"\x11" * 32, b"\x22" * 32], b"\x33" * 32)
        )
        self.assertEqual(decoded["command"], "construct.implement")
        self.assertEqual(decoded["inputs"], ["11" * 32, "22" * 32])
        self.assertEqual(decoded["checkout"], "33" * 32)

    def test_trailing_fields_do_not_disturb_the_leading_ones(self):
        # Tripwire: catches the decoder growing a dependency on the fields past
        # `checkout` (diff_base, outputs, image, limits, network, ...). Those
        # change with the line's vocabulary; the three this reads do not, and
        # coupling to them would make an unrelated Rust change break recovery.
        decoded = operator.decode_transformation(
            wire_transformation("verify.clippy", [], b"\xab" * 32, tail=b"\x01" + b"\xcd" * 32 + b"junk")
        )
        self.assertEqual(decoded["command"], "verify.clippy")
        self.assertEqual(decoded["checkout"], "ab" * 32)

    def test_a_truncated_or_absurd_blob_degrades_to_none(self):
        # Tripwire: catches a crash on an unexpected layout. `why` calls this
        # while diagnosing a member that is already broken, so a struct.error
        # here would take out the diagnosis the operator opened the tool for --
        # the exact moment the tool must still answer.
        self.assertIsNone(operator.decode_transformation(b""))
        self.assertIsNone(operator.decode_transformation(None))
        self.assertIsNone(operator.decode_transformation(b"\x05\x00\x00\x00ab"))
        self.assertIsNone(operator.decode_transformation(wire_transformation("x", [], b"\x00" * 31)))
        # A count that would index far past the blob is refused before it is
        # trusted into an allocation.
        self.assertIsNone(operator.decode_transformation(struct.pack("<I", 1) + b"c" + struct.pack("<I", 2**30)))


class OverdueArithmetic(unittest.TestCase):
    """The signed number an operator reads to tell wedged from waiting."""

    def test_the_sign_separates_overdue_from_still_running(self):
        # Tripwire: catches an inverted subtraction. Both directions are tested
        # because a flipped sign still produces a plausible-looking magnitude --
        # and reading it backwards is precisely the mistake that turns "wait
        # ninety seconds" into an unnecessary supersession.
        self.assertAlmostEqual(operator.overdue_secs(1_000_000, 1_060_000), 60.0)
        self.assertAlmostEqual(operator.overdue_secs(1_060_000, 1_000_000), -60.0)
        self.assertAlmostEqual(operator.overdue_secs(1_000_000, 1_000_000), 0.0)

    def test_the_rendering_keeps_the_sign_at_every_scale(self):
        # Tripwire: catches the sign being dropped by the unit-scaling branches.
        # abs() is taken before formatting, so a branch that forgets to re-apply
        # `sign` renders an overdue order and a healthy one identically.
        self.assertEqual(operator.human_secs(-30.0), "-30s")
        self.assertEqual(operator.human_secs(30.0), "+30s")
        self.assertEqual(operator.human_secs(-600.0), "-10.0m")
        self.assertEqual(operator.human_secs(600.0), "+10.0m")
        self.assertEqual(operator.human_secs(-7200.0), "-2.0h")
        self.assertEqual(operator.human_secs(7200.0), "+2.0h")


class DigestValidation(unittest.TestCase):
    """The guard that keeps a malformed override off the wire."""

    def test_only_64_hex_characters_are_a_digest(self):
        # Tripwire: catches a length-only or charset-only check. `repair` sends
        # two of these into a CandidateRef, and the API's refusal for a bad one
        # is a generic 400 -- so a digest that slips through costs a round trip
        # and an error message that does not say which flag was wrong.
        self.assertTrue(operator.is_digest_hex("ab" * 32))
        self.assertTrue(operator.is_digest_hex("AB" * 32))
        self.assertFalse(operator.is_digest_hex("ab" * 31))
        self.assertFalse(operator.is_digest_hex("ab" * 33))
        self.assertFalse(operator.is_digest_hex("g" + "a" * 63))
        self.assertFalse(operator.is_digest_hex(""))

    def test_require_digest_names_the_flag_it_refused(self):
        # Tripwire: catches the error losing the argument's identity. `repair`
        # takes two digests; "invalid digest" leaves the operator diffing both
        # by eye at the exact hour they are least able to.
        with self.assertRaises(operator.OperatorError) as caught:
            operator.require_digest("nope", "--checkout")
        self.assertIn("--checkout", str(caught.exception))
        self.assertEqual(operator.require_digest("AB" * 32, "--tree"), "ab" * 32)


class RepairPayload(unittest.TestCase):
    """Which source a repair command is allowed to name."""

    def test_from_commit_does_not_restate_the_digest_scheme(self):
        # Tripwire (#5032): --from-commit must send the sha, never a locally
        # hashed tree/checkout pair. Re-deriving here is how the last repair
        # spent six tool calls reconstructing the domain tags.
        payload = operator.repair_payload(None, None, "deadbeef" * 5, None)
        self.assertEqual(payload, {"from_commit": "deadbeef" * 5})
        self.assertNotIn("candidate", payload)
        self.assertNotIn("tree", payload)

    def test_the_low_level_pair_still_builds_a_candidate_struct(self):
        tree = "ab" * 32
        checkout = "cd" * 32
        payload = operator.repair_payload(tree, checkout, None, None)
        self.assertEqual(payload, {"candidate": {"tree": tree, "checkout": checkout}})

    def test_two_sources_or_a_half_pair_are_refused_by_name(self):
        # Tripwire: a mixed invocation must not silently prefer --from-commit
        # and drop the pair (or the other way around). The operator named two
        # intents; picking one would push the wrong candidate.
        with self.assertRaises(operator.OperatorError) as caught:
            operator.repair_payload("ab" * 32, "cd" * 32, "deadbeef" * 5, None)
        self.assertIn("--from-commit", str(caught.exception))
        with self.assertRaises(operator.OperatorError) as caught:
            operator.repair_payload("ab" * 32, None, None, None)
        self.assertIn("--tree", str(caught.exception))
        self.assertIn("--checkout", str(caught.exception))
        with self.assertRaises(operator.OperatorError) as caught:
            operator.repair_payload(None, None, None, None)
        self.assertIn("exactly one", str(caught.exception))


class EvidencePathResolution(unittest.TestCase):
    """Where a dispatch's lane evidence sits."""

    def test_a_bare_sequence_is_completed_to_a_nonce(self):
        # Tripwire: catches the bare-number spelling silently producing
        # `<base>/1746-evidence`, a path that never exists -- the operator
        # would read "no evidence" and conclude the lane wrote none.
        self.assertEqual(
            operator.evidence_path("/srv/lanes", "1746"),
            Path("/srv/lanes/dispatch-1746-evidence/evidence.json"),
        )
        self.assertEqual(
            operator.evidence_path("/srv/lanes", "dispatch-1746"),
            Path("/srv/lanes/dispatch-1746-evidence/evidence.json"),
        )


class EvidenceSummary(unittest.TestCase):
    """Projecting a lane's evidence.json into the columns an operator reads."""

    def test_a_run_that_recorded_nothing_summarizes_instead_of_raising(self):
        # Tripwire: catches an unguarded `value["result_record"]["..."]` walk.
        # A lane that died before writing its record is exactly the run an
        # operator reaches for `evidence` to explain, so a KeyError here fails
        # on the only input that matters.
        summary = operator.evidence_summary({})
        self.assertEqual(summary["command"], "unstated")
        self.assertFalse(summary["produced_candidate"])
        self.assertIsNone(summary["turns"])
        self.assertIsNone(summary["duration_secs"])
        self.assertIsNone(summary["result_text"])

    def test_duration_is_reported_in_seconds(self):
        # Tripwire: catches `duration_ms` being printed as-is. A 20-minute lap
        # rendered as "1200000s" is not a number anyone reads correctly at 2am.
        summary = operator.evidence_summary(
            {
                "command": "construct.implement",
                "produced_candidate": True,
                "result_record": {
                    "num_turns": 3,
                    "duration_ms": 1_200_000,
                    "output": 250,
                    "cost_usd": 0.01,
                    "result": {"result": "done"},
                },
            }
        )
        self.assertEqual(summary["duration_secs"], 1200.0)
        self.assertEqual(summary["turns"], 3)
        self.assertEqual(summary["output_tokens"], 250)
        self.assertEqual(summary["result_text"], "done")
        self.assertTrue(summary["produced_candidate"])


class MemberStatusState(unittest.TestCase):
    """The status table's one-word state, including a graph-held dependent."""

    def test_a_dependent_waiting_on_an_ancestor_is_blocked_not_idle(self):
        # The plausible bug: a member that has not entered the line because
        # its dependency has not resolved is painted `idle`, so an operator
        # reading `status` / `why` cannot tell a held subtree from a forgotten
        # dispatch.
        member = {"workpiece": "wp-b", "blocked_by": "wp-a"}
        self.assertEqual(operator.member_status_state(member, has_order=False), "blocked")
        self.assertEqual(operator.blocked_by_of(member), "wp-a")

    def test_a_running_root_is_not_blocked(self):
        # The plausible bug: `blocked_by` winning over an outstanding order
        # paints a dispatched root as waiting on itself (or on a stale ancestor).
        member = {"workpiece": "wp-a", "blocked_by": None}
        self.assertEqual(operator.member_status_state(member, has_order=True), "running")
        self.assertIsNone(operator.blocked_by_of(member))

    def test_wedge_and_resolution_outrank_a_stale_blocker(self):
        # The plausible bug: a resolved or wedged member still carrying a
        # leftover `blocked_by` from an older view shape is shown as blocked
        # instead of as finished / terminal.
        self.assertEqual(
            operator.member_status_state({"resolution": {"candidate": "aa"}, "blocked_by": "wp-a"}, has_order=False),
            "integrated",
        )
        self.assertEqual(
            operator.member_status_state({"wedge": {"stage": "Construct"}, "blocked_by": "wp-a"}, has_order=False),
            "WEDGED",
        )

    def test_an_empty_blocker_string_is_not_a_hold(self):
        # The plausible bug: a missing JSON field that arrived as "" is treated
        # as a named ancestor, so the table prints `blocked by` with no one.
        self.assertIsNone(operator.blocked_by_of({"blocked_by": ""}))
        self.assertEqual(operator.member_status_state({"blocked_by": ""}, has_order=False), "idle")


class LastMovement(unittest.TestCase):
    """Recovering a member's cursor from the journal."""

    def test_the_newest_outcome_naming_the_member_wins(self):
        # Tripwire: catches the scan running oldest-first. The journal is
        # append-only and a member passes through a stage repeatedly, so a
        # forward scan reports the member's FIRST stage as its cursor -- a
        # confident, stable, and completely wrong answer.
        records = [
            {"sequence": 1, "outcome": {"AttemptAdvanced": {"workpiece": "wp-a", "from": "Scope", "to": "Construct"}}},
            {"sequence": 2, "outcome": {"AttemptRetried": {"workpiece": "wp-b", "stage": "Verify", "attempt": 2}}},
            {"sequence": 3, "outcome": {"AttemptAdvanced": {"workpiece": "wp-a", "from": "Construct", "to": "Verify"}}},
        ]
        movement = operator.last_movement(records, "wp-a")
        self.assertEqual(movement["sequence"], 3)
        self.assertEqual(movement["stage"], "Verify")

    def test_a_retry_carries_its_attempt_count(self):
        # Tripwire: catches the attempt count being read off the wrong field.
        # `AttemptAdvanced` names `to` and `AttemptRetried` names `stage`; one
        # mapping for both would leave a retried member with no cursor at all.
        records = [{"sequence": 9, "outcome": {"AttemptRetried": {"workpiece": "wp-b", "stage": "Refine", "attempt": 3}}}]
        movement = operator.last_movement(records, "wp-b")
        self.assertEqual(movement["stage"], "Refine")
        self.assertEqual(movement["attempt"], 3)

    def test_unit_outcomes_and_other_members_are_skipped(self):
        # Tripwire: catches an unguarded `.items()` over `"Duplicate"` (a unit
        # variant serializes as a bare string, not an object) or a match on a
        # different member's row. Either would raise, or answer about the wrong
        # workpiece, on an ordinary journal.
        records = [
            {"sequence": 1, "outcome": "Duplicate"},
            {"sequence": 2, "outcome": {"AttemptWedged": {"workpiece": "other", "stage": "Verify"}}},
        ]
        self.assertIsNone(operator.last_movement(records, "wp-a"))


def write_slot_evidence(base: Path, nonce: str, slot: int, identity: dict[str, object] | str | None = None) -> None:
    evidence = base / f"{nonce}-evidence"
    evidence.mkdir()
    (evidence / "slot").write_text(f"{slot}\n", encoding="utf-8")
    if isinstance(identity, str):
        (evidence / "identity").write_text(identity, encoding="utf-8")
    elif identity is not None:
        (evidence / "identity").write_text(json.dumps(identity), encoding="utf-8")


def this_process_identity() -> dict[str, object]:
    identity = operator.observe_process(os.getpid())
    assert identity is not None
    return identity


DEAD_IDENTITY = {"pid": 999999, "pgid": 999999, "starttime": 1, "boot_id": "historical"}


class SlotListing(unittest.TestCase):
    """Projecting scratch-root slot files into the state an operator reads."""

    def test_a_quarantined_slot_names_its_identity_and_stays_distinct_from_occupied(self):
        # Tripwire: catches listing a quarantined slot as merely occupied (or
        # free). The operator reaches for this table to find the child a
        # restart could not kill; collapsing the states hides the one row
        # they came to clear.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-0").mkdir()
            (base / "slot-1").mkdir()
            live = this_process_identity()
            write_slot_evidence(base, "dispatch-9", 1, live)
            (base / "slot-0.quarantine").write_text(
                json.dumps(
                    {
                        "slot": 0,
                        "nonce": "dispatch-8",
                        "identity": {"pid": 22, "pgid": 22, "starttime": 200, "boot_id": "boot-a"},
                    }
                ),
                encoding="utf-8",
            )

            listed = {entry["slot"]: entry for entry in operator.list_slots(str(base))}

            self.assertEqual(listed[0]["state"], "quarantined")
            self.assertEqual(listed[0]["nonce"], "dispatch-8")
            self.assertEqual(listed[0]["identity"]["pid"], 22)
            self.assertEqual(listed[1]["state"], "occupied")
            self.assertEqual(listed[1]["nonce"], "dispatch-9")
            self.assertEqual(listed[1]["identity"]["pid"], live["pid"])

    def test_quarantine_outranks_a_live_occupant_on_the_same_slot(self):
        # Tripwire: a cancel that failed leaves both a quarantine file and a
        # still-live identity. The operator came to see the withheld slot;
        # collapsing it to occupied hides the file they need to clear.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-0").mkdir()
            write_slot_evidence(base, "dispatch-9", 0, this_process_identity())
            (base / "slot-0.quarantine").write_text(
                json.dumps(
                    {
                        "slot": 0,
                        "nonce": "dispatch-8",
                        "identity": {"pid": 22, "pgid": 22, "starttime": 200, "boot_id": "boot-a"},
                    }
                ),
                encoding="utf-8",
            )

            listed = operator.list_slots(str(base))

            self.assertEqual(len(listed), 1)
            self.assertEqual(listed[0]["state"], "quarantined")
            self.assertEqual(listed[0]["nonce"], "dispatch-8")
            self.assertEqual(listed[0]["identity"]["pid"], 22)
            self.assertIsNotNone(listed[0]["quarantine"])

    def test_a_checkout_with_no_dispatch_and_no_quarantine_is_free(self):
        # Tripwire: a slot checkout is reused across dispatches, so an idle
        # directory is the common case, not an error. Calling it occupied
        # would send the operator after a child that is not there.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-2").mkdir()
            listed = operator.list_slots(str(base))
            self.assertEqual(listed, [{"slot": 2, "state": "free", "nonce": None, "identity": None, "quarantine": None}])

    def test_the_live_identity_is_the_occupant_regardless_of_retained_history(self):
        # Tripwire: Wave 4/7. list_slots used to assign occupants[slot] in
        # iterdir order, so a retained dispatch-225-evidence could overwrite
        # the live dispatch-436 that /proc still shows in the slot. Occupancy
        # is a live identity match, not a leftover directory or a newer nonce.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-0").mkdir()
            (base / "slot-1").mkdir()
            live = this_process_identity()
            recycled = {**live, "starttime": live["starttime"] + 1}

            write_slot_evidence(base, "dispatch-436", 0, live)
            write_slot_evidence(base, "dispatch-225", 0, DEAD_IDENTITY)
            write_slot_evidence(base, "dispatch-237", 0, DEAD_IDENTITY)
            write_slot_evidence(base, "dispatch-272", 0, DEAD_IDENTITY)
            write_slot_evidence(base, "dispatch-100", 0, recycled)
            write_slot_evidence(base, "dispatch-200", 0, "not-json")

            write_slot_evidence(base, "dispatch-999", 1, DEAD_IDENTITY)
            write_slot_evidence(base, "dispatch-1000", 1, DEAD_IDENTITY)
            write_slot_evidence(base, "dispatch-1", 1, live)

            listed = {entry["slot"]: entry for entry in operator.list_slots(str(base))}

            self.assertEqual(listed[0]["state"], "occupied")
            self.assertEqual(listed[0]["nonce"], "dispatch-436")
            self.assertEqual(listed[0]["identity"]["pid"], live["pid"])
            self.assertEqual(listed[1]["state"], "occupied")
            self.assertEqual(listed[1]["nonce"], "dispatch-1")
            self.assertEqual(listed[1]["identity"]["pid"], live["pid"])

    def test_retained_dead_evidence_does_not_occupy_a_checkout(self):
        # Tripwire: evidence outlives the process. Treating a dead record as
        # occupied is how Wave 7 reported stale blockers on free slots.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-0").mkdir()
            write_slot_evidence(base, "dispatch-225", 0, DEAD_IDENTITY)
            write_slot_evidence(base, "dispatch-272", 0, DEAD_IDENTITY)

            listed = operator.list_slots(str(base))

            self.assertEqual(
                listed, [{"slot": 0, "state": "free", "nonce": None, "identity": None, "quarantine": None}]
            )

    def test_two_live_identities_for_one_slot_are_unknown(self):
        # Tripwire: two matching /proc identities cannot both be "the" occupant.
        # Picking either nonce (by name, age, or iterdir) is the same lie as
        # the historical overwrite — report unknown instead of guessing.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-0").mkdir()
            self_live = this_process_identity()
            init_live = operator.observe_process(1)
            self.assertIsNotNone(init_live, "pid 1 must be observable so the two live claims are distinct")
            write_slot_evidence(base, "dispatch-436", 0, self_live)
            write_slot_evidence(base, "dispatch-437", 0, init_live)

            listed = operator.list_slots(str(base))

            self.assertEqual(
                listed, [{"slot": 0, "state": "unknown", "nonce": None, "identity": None, "quarantine": None}]
            )

    def test_an_unreadable_identity_is_unknown_not_an_occupant(self):
        # Tripwire: a slot claim with no readable identity used to be occupied
        # on the directory name alone. That is a nonce with no process proof.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-0").mkdir()
            write_slot_evidence(base, "dispatch-225", 0, "not-json")
            write_slot_evidence(base, "dispatch-226", 0, None)

            listed = operator.list_slots(str(base))

            self.assertEqual(
                listed, [{"slot": 0, "state": "unknown", "nonce": None, "identity": None, "quarantine": None}]
            )


class QuarantineClearing(unittest.TestCase):
    """The operator door that releases a withheld slot."""

    def test_clearing_removes_the_file_and_states_what_was_checked(self):
        # Tripwire: catches a clear that deletes the file without saying what
        # it compared — the operator is asserting the child is gone, and the
        # command has to state the check it ran and that the rest is their
        # word. Also catches a clear that refuses when no matching process is
        # live, which is the usual case they are confirming.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            path = operator.slot_quarantine_path(str(base), 3)
            path.write_text(
                json.dumps(
                    {
                        "slot": 3,
                        "nonce": "dispatch-12",
                        "identity": {"pid": 1, "pgid": 1, "starttime": 0, "boot_id": "not-this-boot"},
                    }
                ),
                encoding="utf-8",
            )

            result = operator.clear_quarantine(str(base), 3)

            self.assertTrue(result["cleared"])
            self.assertFalse(result["matching_process_live"])
            self.assertIn("on your word", result["on_operator_word"])
            self.assertIn("/proc/1/stat", result["checked"])
            self.assertFalse(path.exists(), "clearing removes the file the allocator reads")

    def test_clearing_a_slot_that_is_not_quarantined_is_a_named_error(self):
        # Tripwire: a missing file must not look like success. The operator
        # would walk away thinking the slot was released when it was never
        # withheld — or, worse, when they typed the wrong index.
        with tempfile.TemporaryDirectory() as scratch:
            with self.assertRaises(operator.OperatorError) as caught:
                operator.clear_quarantine(scratch, 4)
            self.assertIn("slot 4", str(caught.exception))
            self.assertIn("not quarantined", str(caught.exception))


BLOOM = "4f" * 32
TREE = "aa" * 32
CHECKOUT = "bb" * 32
BASE = "01" * 32
MAINLINE = "02" * 32
GIT_SHA = "cd" * 20


def parse_printed(command: str) -> object:
    tokens = shlex.split(command)
    assert tokens[0] == operator.OPERATOR_SCRIPT
    return operator.build_parser().parse_args(tokens[1:])


def printed_ladder(rungs: list) -> str:
    buf = io.StringIO()
    with redirect_stdout(buf):
        operator.print_ladder(rungs)
    return buf.getvalue()


class JournalFactWalk(unittest.TestCase):
    """Recovering a bloom's base and a member's capture from journal records."""

    def test_digest_hex_accepts_both_api_spellings(self):
        # The plausible bug: a ladder that only accepted the API's hex form
        # would treat a 32-byte array (serde's default, a fixture that skipped
        # the hex adapter) as unknown and print the missing-input line over a
        # digest it already had.
        self.assertEqual(operator.digest_hex("AB" * 32), "ab" * 32)
        self.assertEqual(operator.digest_hex([0xAA] * 32), "aa" * 32)
        self.assertIsNone(operator.digest_hex("unavailable"))
        self.assertIsNone(operator.digest_hex([1, 2, 3]))

    def test_the_newest_seal_for_this_bloom_supplies_the_base(self):
        # The plausible bug: reading the first Seal in the journal, or any
        # Seal regardless of outcome, reports a predecessor's base as this
        # bloom's — so a successor whose mainline has not moved looks stale
        # (or a stale predecessor looks current).
        records = [
            {
                "sequence": 1,
                "event": {"fact": {"Seal": {"base": BASE}}},
                "outcome": {"Sealed": "11" * 32},
            },
            {
                "sequence": 2,
                "event": {"fact": {"Seal": {"base": "03" * 32}}},
                "outcome": {"Sealed": BLOOM},
            },
        ]
        self.assertEqual(operator.bloom_sealed_base(records, BLOOM), "03" * 32)
        self.assertIsNone(operator.bloom_sealed_base(records, "99" * 32))

        successor = [
            {
                "event": {"fact": {"Supersede": {"predecessor": "11" * 32, "successor": {"base": "04" * 32}}}},
                "outcome": {"Superseded": {"predecessor": "11" * 32, "successor": BLOOM}},
            }
        ]
        self.assertEqual(operator.bloom_sealed_base(successor, BLOOM), "04" * 32)

    def test_the_newest_capture_for_this_member_wins(self):
        # The plausible bug: a forward scan reports the first construct
        # capture, so a later refine or operator repair is ignored and the
        # repair line pastes a tree the member has already left.
        records = [
            {
                "event": {
                    "fact": {
                        "AttemptCompleted": {
                            "workpiece": "issue-5034",
                            "candidate": {"tree": "11" * 32, "checkout": "22" * 32},
                        }
                    }
                }
            },
            {
                "event": {
                    "fact": {
                        "AttemptCompleted": {
                            "workpiece": "issue-5034",
                            "candidate": {"tree": TREE, "checkout": CHECKOUT},
                        }
                    }
                }
            },
            {
                "event": {
                    "fact": {
                        "AttemptCompleted": {
                            "workpiece": "other",
                            "candidate": {"tree": "33" * 32, "checkout": "44" * 32},
                        }
                    }
                }
            },
        ]
        captured = operator.last_captured_candidate(records, "issue-5034")
        self.assertEqual(captured["tree"], TREE)
        self.assertEqual(captured["checkout"], CHECKOUT)

        repaired = operator.last_captured_candidate(
            [
                {
                    "event": {
                        "fact": {
                            "OperatorRepair": {
                                "repair": {
                                    "workpiece": "issue-5034",
                                    "candidate": {"tree": TREE, "checkout": CHECKOUT},
                                }
                            }
                        }
                    }
                }
            ],
            "issue-5034",
        )
        self.assertEqual(repaired["source"], "OperatorRepair.candidate")
        self.assertEqual(repaired["tree"], TREE)

    def test_a_wedge_does_not_erase_the_attempt_count_behind_it(self):
        # The plausible bug: newest-first stops at AttemptWedged, which has
        # no `attempt`, and the grant line has to invent a count — or print
        # none — at the exact moment the operator is asking how many rolls
        # were spent.
        records = [
            {"outcome": {"AttemptRetried": {"workpiece": "issue-5034", "stage": "Verify", "attempt": 3}}},
            {"outcome": {"AttemptWedged": {"workpiece": "issue-5034", "stage": "Verify"}}},
        ]
        self.assertEqual(operator.last_attempt_count(records, "issue-5034"), 3)


class ReviewParkPresentation(unittest.TestCase):
    """An otherwise idle sealed bloom's park in status/why."""

    QUESTION = "ab" * 32
    BLOOM = "4f" * 32

    def _parked_entry(self, *, resolved: bool = True) -> dict:
        park = {"question": self.QUESTION}
        if resolved:
            park.update(
                {
                    "stage": "AggregateReview",
                    "prompt": "delta-confirm still fails; accept the weave or file a follow-up?",
                    "options": ["accept — land as-is", "defer — file the finding forward"],
                    "blocked": "the bloom cannot land until the owner settles the review",
                }
            )
        extracted = operator.review_park_of({"review_park": park})
        return {
            "id": self.BLOOM,
            "status": "Sealed",
            "superseded_by": None,
            "landing_blocked": None,
            "executor_fault": None,
            "review_park": extracted,
            "adjudicate": operator.adjudicate_command(self.BLOOM, extracted["question"]),
            "members": [
                {
                    "workpiece": "issue-5055",
                    "cursor": None,
                    "state": "integrated",
                    "candidate": "aa" * 32,
                    "blocked_by": None,
                    "held_on_question": None,
                }
            ],
        }

    def test_an_idle_sealed_bloom_names_the_park_and_prints_adjudicate(self):
        # The plausible bug: status of a Sealed bloom with every member
        # integrated and no outstanding order still reads as idle, so the
        # operator never sees the digest the existing adjudicate route needs.
        entry = self._parked_entry()
        command = entry["adjudicate"]
        parsed = parse_printed(command)
        self.assertEqual(parsed.command, "adjudicate")
        self.assertEqual(parsed.bloom, self.BLOOM)
        self.assertEqual(parsed.finding, [self.QUESTION])
        self.assertEqual(parsed.reason, "<reason>")
        self.assertEqual(parsed.operator, "<operator>")
        self.assertIsNone(parsed.defer)

        buf = io.StringIO()
        with redirect_stdout(buf):
            operator.print_status_bloom(entry)
        text = buf.getvalue()
        self.assertIn("REVIEW PARK", text)
        self.assertIn(self.QUESTION, text)
        self.assertIn(command, text)
        self.assertIn("issue-5055", text)
        self.assertIn("integrated", text)
        self.assertNotIn("HELD", text)

    def test_a_digest_only_park_is_still_actionable(self):
        # The plausible bug: a live REST view that cannot resolve the question
        # artifact drops the park, so the recovery line is missing exactly
        # when the operator has only the digest.
        entry = self._parked_entry(resolved=False)
        self.assertEqual(entry["review_park"]["question"], self.QUESTION)
        self.assertIsNone(entry["review_park"]["prompt"])
        parsed = parse_printed(entry["adjudicate"])
        self.assertEqual(parsed.finding, [self.QUESTION])

        buf = io.StringIO()
        with redirect_stdout(buf):
            operator.print_review_park(entry["review_park"], self.BLOOM)
        text = buf.getvalue()
        self.assertIn(self.QUESTION, text)
        self.assertIn(entry["adjudicate"], text)

    def test_a_member_hold_or_executor_fault_is_not_a_review_park(self):
        # The plausible bug: any pending_decision or executor_fault is painted
        # as the bloom-scoped park, so why prints an adjudicate line for a
        # question the bloom-scope door will refuse.
        self.assertIsNone(
            operator.review_park_of(
                {
                    "executor_fault": {"rolls": 2, "budget": 2, "terminal": True},
                    "members": [{"pending_decision": {"question": self.QUESTION, "stage": "Construct"}}],
                }
            )
        )
        self.assertIsNone(operator.review_park_of({"review_park": {"question": "not-a-digest"}}))
        self.assertIsNone(operator.review_park_of({}))


class RecoveryLadder(unittest.TestCase):
    """The rungs `why` prints for a wedged or overdue member."""

    def _ladder(self, **overrides):
        values = {
            "bloom_id": BLOOM,
            "workpiece": "issue-5034",
            "wedged_at": "Verify",
            "overdue": False,
            "force": False,
            "attempt_count": 3,
            "sealed_base": BASE,
            "mainline": BASE,
            "captured": {"tree": TREE, "checkout": CHECKOUT, "source": "AttemptCompleted.candidate"},
            "produced_candidate": True,
            "correspondence": {CHECKOUT: GIT_SHA},
            "stranded": ["issue-4900"],
        }
        values.update(overrides)
        return operator.recovery_ladder(**values)

    def test_a_wedged_member_prints_a_runnable_grant_line(self):
        # The plausible bug: the grant line is a reminder (`grant for another
        # attempt`) that still makes the operator look up the bloom id and
        # invent --attempts. A diagnosis that does not assemble the invocation
        # is the improvisation this rung exists to retire.
        rungs = self._ladder()
        grant = next(rung for rung in rungs if rung["verb"] == "grant")
        parsed = parse_printed(grant["command"])
        self.assertEqual(parsed.command, "grant")
        self.assertEqual(parsed.bloom, BLOOM)
        self.assertEqual(parsed.workpiece, "issue-5034")
        self.assertEqual(parsed.attempts, 1)
        self.assertEqual(parsed.stage, "Verify")
        self.assertIn(grant["command"], printed_ladder(rungs))

    def test_a_captured_candidate_prints_a_runnable_repair_line(self):
        # The plausible bug: repair is printed as a verb with no digests, so
        # the operator still has to walk backend_correspondence by hand — the
        # exact archaeology the ladder is supposed to have done. The printed
        # line must parse and satisfy the repair contract (exactly one source).
        rungs = self._ladder()
        repair = next(rung for rung in rungs if rung["verb"] == "repair")
        parsed = parse_printed(repair["command"])
        self.assertEqual(parsed.command, "repair")
        payload = operator.repair_payload(parsed.tree, parsed.checkout, parsed.from_commit, parsed.from_worktree)
        self.assertEqual(payload, {"candidate": {"tree": TREE, "checkout": CHECKOUT}})
        self.assertIn(GIT_SHA, " ".join(repair["notes"]))

    def test_a_checkout_correspondence_alone_prints_from_commit(self):
        # The plausible bug: repair waits for both 64-hex halves before
        # offering any command, so a journal that only kept the capture
        # commit — the half backend_correspondence can turn into a sha —
        # prints a template the operator still cannot run. After #5032 the
        # chassis derives the pair from that sha.
        rungs = self._ladder(
            captured={"tree": None, "checkout": CHECKOUT, "source": "AttemptCompleted.candidate"},
            correspondence={CHECKOUT: GIT_SHA},
        )
        repair = next(rung for rung in rungs if rung["verb"] == "repair")
        parsed = parse_printed(repair["command"])
        payload = operator.repair_payload(parsed.tree, parsed.checkout, parsed.from_commit, parsed.from_worktree)
        self.assertEqual(payload, {"from_commit": GIT_SHA})

    def test_an_unknown_digest_names_the_producing_step(self):
        # The plausible bug: a missing tree or checkout drops the repair rung
        # (or silently omits the flag), so the operator cannot tell whether
        # there is no capture or the journal just does not have the digest
        # yet. The rung stays, and the miss names the step that would have
        # written it.
        rungs = self._ladder(captured=None, produced_candidate=True, correspondence={})
        repair = next(rung for rung in rungs if rung["verb"] == "repair")
        notes = " ".join(repair["notes"])
        self.assertIn("unknown to the journal", notes)
        self.assertIn("AttemptCompleted.candidate.tree", notes)
        self.assertIn("AttemptCompleted.candidate.checkout", notes)
        self.assertIn("backend_correspondence", notes)

    def test_a_stale_base_names_the_members_a_supersede_would_strand(self):
        # The plausible bug: supersede is suggested as a verb with no
        # accounting of the claims already on the bloom, so the operator
        # mints a successor and only then discovers which integrated
        # candidates it stranded.
        rungs = self._ladder(mainline=MAINLINE)
        supersede = next(rung for rung in rungs if rung["verb"] == "supersede")
        parsed = parse_printed(supersede["command"].replace("<draft>", "1"))
        self.assertEqual(parsed.command, "supersede")
        self.assertEqual(parsed.bloom, BLOOM)
        self.assertIn("issue-4900", " ".join(supersede["notes"]))
        self.assertIn("stale", supersede["because"])

    def test_an_idle_member_does_not_grow_a_ladder(self):
        # The plausible bug: every `why` ends in grant/repair, so a member
        # that is waiting on an ancestor or still inside its window is
        # presented as wedged. The ladder is a recovery surface, not a
        # decoration on the diagnosis.
        self.assertEqual(
            self._ladder(wedged_at=None, overdue=False, force=False, captured=None, produced_candidate=False),
            [],
        )


class CorrespondenceLookup(unittest.TestCase):
    """Reading a digest's backend object out of the coordinator's store."""

    def test_a_recorded_row_renders_as_hex_and_a_miss_is_none(self):
        # The plausible bug: a missing table or a missing row raises, and
        # `why` dies on the diagnosis the operator opened it for. A miss is
        # None so the ladder can name the producing step instead.
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "store.db"
            conn = sqlite3.connect(path)
            conn.execute(
                "CREATE TABLE backend_correspondence ("
                "digest BLOB NOT NULL PRIMARY KEY, backend_object BLOB NOT NULL UNIQUE)"
            )
            conn.execute(
                "INSERT INTO backend_correspondence VALUES (?, ?)",
                (bytes.fromhex(CHECKOUT), bytes.fromhex(GIT_SHA)),
            )
            conn.commit()
            conn.close()

            journal = operator.Journal(str(path))
            self.assertEqual(journal.correspondence_hex(CHECKOUT), GIT_SHA)
            self.assertIsNone(journal.correspondence_hex(TREE), "a digest with no row is a miss, not an error")

    def test_a_journal_without_the_table_is_a_miss(self):
        # The plausible bug: an older store that never grew
        # backend_correspondence takes `why` down with an OperationalError
        # at the exact moment the operator needs the rest of the diagnosis.
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "store.db"
            sqlite3.connect(path).close()
            self.assertIsNone(operator.Journal(str(path)).correspondence_hex(CHECKOUT))


BLOOM_A = "aa" * 32
DETAIL_A = "11" * 32
DETAIL_B = "22" * 32
SUBJECT = "33" * 32


def verify_failed_fact(workpiece, verifiers, bloom=BLOOM_A, detail=DETAIL_A, kind="VerificationResult"):
    return {
        "VerifyFailed": {
            "bloom": bloom,
            "workpiece": workpiece,
            "failed_verifiers": verifiers,
            "evidence": {"subject": SUBJECT, "kind": kind, "detail": detail},
            "stderr": "clippy 2026-08-17T00:00:00Z /tmp/slot-0/src/lib.rs:1",
        }
    }


def machinery_fact(workpiece, stage="Verify", bloom=BLOOM_A, detail=DETAIL_B):
    return {
        "MemberMachineryFault": {
            "bloom": bloom,
            "workpiece": workpiece,
            "stage": stage,
            "evidence": {"subject": SUBJECT, "kind": "ExecutorFault", "detail": detail},
        }
    }


def journal_record(sequence, fact, outcome=None):
    return {"sequence": sequence, "event": {"fact": fact}, "outcome": outcome}


def wire_string(text: str) -> bytes:
    encoded = text.encode("utf-8")
    return struct.pack("<I", len(encoded)) + encoded


def wire_verify_failed_event(key, bloom, workpiece, verifiers, subject=SUBJECT, detail=DETAIL_A, kind=1) -> bytes:
    body = bytes.fromhex(bloom) + wire_string(workpiece)
    body += bytes.fromhex(subject) + struct.pack("<I", kind) + bytes.fromhex(detail)
    body += struct.pack("<I", len(verifiers))
    for name in verifiers:
        body += wire_string(name)
    return wire_string(key) + struct.pack("<I", 13) + body


def open_operator_store(path: Path) -> sqlite3.Connection:
    conn = sqlite3.connect(path)
    conn.execute(
        "CREATE TABLE journal ("
        "sequence INTEGER PRIMARY KEY AUTOINCREMENT, "
        "idempotency_key TEXT NOT NULL UNIQUE, "
        "event BLOB NOT NULL, "
        "decisions BLOB, "
        "decider TEXT, "
        "decisions_schema TEXT)"
    )
    conn.execute(
        "CREATE TABLE outstanding_orders ("
        "nonce TEXT PRIMARY KEY, "
        "bloom BLOB NOT NULL, "
        "workpiece TEXT NOT NULL, "
        "scope_revision BLOB NOT NULL, "
        "candidate BLOB NOT NULL, "
        "displayed_digest BLOB NOT NULL, "
        "stage BLOB NOT NULL, "
        "transformation BLOB NOT NULL, "
        "configs BLOB NOT NULL, "
        "profile BLOB NOT NULL, "
        "deadline_unix_millis INTEGER NOT NULL)"
    )
    return conn


def insert_journal_json(conn: sqlite3.Connection, key: str, fact: dict, outcome) -> int:
    decisions = None if outcome is None else json.dumps(outcome).encode("utf-8")
    cursor = conn.execute(
        "INSERT INTO journal (idempotency_key, event, decisions) VALUES (?, ?, ?)",
        (key, json.dumps({"idempotency_key": key, "fact": fact}).encode("utf-8"), decisions),
    )
    return cursor.lastrowid


def insert_order(conn: sqlite3.Connection, *, nonce, workpiece, stage_index, deadline, bloom=BLOOM_A) -> None:
    digest = bytes.fromhex(bloom)
    conn.execute(
        "INSERT INTO outstanding_orders VALUES (?,?,?,?,?,?,?,?,?,?,?)",
        (
            nonce,
            digest,
            workpiece,
            digest,
            digest,
            digest,
            struct.pack("<I", stage_index),
            wire_transformation("verify.clippy", [], digest),
            b"",
            b"",
            deadline,
        ),
    )


class FakeStat:
    def __init__(self, device: int) -> None:
        self.st_dev = device


class FakeVfs:
    def __init__(self, total: int, free: int, fragment: int = 4096) -> None:
        self.f_frsize = fragment
        self.f_blocks = total // fragment
        self.f_bavail = free // fragment


class FlakeSignatures(unittest.TestCase):
    """Grouping admitted verifier and machinery failures into stable signatures."""

    def test_repeated_verifier_failures_share_one_signature_across_artifacts(self):
        # The plausible bug: grouping on evidence.detail (a per-run digest) or
        # on the free-form stderr the fixture carries would make every clippy
        # miss look unique, so recurrence — the whole point of the report —
        # would never appear.
        records = [
            journal_record(4, verify_failed_fact("issue-1", ["verify.clippy"], detail=DETAIL_A)),
            journal_record(9, verify_failed_fact("issue-1", ["verify.test", "verify.clippy"], detail=DETAIL_B)),
            journal_record(12, verify_failed_fact("issue-2", ["verify.clippy"], detail="99" * 32)),
        ]
        signatures = operator.group_flake_signatures(records)
        clippy = next(item for item in signatures if item["cause"] == "verify.clippy")
        self.assertEqual(clippy["count"], 2)
        self.assertEqual(clippy["kind"], "verifier")
        self.assertEqual(clippy["stage"], "Verify")
        self.assertEqual(clippy["workpieces"], ["issue-1", "issue-2"])
        self.assertEqual(clippy["first_sequence"], 4)
        self.assertEqual(clippy["last_sequence"], 12)
        self.assertEqual(clippy["machinery_retries"], 0)

    def test_distinct_verifier_sets_stay_distinct(self):
        # The plausible bug: joining verifier names without canonicalizing
        # order collapses `{test,clippy}` and `{clippy,test}` into two rows,
        # or the reverse — flattening every VerifyFailed into one "verify"
        # bucket so a clippy flake and a test flake cannot be told apart.
        records = [
            journal_record(1, verify_failed_fact("issue-1", ["verify.test", "verify.clippy"])),
            journal_record(2, verify_failed_fact("issue-1", ["verify.clippy", "verify.test"])),
            journal_record(3, verify_failed_fact("issue-1", ["verify.fmt"])),
        ]
        signatures = operator.group_flake_signatures(records)
        causes = [item["cause"] for item in signatures]
        self.assertEqual(causes, ["verify.clippy,verify.test", "verify.fmt"])
        self.assertEqual(signatures[0]["count"], 2)
        self.assertEqual(signatures[0]["verifiers"], ["verify.clippy", "verify.test"])

    def test_machinery_stays_distinct_from_verifier_failures(self):
        # The plausible bug: a MemberMachineryFault at Verify is folded into
        # the verifier row for that stage, so a sick host is reported as a
        # candidate defect (or the reverse).
        records = [
            journal_record(1, verify_failed_fact("issue-1", ["verify.clippy"])),
            journal_record(
                2,
                machinery_fact("issue-1"),
                {"MemberMachineryRetried": {"bloom": BLOOM_A, "workpiece": "issue-1", "stage": "Verify", "rolls": 1}},
            ),
        ]
        signatures = operator.group_flake_signatures(records)
        self.assertEqual({item["kind"] for item in signatures}, {"verifier", "machinery"})
        machinery = next(item for item in signatures if item["kind"] == "machinery")
        self.assertEqual(machinery["cause"], "executor_fault")
        self.assertEqual(machinery["verifiers"], [])
        self.assertEqual(machinery["stage"], "Verify")

    def test_machinery_retries_and_wedges_use_rolls_without_double_counting(self):
        # The plausible bug: summing each outcome's `rolls` counts the same
        # retry series as 1+2+3, or incrementing per event and then taking
        # max(rolls) double-counts a series that already numbered its rolls.
        records = [
            journal_record(
                1,
                machinery_fact("issue-1"),
                {"MemberMachineryRetried": {"workpiece": "issue-1", "stage": "Verify", "rolls": 1}},
            ),
            journal_record(
                2,
                machinery_fact("issue-1"),
                {"MemberMachineryRetried": {"workpiece": "issue-1", "stage": "Verify", "rolls": 2}},
            ),
            journal_record(
                3,
                machinery_fact("issue-1"),
                {"MemberMachineryWedged": {"workpiece": "issue-1", "stage": "Verify", "rolls": 2}},
            ),
        ]
        signatures = operator.group_flake_signatures(records)
        self.assertEqual(len(signatures), 1)
        self.assertEqual(signatures[0]["count"], 3)
        self.assertEqual(signatures[0]["machinery_retries"], 2)
        self.assertEqual(signatures[0]["machinery_wedges"], 1)

    def test_a_refused_or_duplicate_fact_is_not_an_observation(self):
        # The plausible bug: grouping every VerifyFailed *fact* counts
        # intake refusals and idempotent replays as recurrences.
        records = [
            journal_record(1, verify_failed_fact("issue-1", ["verify.clippy"]), "Duplicate"),
            journal_record(
                2,
                verify_failed_fact("issue-1", ["verify.clippy"]),
                {"VerifyFailedRejected": {"reason": "stale"}},
            ),
            journal_record(3, verify_failed_fact("issue-1", ["verify.clippy"])),
        ]
        signatures = operator.group_flake_signatures(records)
        self.assertEqual(len(signatures), 1)
        self.assertEqual(signatures[0]["count"], 1)
        self.assertEqual(signatures[0]["first_sequence"], 3)

    def test_empty_history_is_an_empty_successful_report(self):
        # The plausible bug: zero rows is treated as an error, so a fresh
        # journal (or a wave that has not yet failed) cannot be inspected.
        self.assertEqual(operator.group_flake_signatures([]), [])
        report = operator.flakes_report([], [], "", "", 1_000)
        self.assertEqual(report["signatures"], [])
        self.assertFalse(report["pressure"]["slots"]["measured"])
        self.assertFalse(report["pressure"]["filesystems"]["lane_target"]["measured"])


class FlakeJournalDecoding(unittest.TestCase):
    """Sequence-stable journal reads and the wire path a live store uses."""

    def test_fact_and_outcome_indexes_match_the_rust_declaration_order(self):
        # Tripwire: a Fact/Outcome inserted in the middle of the Rust enum
        # silently renames every later variant here. VerifyFailed landing on
        # the wrong index would group an unrelated fact as a verifier flake.
        self.assertEqual(operator.FACT_NAMES[13], "VerifyFailed")
        self.assertEqual(operator.FACT_NAMES[16], "AggregateReviewExecutorFault")
        self.assertEqual(operator.OUTCOME_NAMES[41], "AggregateReviewExecutorFaulted")
        self.assertEqual(operator.OUTCOME_NAMES[42], "AggregateReviewExecutorWedged")

    def test_journal_sequences_survive_a_reopen(self):
        # The plausible bug: grouping by Python insertion order or by rowid
        # after a vacuum, so a restart rewrites first/last and a recurrence
        # looks like a new signature.
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "store.db"
            conn = open_operator_store(path)
            first = insert_journal_json(
                conn, "k1", verify_failed_fact("issue-1", ["verify.clippy"]), {"RefineReentered": {"rolls": 0}}
            )
            second = insert_journal_json(
                conn, "k2", verify_failed_fact("issue-1", ["verify.clippy"]), {"RefineReentered": {"rolls": 1}}
            )
            conn.commit()
            conn.close()

            first_pass = operator.group_flake_signatures(operator.Journal(str(path)).records())
            reopened = operator.group_flake_signatures(operator.Journal(str(path)).records())

            self.assertEqual(first_pass[0]["first_sequence"], first)
            self.assertEqual(first_pass[0]["last_sequence"], second)
            self.assertEqual(reopened[0]["first_sequence"], first)
            self.assertEqual(reopened[0]["last_sequence"], second)
            self.assertEqual(first_pass[0]["count"], 2)

    def test_a_wire_encoded_verify_failed_decodes_as_the_json_shape(self):
        # The plausible bug: only the JSON fixture path works, so a live
        # coordinator journal (canonical wire bytes) reports no signatures.
        blob = wire_verify_failed_event("k", BLOOM_A, "issue-7", ["verify.test", "verify.clippy"])
        decoded = operator.decode_event_blob(blob)
        fact = decoded["fact"]["VerifyFailed"]
        self.assertEqual(fact["workpiece"], "issue-7")
        self.assertEqual(fact["bloom"], BLOOM_A)
        self.assertEqual(fact["failed_verifiers"], ["verify.test", "verify.clippy"])
        self.assertEqual(fact["evidence"]["kind"], "VerificationResult")

        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "store.db"
            conn = open_operator_store(path)
            conn.execute(
                "INSERT INTO journal (idempotency_key, event, decisions) VALUES (?, ?, ?)",
                ("wire-1", blob, json.dumps({"RefineReentered": {"rolls": 1}}).encode("utf-8")),
            )
            conn.commit()
            conn.close()
            signatures = operator.group_flake_signatures(operator.Journal(str(path)).records())
            self.assertEqual(signatures[0]["cause"], "verify.clippy,verify.test")
            self.assertEqual(signatures[0]["workpieces"], ["issue-7"])


class FlakePressure(unittest.TestCase):
    """Live queue / slot / filesystem pressure, without walking a target tree."""

    def test_missing_target_base_is_unmeasured_not_assumed_shared(self):
        # The plausible bug: an omitted lane-target base is reported as the
        # worktree volume, so an operator cannot tell whether that axis was
        # measured.
        filesystems = operator.live_filesystems("/does-not-need-to-exist-for-this", "")
        self.assertFalse(filesystems["lane_target"]["measured"])
        self.assertIn("not supplied", filesystems["lane_target"]["reason"])
        self.assertNotIn("shared_with", filesystems["lane_target"])

    def test_shared_and_distinct_devices_are_labelled_differently(self):
        # The plausible bug: two bases on one device are double-counted as
        # two volumes, or two devices are collapsed because the paths share
        # a parent. The axis is unmeasured when it shares the worktree
        # filesystem, and measured only when the device differs.
        worktree = "/worktree"
        target = "/targets"

        def stat(path):
            return FakeStat(1 if path == worktree else 2)

        def shared_stat(_path):
            return FakeStat(7)

        vfs = FakeVfs(total=10 * 1024 * 1024, free=4 * 1024 * 1024)
        with mock.patch("os.stat", side_effect=stat), mock.patch("os.statvfs", return_value=vfs):
            distinct = operator.live_filesystems(worktree, target)
        self.assertTrue(distinct["worktree"]["measured"])
        self.assertTrue(distinct["lane_target"]["measured"])
        self.assertEqual(distinct["lane_target"]["free_bytes"], 4 * 1024 * 1024)
        self.assertNotIn("shared_with", distinct["lane_target"])

        with mock.patch("os.stat", side_effect=shared_stat), mock.patch("os.statvfs", return_value=vfs):
            shared = operator.live_filesystems(worktree, target)
        self.assertTrue(shared["worktree"]["measured"])
        self.assertFalse(shared["lane_target"]["measured"])
        self.assertEqual(shared["lane_target"]["shared_with"], "worktree")

    def test_quarantined_and_unknown_slots_are_counted(self):
        # The plausible bug: quarantine and unknown collapse into occupied
        # (or free), so the pressure line cannot show a withheld slot.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-0").mkdir()
            (base / "slot-1").mkdir()
            (base / "slot-2").mkdir()
            write_slot_evidence(base, "dispatch-1", 0, this_process_identity())
            write_slot_evidence(base, "dispatch-2", 1, "not-json")
            (base / "slot-2.quarantine").write_text(
                json.dumps({"slot": 2, "nonce": "dispatch-3", "identity": DEAD_IDENTITY}),
                encoding="utf-8",
            )
            pressure = operator.slot_pressure(str(base))
            self.assertTrue(pressure["measured"])
            self.assertEqual(pressure["occupied"], 1)
            self.assertEqual(pressure["unknown"], 1)
            self.assertEqual(pressure["quarantined"], 1)
            self.assertEqual(pressure["free"], 0)

    def test_overdue_orders_are_split_from_those_still_inside_the_window(self):
        # The plausible bug: every outstanding order is painted overdue (or
        # none are), so the pressure line cannot tell a deadline stall from
        # a healthy queue.
        now = 1_000_000
        orders = [
            {"stage": "Verify", "overdue_secs": 30.0},
            {"stage": "Verify", "overdue_secs": -10.0},
            {"stage": "Construct", "overdue_secs": -5.0},
        ]
        pressure = operator.order_pressure(orders)
        self.assertEqual(pressure["outstanding"], 3)
        self.assertEqual(pressure["overdue"], 1)
        by_stage = {bucket["stage"]: bucket for bucket in pressure["by_stage"]}
        self.assertEqual(by_stage["Verify"], {"stage": "Verify", "outstanding": 2, "overdue": 1})
        self.assertEqual(by_stage["Construct"]["overdue"], 0)
        live = operator.live_pressure(orders, "", "", now)
        self.assertEqual(live["queue_depth"], 3)
        self.assertEqual(live["observed_unix_millis"], now)

    def test_pressure_does_not_walk_a_target_tree_or_call_the_network(self):
        # The plausible bug: sizing the lane-target axis by walking cargo
        # target contents (millions of files) or reaching the coordinator /
        # a provider to render a read-only report.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            target = base / "targets"
            (target / "slot-0-target" / "debug").mkdir(parents=True)
            (target / "slot-0-target" / "debug" / "huge.rlib").write_bytes(b"x")
            walked = []

            def walk(*_args, **_kwargs):
                walked.append("walk")
                raise AssertionError("target walk")

            def rglob(self, pattern):
                walked.append(f"rglob:{pattern}")
                raise AssertionError("target rglob")

            vfs = FakeVfs(total=8 * 1024 * 1024, free=3 * 1024 * 1024)
            with (
                mock.patch("os.walk", side_effect=walk),
                mock.patch.object(Path, "rglob", rglob),
                mock.patch.object(operator.urllib.request, "urlopen", side_effect=AssertionError("network")),
                mock.patch("os.statvfs", return_value=vfs),
            ):
                filesystems = operator.live_filesystems(str(base), str(target))
                report = operator.flakes_report([], [], str(base), str(target), 1)

            self.assertEqual(walked, [])
            self.assertTrue(filesystems["worktree"]["measured"])
            self.assertFalse(filesystems["lane_target"]["measured"])
            self.assertEqual(filesystems["lane_target"]["shared_with"], "worktree")
            self.assertEqual(report["signatures"], [])

    def test_json_is_stable_and_the_command_never_opens_a_provider(self):
        # The plausible bug: field order or unsorted workpieces makes two
        # identical journals render as different documents, or `flakes`
        # constructs the REST client the other read commands use.
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "store.db"
            conn = open_operator_store(path)
            insert_journal_json(conn, "k1", verify_failed_fact("issue-2", ["verify.clippy"]), None)
            insert_journal_json(conn, "k2", verify_failed_fact("issue-1", ["verify.clippy"]), None)
            insert_journal_json(
                conn,
                "k3",
                machinery_fact("issue-1"),
                {"MemberMachineryWedged": {"workpiece": "issue-1", "stage": "Verify", "rolls": 2}},
            )
            insert_order(conn, nonce="dispatch-1", workpiece="issue-1", stage_index=4, deadline=500)
            conn.commit()
            conn.close()

            journal = operator.Journal(str(path))
            now = 1_000
            orders = [operator.order_summary(row, now) for row in journal.outstanding_orders()]
            report = operator.flakes_report(journal.records(), orders, "", "", now)
            dumped = json.dumps(report, indent=2)
            again = json.dumps(operator.flakes_report(journal.records(), orders, "", "", now), indent=2)
            self.assertEqual(dumped, again)
            clippy = next(item for item in report["signatures"] if item["kind"] == "verifier")
            self.assertEqual(clippy["workpieces"], ["issue-1", "issue-2"])
            self.assertEqual(report["pressure"]["orders"]["overdue"], 1)
            self.assertFalse(report["pressure"]["filesystems"]["lane_target"]["measured"])

            stdout = io.StringIO()
            with (
                mock.patch.object(operator.urllib.request, "urlopen", side_effect=AssertionError("network")),
                mock.patch.object(operator, "Api", side_effect=AssertionError("provider")),
                redirect_stdout(stdout),
            ):
                status = operator.main(["--journal", str(path), "--json", "flakes"])
            self.assertEqual(status, 0)
            rendered = json.loads(stdout.getvalue())
            self.assertEqual(rendered["signatures"], report["signatures"])
            self.assertIn("no durable signatures" if not rendered["signatures"] else "verify.clippy", dumped)

    def test_the_empty_table_names_that_nothing_has_been_observed(self):
        # The plausible bug: an empty journal prints a blank table, so the
        # operator cannot tell a successful zero from a command that failed
        # to read.
        stdout = io.StringIO()
        with tempfile.TemporaryDirectory() as scratch:
            path = Path(scratch) / "store.db"
            open_operator_store(path).close()
            with redirect_stdout(stdout):
                status = operator.main(["--journal", str(path), "flakes"])
        self.assertEqual(status, 0)
        self.assertIn("no durable signatures have yet been observed", stdout.getvalue())


if __name__ == "__main__":
    unittest.main()
