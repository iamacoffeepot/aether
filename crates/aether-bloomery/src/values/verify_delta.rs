//! Which gates a delta can affect (ADR-0200, amendment of 2026-09-15).
//!
//! ADR-0200 makes a verify verdict a fact about content: a green proof over
//! tree `T` under gate set `G` answers every later verify of `T` under `G`.
//! That reuse is all-or-nothing on the *whole* tree, so a repair lap that
//! changed one comment line falls off it entirely and buys the whole umbrella
//! again — measured on bloom `0f16e207`, where issue-6023 was red only on
//! `verify.suppress`, repaired two comment lines, and re-ran clippy, docs and
//! test in full.
//!
//! The finer fact is per gate. A gate reads some of the tree; a delta touches
//! some of the tree; where the two do not meet, the gate's verdict over the new
//! tree is the verdict it already gave over the old one, and the earlier
//! receipt carries forward. [`DeltaClass`] names what a changed line *is* and
//! [`DeltaClass::invalidates`] is the single table saying which gates can see
//! it. Both sides of the ledger read this one table: the lane selects the gates
//! it runs from it, and the coordinator's receipt admission judges a carried
//! coverage claim against it. Two tables would be two answers to the same
//! question, and the looser one would decide.
//!
//! The classifier that produces these classes is the lane's — it needs a git
//! diff, which this crate cannot take — and it is deliberately conservative:
//! a hunk it cannot classify is [`DeltaClass::Code`], which invalidates
//! everything and reduces to the behaviour that existed before this amendment.

use alloc::string::String;
use alloc::vec::Vec;

use serde::{Deserialize, Serialize};

use crate::digest::Digest;
use crate::values::{VerifyFailure, VerifyFailureSet};

/// How the host states a delta-confirm's carry to the verify lane:
/// `<proved tree hex>:<receipt digest hex>:<gate,gate,…>`, where the gate list
/// is the identities that *passed* in that receipt.
///
/// The environment rather than an argv flag, for the reason
/// [`EXECUTION_DEADLINE_ENV`](crate::EXECUTION_DEADLINE_ENV) states: the lane's
/// own `xtask` is compiled from the sealed subject tree, so a new flag is one
/// an already-sealed dispatch cannot parse, while a key an older lane does not
/// read costs nothing — it runs the whole umbrella, which is the behaviour this
/// carry is an optimization over.
///
/// The passed-gate list is the host's to state because only the ledger knows
/// it. The lane knows what changed; the receipt knows what was green; a carry
/// needs both, and neither side can invent the other's half.
pub const VERIFY_PROVED_ENV: &str = "AETHER_BLOOMERY_VERIFY_PROVED";

/// A per-invocation request to skip gates, as a comma-separated identity list
/// of the gates to *run*.
///
/// Honoured only where [`VERIFY_PROVED_ENV`] backs the omission with a green
/// receipt over a tree the delta cannot have moved for that gate: a selection
/// that names fewer gates than the receipt backs still runs the rest. One rule
/// for every caller that wants a narrower umbrella — the delta-confirm carry
/// and the probe selection alike — so no selection input can drop a gate that
/// nothing has judged.
pub const VERIFY_GATES_ENV: &str = "AETHER_BLOOMERY_VERIFY_GATES";

/// The eight umbrella gates a delta class is answerable for.
///
/// [`VerifyFailure::Preflight`] and [`VerifyFailure::Containment`] are absent
/// and must stay absent: neither is a gate the fan-out runs over the tree.
/// Preflight is the umbrella's own refusal to start, and containment is a
/// chassis-side overlay over the declared surface — it reads *which* paths a
/// delta touched rather than what changed inside them, so no class of line edit
/// can excuse it. Carrying either forward would be carrying a verdict nothing
/// ever gave.
pub const DELTA_GATES: [VerifyFailure; 8] = [
    VerifyFailure::Fmt,
    VerifyFailure::Clippy,
    VerifyFailure::Docs,
    VerifyFailure::Test,
    VerifyFailure::Dup,
    VerifyFailure::Deps,
    VerifyFailure::Suppress,
    VerifyFailure::Lock,
];

