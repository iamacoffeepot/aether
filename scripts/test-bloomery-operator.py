#!/usr/bin/env python3
"""Regression tests for the Bloomery operator CLI's decoders.

Only the logic this script owns: the two wire decoders it re-implements against
the coordinator's storage format, the overdue arithmetic an operator reads a
sign off, the digest guard that keeps a malformed repair from reaching the
API, and the scratch-root slot listing / quarantine-clearing the operator uses
to recover a re-adopted lane child. Nothing here exercises urllib, sqlite3, or
argparse -- those are the standard library's to get right.
"""

from __future__ import annotations

import importlib.util
import json
import struct
import tempfile
import unittest
from pathlib import Path


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
            evidence = base / "dispatch-9-evidence"
            evidence.mkdir()
            (evidence / "slot").write_text("1\n", encoding="utf-8")
            (evidence / "identity").write_text(
                json.dumps({"pid": 11, "pgid": 11, "starttime": 100, "boot_id": "boot-a"}),
                encoding="utf-8",
            )
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
            self.assertEqual(listed[1]["identity"]["pid"], 11)

    def test_a_checkout_with_no_dispatch_and_no_quarantine_is_free(self):
        # Tripwire: a slot checkout is reused across dispatches, so an idle
        # directory is the common case, not an error. Calling it occupied
        # would send the operator after a child that is not there.
        with tempfile.TemporaryDirectory() as scratch:
            base = Path(scratch)
            (base / "slot-2").mkdir()
            listed = operator.list_slots(str(base))
            self.assertEqual(listed, [{"slot": 2, "state": "free", "nonce": None, "identity": None, "quarantine": None}])


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


if __name__ == "__main__":
    unittest.main()