/// What one changed line — or one changed file — of a delta is.
///
/// A delta is classified per changed line for a Rust file and per path for
/// everything else, and the classes it produced are unioned: a delta that
/// touched a doc comment and a `let` is [`Self::Code`] and
/// [`Self::DocComment`] together, which invalidates the union of both rows.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeltaClass {
    /// Rust tokens changed — or the line could not be classified at all.
    ///
    /// Every gate, because a changed expression reaches every one of them: it
    /// reformats, it lints, it documents, it runs, it duplicates, it uses a
    /// dependency, it can carry a suppression, and it can move the resolved
    /// manifest graph through a `#[path]` or a feature-gated module.
    Code,
    /// Only `///` or `//!` lines changed.
    ///
    /// Rustdoc reads them, rustfmt reformats them, jscpd tokenizes them, and
    /// the suppression scanner reads the request marker off any comment
    /// (`scripts/check-suppressions.py`'s `REQUEST_RE` matches `//`, which
    /// `///` opens with). Clippy and the test suite cannot see a doc line:
    /// rustc discards it before the lint pass and no test observes it.
    DocComment,
    /// Only ordinary `//` lines, blank lines, or suppression attributes
    /// (`#[allow(…)]`, `#[expect(…)]`, `#[ignore]`) changed.
    ///
    /// Clippy is in this row precisely because an `#[allow]` is clippy's
    /// business — the attribute is the thing that silences the lint. Rustdoc
    /// cannot see an ordinary comment and the suite cannot observe one, so
    /// docs and test stay out.
    Comment,
    /// A `Cargo.toml` changed.
    ///
    /// The three compiling gates re-resolve their graph from it, `verify.deps`
    /// scans it for unused dependencies, and `verify.lock` is the gate that
    /// exists for a manifest edit landing without its lock regeneration.
    /// `verify.suppress` scans `Cargo.toml` too (the scanner dispatches on the
    /// file's name, `scripts/check-suppressions.py`), and `cargo fmt --all`
    /// resolves workspace membership out of the manifests — neither was in the
    /// first statement of this table and both are what the scanners actually
    /// read.
    Manifest,
    /// `Cargo.lock` changed.
    ///
    /// `verify.lock` reads it under `--locked` and `verify.deps` resolves
    /// against it. Nothing else opens it: a lock edit alone changes no source
    /// the compiling gates or the scanners read.
    Lockfile,
    /// A path no gate in the umbrella opens: `docs/**` and the top-level
    /// `*.md`.
    ///
    /// Stated as a narrow allowlist rather than as "non-Rust", because the
    /// non-Rust files that *are* read are the ones a repair lap is most likely
    /// to reach for: `rustfmt.toml` decides every `verify.fmt` verdict,
    /// `clippy.toml` every `verify.clippy` verdict, `rust-toolchain.toml` all
    /// of them, `scripts/check-suppressions.py` is the suppression gate
    /// itself, and `xtask/src/transform/*.md` is `include_str!`d into a crate
    /// the suite compiles. Every one of those falls to [`Self::Code`] instead.
    Inert,
}

impl DeltaClass {
    /// Every class, in declaration order.
    pub const ALL: [Self; 6] =
        [Self::Code, Self::DocComment, Self::Comment, Self::Manifest, Self::Lockfile, Self::Inert];

    /// The gates a delta of this class can affect — the table.
    ///
    /// This is the single source of truth the amendment records. A gate whose
    /// scanner reads something a row does not name is a wrong row, never a
    /// reason for that gate to opt out of the selection.
    #[must_use]
    pub fn invalidates(self) -> VerifyFailureSet {
        let gates: &[VerifyFailure] = match self {
            Self::Code => &DELTA_GATES,
            Self::DocComment => &[VerifyFailure::Docs, VerifyFailure::Fmt, VerifyFailure::Dup, VerifyFailure::Suppress],
            Self::Comment => &[VerifyFailure::Fmt, VerifyFailure::Clippy, VerifyFailure::Dup, VerifyFailure::Suppress],
            Self::Manifest => &[
                VerifyFailure::Deps,
                VerifyFailure::Lock,
                VerifyFailure::Clippy,
                VerifyFailure::Docs,
                VerifyFailure::Test,
                VerifyFailure::Suppress,
                VerifyFailure::Fmt,
            ],
            Self::Lockfile => &[VerifyFailure::Lock, VerifyFailure::Deps],
            Self::Inert => &[],
        };
        gates.iter().copied().collect()
    }

    /// The canonical name this class records itself under.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Code => "code",
            Self::DocComment => "doc_comment",
            Self::Comment => "comment",
            Self::Manifest => "manifest",
            Self::Lockfile => "lockfile",
            Self::Inert => "inert",
        }
    }

    /// The class `name` spells, for a reader decoding a recorded coverage
    /// claim.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|class| class.as_str() == name)
    }
}

/// The gates a delta made of `classes` can affect.
///
/// An empty `classes` is the empty set — a delta that changed nothing
/// invalidates nothing — and is reachable: a repair lap that only amended a
/// commit message leaves the tree it started from.
pub fn invalidated_by(classes: impl IntoIterator<Item = DeltaClass>) -> VerifyFailureSet {
    classes.into_iter().fold(VerifyFailureSet::EMPTY, |set, class| set.union(class.invalidates()))
}

/// The gates a delta made of `classes` cannot affect, and so may be carried
/// from an earlier receipt.
#[must_use]
pub fn carryable(classes: &[DeltaClass]) -> VerifyFailureSet {
    DELTA_GATES.into_iter().collect::<VerifyFailureSet>().difference(invalidated_by(classes.iter().copied()))
}

/// One gate a run did not execute, and the receipt whose verdict stands in its
/// place (ADR-0200 amendment).
///
/// Named rather than implied by absence: `failed_verifiers` says which gates
/// were red and `gates` says which ones ran, and a gate missing from both would
/// read as a silent pass. A carried gate is neither run nor passed here — it is
/// a pointer at the run that did judge it.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CarriedGate {
    /// The gate identity, as `VerifyFailure::as_str` spells it.
    pub gate: String,
    /// The evidence digest of the receipt this verdict comes from.
    pub receipt: Digest,
    /// The tree that receipt proved.
    pub tree: Digest,
    /// Whether that receipt's verdict for this gate was a pass.
    pub passed: bool,
}

/// A run's claim about what it did not execute: "ran the rest, carried these
/// from a receipt over `proved`, because the delta from `proved` was made of
/// `classes`".
///
/// The whole of what the coordinator has to judge, held together so a claim
/// cannot arrive with the carried gates of one delta and the classes of
/// another.
#[derive(Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct CarriedCoverage {
    /// The tree the carried receipt proved.
    pub proved: Digest,
    /// The evidence digest of that receipt.
    pub receipt: Digest,
    /// What the lane classified the `proved`-to-judged delta as.
    pub classes: Vec<DeltaClass>,
    /// The gates it carried rather than ran.
    pub carried: Vec<CarriedGate>,
}

/// Why a carried-coverage claim is not admissible.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CarryRefusal {
    /// The claim names a receipt the ledger does not hold for exactly the tree
    /// it names.
    UnknownReceipt,
    /// A carried gate is one the stated delta classes *can* affect, so the
    /// earlier verdict says nothing about the new tree.
    InvalidatedGate(VerifyFailure),
    /// A carried gate is not one of [`DELTA_GATES`] — preflight or containment,
    /// which no delta class can excuse.
    NotAGate,
    /// A carried gate's earlier verdict was not a pass, so carrying it forward
    /// would launder a red gate into an unexamined one.
    CarriedFailure(VerifyFailure),
}

impl CarriedCoverage {
    /// Judge this claim against the table, given whether the ledger holds the
    /// named receipt for exactly the tree the claim names.
    ///
    /// The ledger lookup is the caller's because this crate stores nothing; the
    /// *rule* is here, so the admission door and the lane cannot reach two
    /// answers about the same claim.
    ///
    /// # Errors
    ///
    /// [`CarryRefusal`] naming the first reason the claim does not stand. A
    /// refused claim is an incomplete receipt: the full umbrella is dispatched
    /// rather than the coverage being patched up.
    pub fn admissible(&self, receipt_on_record: bool) -> Result<(), CarryRefusal> {
        if !receipt_on_record {
            return Err(CarryRefusal::UnknownReceipt);
        }
        let invalidated = invalidated_by(self.classes.iter().copied());
        for entry in &self.carried {
            let gate = VerifyFailure::from_name(&entry.gate).ok_or(CarryRefusal::NotAGate)?;
            if !DELTA_GATES.contains(&gate) {
                return Err(CarryRefusal::NotAGate);
            }
            // As a pair, so an entry pointing at a different receipt *or* a
            // different tree is one refusal rather than two conditions a reader
            // has to hold apart.
            if (entry.receipt, entry.tree) != (self.receipt, self.proved) {
                return Err(CarryRefusal::UnknownReceipt);
            }
            if invalidated.contains(gate) {
                return Err(CarryRefusal::InvalidatedGate(gate));
            }
            if !entry.passed {
                return Err(CarryRefusal::CarriedFailure(gate));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use alloc::vec;

    use super::{CarriedCoverage, CarriedGate, CarryRefusal, DELTA_GATES, DeltaClass, carryable, invalidated_by};
    use crate::digest::Digest;
    use crate::values::{VerifyFailure, VerifyFailureSet};

    fn digest(byte: u8) -> Digest {
        Digest::from_bytes([byte; 32])
    }

    /// Tripwire: the table itself. Each row is the answer to "what can a gate
    /// of this shape see", derived by reading every scanner's argv and source
    /// in `xtask/src/transform/verify/mod.rs` and
    /// `scripts/check-suppressions.py`. A row that quietly loses a gate is a
    /// gate that stops running on a delta that changes its verdict, which is
    /// the false-green direction and invisible in any other test.
    #[test]
    fn the_table_states_what_each_class_reaches() {
        assert_eq!(DeltaClass::Code.invalidates(), DELTA_GATES.into_iter().collect::<VerifyFailureSet>());
        assert_eq!(
            DeltaClass::DocComment.invalidates(),
            [VerifyFailure::Fmt, VerifyFailure::Docs, VerifyFailure::Dup, VerifyFailure::Suppress]
                .into_iter()
                .collect::<VerifyFailureSet>(),
        );
        assert_eq!(
            DeltaClass::Comment.invalidates(),
            [VerifyFailure::Fmt, VerifyFailure::Clippy, VerifyFailure::Dup, VerifyFailure::Suppress]
                .into_iter()
                .collect::<VerifyFailureSet>(),
        );
        assert_eq!(
            DeltaClass::Manifest.invalidates(),
            [
                VerifyFailure::Fmt,
                VerifyFailure::Clippy,
                VerifyFailure::Docs,
                VerifyFailure::Test,
                VerifyFailure::Deps,
                VerifyFailure::Suppress,
                VerifyFailure::Lock,
            ]
            .into_iter()
            .collect::<VerifyFailureSet>(),
        );
        assert_eq!(
            DeltaClass::Lockfile.invalidates(),
            [VerifyFailure::Deps, VerifyFailure::Lock].into_iter().collect::<VerifyFailureSet>(),
        );
        assert!(DeltaClass::Inert.invalidates().is_empty());
    }

    /// Tripwire: preflight and containment are not gates a delta can excuse.
    /// A future identity appended to `VerifyFailure::ALL` that silently joined
    /// `DELTA_GATES` would become carryable on an inert delta without anyone
    /// deciding it should be.
    #[test]
    fn the_umbrella_only_carries_gates_that_read_the_tree() {
        assert!(!DELTA_GATES.contains(&VerifyFailure::Preflight));
        assert!(!DELTA_GATES.contains(&VerifyFailure::Containment));
        assert_eq!(DELTA_GATES.len(), 8);
    }

    #[test]
    fn a_comment_delta_carries_the_gates_it_cannot_reach() {
        let carried = carryable(&[DeltaClass::Comment]);

        assert!(carried.contains(VerifyFailure::Test), "no test observes a comment line");
        assert!(carried.contains(VerifyFailure::Docs), "rustdoc does not read an ordinary comment");
        assert!(carried.contains(VerifyFailure::Deps));
        assert!(carried.contains(VerifyFailure::Lock));
        assert!(!carried.contains(VerifyFailure::Clippy), "an allow attribute is clippy's own business");
        assert!(!carried.contains(VerifyFailure::Suppress));
    }

    #[test]
    fn classes_union_rather_than_pick_one() {
        let mixed = invalidated_by([DeltaClass::DocComment, DeltaClass::Lockfile]);

        assert!(mixed.contains(VerifyFailure::Docs));
        assert!(mixed.contains(VerifyFailure::Lock));
        assert!(!mixed.contains(VerifyFailure::Test), "neither row reaches the suite");
    }

    fn coverage(classes: &[DeltaClass], carried: &[VerifyFailure]) -> CarriedCoverage {
        CarriedCoverage {
            proved: digest(0xA0),
            receipt: digest(0xB0),
            classes: classes.to_vec(),
            carried: carried
                .iter()
                .map(|gate| CarriedGate {
                    gate: gate.as_str().into(),
                    receipt: digest(0xB0),
                    tree: digest(0xA0),
                    passed: true,
                })
                .collect(),
        }
    }

    #[test]
    fn a_claim_stands_only_over_a_receipt_the_ledger_holds() {
        let claim = coverage(&[DeltaClass::Comment], &[VerifyFailure::Test]);

        assert_eq!(claim.admissible(false), Err(CarryRefusal::UnknownReceipt));
        assert_eq!(claim.admissible(true), Ok(()));
    }

    #[test]
    fn a_claim_carrying_a_gate_its_own_delta_invalidates_is_refused() {
        let claim = coverage(&[DeltaClass::Code], &[VerifyFailure::Test]);

        assert_eq!(claim.admissible(true), Err(CarryRefusal::InvalidatedGate(VerifyFailure::Test)));
    }

    #[test]
    fn a_red_earlier_verdict_is_never_carried() {
        let mut claim = coverage(&[DeltaClass::Comment], &[VerifyFailure::Test]);
        claim.carried[0].passed = false;

        assert_eq!(claim.admissible(true), Err(CarryRefusal::CarriedFailure(VerifyFailure::Test)));
    }

    #[test]
    fn containment_and_preflight_are_not_carryable() {
        let claim = coverage(&[DeltaClass::Inert], &[VerifyFailure::Containment]);

        assert_eq!(claim.admissible(true), Err(CarryRefusal::NotAGate));
    }

    #[test]
    fn a_carried_entry_must_name_the_claim_s_own_receipt_and_tree() {
        let mut claim = coverage(&[DeltaClass::Comment], &[VerifyFailure::Test]);
        claim.carried[0].tree = digest(0x99);

        assert_eq!(claim.admissible(true), Err(CarryRefusal::UnknownReceipt));
    }

    #[test]
    fn every_class_round_trips_its_name() {
        for class in DeltaClass::ALL {
            assert_eq!(DeltaClass::from_name(class.as_str()), Some(class));
        }
        assert_eq!(DeltaClass::from_name("tokens"), None);
    }

    #[test]
    fn an_empty_delta_invalidates_nothing() {
        assert!(invalidated_by(vec![]).is_empty());
    }
}
