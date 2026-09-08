//! What the bloom-level reader files, and the derivation it files it under
//! (ADR-0216 §3).
//!
//! The reader gets exactly one write. For each finding it will not fix, the
//! coordinator files an open commission whose intent is a [`Statement`] with
//! [`Provenance::StageReceipt`] — the run that produced it, by profile digest,
//! over exact inputs, producing exact outputs — and whose derivation parent is
//! that receipt statement's own address.
//!
//! What the provenance buys is the record and nothing else, which is the point.
//! [`Statement::verify_authority`] is `false` for a stage receipt at every door,
//! [`Statement::is_instruction_capable`] is `false`, the commission store's
//! approval classifier refuses it, and a filing carries no scope revision for a
//! seal to name. A filed finding becomes work only when a person signs an
//! Approve statement over a frozen revision, exactly as a hand-filed commission
//! does.
//!
//! [`RetrospectFinding::normalize`] is the trust boundary. The reader is an
//! untrusted model lane whose whole input is a landed diff, and this is where
//! its claim stops being a claim: the emission is refused **whole** when any
//! entry is malformed, so a garbled read files nothing rather than filing
//! whatever survived a per-entry filter. The counts are capped, and the surplus
//! past the cap is dropped rather than taking the honest findings down with it.

use alloc::borrow::ToOwned;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write as _;
use core::str::from_utf8;

use serde::{Deserialize, Serialize};

use super::approval::SurfacePattern;
use super::statement::{Provenance, StageReceipt, Statement};
use super::surface::truncated;
use crate::digest::{ContentAddressed, Digest, digest_of};
use crate::ids::{StageId, WorkpieceId};
use crate::port::intent_title;

/// One finding as the reader emitted it — the wire shape
/// `retrospect_finding_contract` describes, before anything has judged it.
///
/// Raw and untrusted, like the claim
/// [`SurfaceRequest::normalize`](super::SurfaceRequest::normalize) takes: the
/// host reads it off the lane's evidence and hands it here, and every field is
/// whatever the model wrote. The receipt the finding derives from is
/// deliberately *not* on this shape — it is the host's, read off the order that
/// dispatched the read, because a lane that could name its own receipt could
/// file a finding derived from a bloom it never opened.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RetrospectClaim {
    /// The work order's title — the heading a reader of the filed commission
    /// sees first.
    pub title: String,
    /// The work order's body: what was seen, where, and what a future bloom
    /// would do about it.
    pub body: String,
    /// The crate globs the work would touch, in the declared-surface grammar
    /// ([`SurfacePattern`]).
    pub surface: Vec<String>,
}

/// One finding the reader will not fix, bound to the receipt it derives from.
///
/// The normalized half of [`RetrospectClaim`]: trimmed, capped, and carrying
/// the landing receipt the order pinned. Content-addressed, because the address
/// is two things at once — the [`StageReceipt::outputs`] entry that records the
/// read produced it, and the stable half of the workpiece id the filing takes.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub struct RetrospectFinding {
    /// The landing receipt this finding derives from — `bloom.receipt`, the
    /// artifact the `Study` binding consumes and the digest the read's evidence
    /// binds to.
    pub receipt: Digest,
    /// The work order's title.
    pub title: String,
    /// The work order's body.
    pub body: String,
    /// The crate globs the work would touch, deduplicated and sorted.
    pub surface: Vec<String>,
}

impl ContentAddressed for RetrospectFinding {
    const DOMAIN: &'static str = "aether.bloomery.retrospect_finding";
}

/// Why a reader's emission was refused whole (ADR-0216 §3).
///
/// Every arm names one entry, because refusing the whole set over one bad entry
/// is the decision: a reader that emitted a finding with no title emitted a
/// document its contract forbids, and the honest reading of a document that
/// does not follow its contract is that none of it can be trusted to. Filing
/// the survivors would put machine-authored work orders in the estate on the
/// strength of a payload the estate already knows is wrong.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum RetrospectRefusal {
    /// The entry at this position carries no title, so the filed commission
    /// would have no heading and its replica no name.
    EmptyTitle {
        /// Position in the emitted order.
        index: usize,
    },
    /// The entry at this position carries no body, so the filing would state a
    /// title and no work.
    EmptyBody {
        /// Position in the emitted order.
        index: usize,
    },
    /// The entry at this position declares no surface, so nothing says where
    /// the work would happen.
    EmptySurface {
        /// Position in the emitted order.
        index: usize,
    },
    /// The entry at this position declares a surface glob outside the
    /// declared-surface grammar [`SurfacePattern`] parses.
    UnparsableSurface {
        /// Position in the emitted order.
        index: usize,
        /// The glob as the reader wrote it.
        glob: String,
    },
}

/// A normalized emission: the findings to file, and how many the ceiling
/// dropped.
///
/// The two travel together because the caller owes an action on each — file the
/// first, say the second out loud. A dropped count is not a refusal: the
/// findings that fit are as good as they were, and losing them because the
/// reader was verbose would be the ceiling punishing the wrong thing.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct RetrospectEmission {
    /// The findings to file, in emitted order, capped at
    /// [`RetrospectFinding::MAX_FINDINGS`].
    pub findings: Vec<RetrospectFinding>,
    /// How many findings past the ceiling were dropped.
    pub dropped: usize,
}

impl RetrospectFinding {
    /// How many findings one read may file.
    ///
    /// The hand pass this arc replaces produced about fifty issues from a whole
    /// day's estate (ADR-0216 §Context). A single bloom is a fraction of that,
    /// so a read emitting more than a dozen work orders is not being thorough —
    /// it is filing noise, and the pile nobody reads is the failure mode the
    /// ADR names by name. The surplus is dropped loudly rather than refused
    /// whole, so a verbose read still files its first twelve.
    pub const MAX_FINDINGS: usize = 12;
    /// The title's byte ceiling, so a filed commission's replica title is a
    /// name rather than a paragraph.
    pub const MAX_TITLE_BYTES: usize = 180;
    /// The body's byte ceiling. Generous — the body is the work order — but
    /// bounded, because it is model text that ends up in a durable row.
    pub const MAX_BODY_BYTES: usize = 8_192;
    /// How many surface globs one finding may declare.
    pub const MAX_SURFACE_GLOBS: usize = 16;

    /// Judge a reader's emission against `receipt`, or refuse it whole.
    ///
    /// Trims and caps every field, deduplicates and sorts each finding's
    /// surface, and drops the surplus past [`Self::MAX_FINDINGS`]. Refuses on
    /// the first entry that carries no title, no body, no surface, or a glob
    /// outside the declared-surface grammar.
    ///
    /// # Errors
    /// [`RetrospectRefusal`] naming the first malformed entry and why.
    pub fn normalize(
        receipt: Digest,
        claimed: impl IntoIterator<Item = RetrospectClaim>,
    ) -> Result<RetrospectEmission, RetrospectRefusal> {
        let mut findings = Vec::new();
        let mut dropped = 0;
        for (index, claim) in claimed.into_iter().enumerate() {
            let title = claim.title.trim();
            if title.is_empty() {
                return Err(RetrospectRefusal::EmptyTitle { index });
            }
            let body = claim.body.trim();
            if body.is_empty() {
                return Err(RetrospectRefusal::EmptyBody { index });
            }
            let mut surface = Vec::new();
            for glob in &claim.surface {
                let glob = glob.trim();
                if SurfacePattern::parse(glob).is_none() {
                    return Err(RetrospectRefusal::UnparsableSurface { index, glob: glob.to_owned() });
                }
                if !surface.iter().any(|kept: &String| kept.as_str() == glob) {
                    surface.push(glob.to_owned());
                }
            }
            if surface.is_empty() {
                return Err(RetrospectRefusal::EmptySurface { index });
            }
            surface.sort();
            surface.truncate(Self::MAX_SURFACE_GLOBS);

            // The whole emission is judged before the ceiling is applied, so a
            // malformed entry past the cap still refuses. A read that emitted
            // garbage in its thirteenth finding emitted garbage.
            if findings.len() == Self::MAX_FINDINGS {
                dropped += 1;
                continue;
            }
            findings.push(Self {
                receipt,
                title: truncated(title, Self::MAX_TITLE_BYTES),
                body: truncated(body, Self::MAX_BODY_BYTES),
                surface,
            });
        }

        Ok(RetrospectEmission { findings, dropped })
    }

    /// The workpiece id this finding files under.
    ///
    /// A function of the finding's own content address, so the id is
    /// deterministic: a replayed admission re-derives the same id and the
    /// store's duplicate check makes the re-file a no-op rather than a second
    /// commission wearing the same words. Namespaced by prefix so a filing is
    /// greppable and cannot be mistaken for a hand-authored `issue-NNNN`.
    #[must_use]
    pub fn workpiece(&self) -> WorkpieceId {
        WorkpieceId(format!("{}{}", Self::ID_PREFIX, &digest_of(self).to_hex()[..12]))
    }

    /// The prefix every filed finding's workpiece id carries.
    pub const ID_PREFIX: &'static str = "retrospect-";

    /// The intent text the filed commission carries — a markdown work order.
    ///
    /// The heading is what the replica projection reads back as the title
    /// ([`intent_title`]), so a filing is a distinguishable
    /// row in an issue list rather than one of a dozen copies of a constant.
    /// The surface section states the crate globs the work would touch, in the
    /// same grammar a scope revision declares, so whoever scopes this filing
    /// starts from what the reader named rather than from nothing.
    #[must_use]
    pub fn intent_words(&self) -> String {
        let mut words = format!("# {}\n\n{}\n\n## Surface\n\n", self.title, self.body);
        for glob in &self.surface {
            let _ = writeln!(words, "- `{glob}`");
        }
        let _ = writeln!(words, "\nDerived from bloom receipt `{}` by the ADR-0216 reader.", self.receipt.to_hex());
        words
    }
}

/// What a filed commission says about the read that filed it — the read side of
/// [`RetrospectFinding::intent_words`].
///
/// A reader of the estate holds a commission, not a [`RetrospectFinding`]: the
/// finding itself is never persisted, only the intent statement minted from it.
/// This projects that statement back into the three things a list row needs to
/// say — which landing receipt the read consumed, what the work order is called,
/// and where it would happen — so the pile is readable beside the bloom it came
/// out of rather than only as a wall of opaque `retrospect-` ids.
///
/// Absent for every hand-filed commission ([`Self::of_intent`] answers `None`),
/// so an ordinary list row is exactly what it was.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct FiledFinding {
    /// The landing receipt the read consumed — the stage receipt's first input,
    /// which is the digest a bloom's own `Study` evidence binds to.
    pub receipt: Digest,
    /// The work order's heading, or empty when the intent carries none.
    pub title: String,
    /// The crate globs the intent's `Surface` section names.
    pub surface: Vec<String>,
}

impl FiledFinding {
    /// Project `intent`, or `None` when no reader filed it.
    ///
    /// The provenance is the whole test: only a [`Provenance::StageReceipt`]
    /// naming [`StageId::Study`] is a reader's filing, and a stage receipt over
    /// no inputs named no receipt to derive from. Nothing here is a check on
    /// authority — a stage receipt authorizes nothing at any door (ADR-0216 §3),
    /// and this is a projection for a reader's eyes.
    #[must_use]
    pub fn of_intent(intent: &Statement) -> Option<Self> {
        let Provenance::StageReceipt(receipt) = &intent.provenance else {
            return None;
        };
        if receipt.stage != StageId::Study {
            return None;
        }
        let words = from_utf8(&intent.words).ok()?;

        Some(Self {
            receipt: *receipt.inputs.first()?,
            title: intent_title(&intent.words).unwrap_or_default(),
            surface: surface_globs(words),
        })
    }
}

/// The globs of a work order's `## Surface` section, in written order.
///
/// The reading half of [`RetrospectFinding::intent_words`], and it lives beside
/// the writer so the two move together: a heading or bullet spelled differently
/// there is a section nothing here finds, and the round-trip test below is what
/// says so out loud. Bounded by the same ceiling the writer is bounded by, so a
/// hand-written intent that happens to carry the section cannot make a list row
/// unbounded.
fn surface_globs(words: &str) -> Vec<String> {
    let mut globs = Vec::new();
    let mut inside = false;
    for line in words.lines() {
        let line = line.trim();
        if let Some(heading) = line.strip_prefix("##") {
            inside = heading.trim().eq_ignore_ascii_case("surface");
            continue;
        }
        if !inside || globs.len() == RetrospectFinding::MAX_SURFACE_GLOBS {
            continue;
        }
        if let Some(glob) = line
            .strip_prefix("- ")
            .and_then(|item| item.trim().strip_prefix('`'))
            .and_then(|item| item.strip_suffix('`'))
            .filter(|glob| !glob.is_empty())
        {
            globs.push(glob.to_owned());
        }
    }

    globs
}

/// The reader's derivation record: one statement asserting the receipt it read,
/// grounded in the run that read it (ADR-0216 §3).
///
/// Its own statement rather than a field on each filing, because the run is one
/// thing and the filings are several: the receipt names the profile that ran,
/// the inputs it consumed (the bloom receipt, then the landed range as
/// `base..head`), and the outputs it produced — the address of every finding it
/// filed. Each filing then names this statement's address as its derivation
/// parent, so the whole read is recoverable from any one of its filings.
///
/// `words` is the landing receipt's own digest bytes, the way
/// [`signed_approval`](super::signed_approval) and every other door shape makes
/// its words the digest it is about. That keeps the statement recomputable: a
/// reader holding one filing has the provenance and the inputs, and re-mints
/// byte-identical bytes.
///
/// Deterministic, so re-deriving it during a replayed admission produces the
/// same parent address rather than a second lineage over the same read.
#[must_use]
pub fn reader_derivation(
    profile: Digest,
    receipt: Digest,
    base: Option<Digest>,
    head: Digest,
    findings: &[RetrospectFinding],
) -> Statement {
    let mut inputs = alloc::vec![receipt];
    inputs.extend(base);
    inputs.push(head);

    Statement {
        words: receipt.as_bytes().to_vec(),
        provenance: Provenance::StageReceipt(StageReceipt {
            stage: StageId::Study,
            profile,
            inputs,
            outputs: findings.iter().map(digest_of).collect(),
        }),
        parents: Vec::new(),
    }
}

/// One filing's intent statement: the work order's words, the read's provenance,
/// and the read's own statement as its derivation parent.
///
/// The provenance is the identical [`StageReceipt`] `derivation` carries, so a
/// filing states what produced it without a lookup, and the parent edge states
/// *which* read — the two answer different questions and both are cheap.
///
/// Nothing here is authorization and nothing here can become one:
/// [`Statement::verify_authority`] answers `false` for this provenance at every
/// door, and a commission store refuses it as an approval. ADR-0214's closing
/// rule holds verbatim — task text does not acquire authority by being emitted
/// from an authorized process.
#[must_use]
pub fn filed_intent(finding: &RetrospectFinding, derivation: &Statement) -> Statement {
    Statement {
        words: finding.intent_words().into_bytes(),
        provenance: derivation.provenance.clone(),
        parents: alloc::vec![digest_of(derivation)],
    }
}

#[cfg(test)]
mod tests {
    use alloc::string::{String, ToString};
    use alloc::vec::Vec;

    use super::{
        FiledFinding, RetrospectClaim, RetrospectFinding, RetrospectRefusal, Statement, filed_intent, reader_derivation,
    };
    use crate::digest::{Digest, digest_of};
    use crate::port::intent_title;
    use crate::sign::{AuthorityDoor, FakeKeyProvider};
    use crate::values::{Observation, Provenance};

    fn claim(title: &str, body: &str, surface: &[&str]) -> RetrospectClaim {
        RetrospectClaim {
            title: title.to_string(),
            body: body.to_string(),
            surface: surface.iter().map(|glob| (*glob).to_string()).collect(),
        }
    }

    fn good(title: &str) -> RetrospectClaim {
        claim(title, "the reader saw this and will not fix it", &["crates/aether-bloomery/**"])
    }

    #[test]
    fn one_malformed_entry_refuses_the_whole_emission() {
        // Tripwire: the alternative — filter the bad entry and file the rest —
        // is what every other lane channel here does (`SurfaceRequest` drops a
        // covered path, `SuppressionRequest` drops an incomplete one), and it
        // is wrong for this one. Those channels annotate a member that already
        // exists; this one *creates work orders in the estate*, so a payload
        // the contract already refuses must file nothing at all.
        let receipt = Digest::from_bytes([7; 32]);
        let emitted = vec![good("first"), claim("", "no title", &["crates/aether-bloomery/**"]), good("third")];

        assert_eq!(
            RetrospectFinding::normalize(receipt, emitted),
            Err(RetrospectRefusal::EmptyTitle { index: 1 }),
            "a malformed entry takes its honest siblings down with it, by design",
        );
        assert_eq!(
            RetrospectFinding::normalize(receipt, vec![claim("t", "b", &["crates/**/src/*.rs"])]),
            Err(RetrospectRefusal::UnparsableSurface { index: 0, glob: String::from("crates/**/src/*.rs") }),
            "a surface outside the declared-surface grammar is malformed, not merely unusual",
        );
    }

    #[test]
    fn the_surplus_past_the_ceiling_is_dropped_rather_than_refused() {
        // Tripwire: the ceiling and the refusal are opposite responses to
        // opposite problems. Refusing a verbose read would let one extra
        // finding destroy twelve good ones; capping a malformed read would file
        // work orders out of a payload the contract already rejected.
        let receipt = Digest::from_bytes([7; 32]);
        let emitted: Vec<_> = (0..RetrospectFinding::MAX_FINDINGS + 3).map(|n| good(&n.to_string())).collect();

        let emission = RetrospectFinding::normalize(receipt, emitted).expect("well-formed entries normalize");
        assert_eq!(emission.findings.len(), RetrospectFinding::MAX_FINDINGS);
        assert_eq!(emission.dropped, 3, "the caller has a number to say out loud");
    }

    #[test]
    fn a_filing_carries_its_read_and_authorizes_nothing() {
        // Tripwire: this is the whole ADR-0216 §3 claim in one place. The
        // derivation has to name the run and the landed range, the filing has
        // to point back at it, and neither may verify as authority at any door
        // — a `Provenance` arm swapped for an author signature here would mint
        // a lane its own approval.
        let receipt = Digest::from_bytes([7; 32]);
        let profile = Digest::from_bytes([8; 32]);
        let (base, head) = (Digest::from_bytes([1; 32]), Digest::from_bytes([2; 32]));
        let emission = RetrospectFinding::normalize(receipt, vec![good("a leak"), good("a gap")])
            .expect("well-formed entries normalize");

        let derivation = reader_derivation(profile, receipt, Some(base), head, &emission.findings);
        let Provenance::StageReceipt(stage_receipt) = &derivation.provenance else {
            panic!("the reader's derivation is a stage receipt: {derivation:?}");
        };
        assert_eq!(stage_receipt.inputs, [receipt, base, head], "the receipt it read, then the range it read");
        assert_eq!(
            stage_receipt.outputs,
            emission.findings.iter().map(digest_of).collect::<Vec<_>>(),
            "and the filings it produced",
        );

        let filing = filed_intent(&emission.findings[0], &derivation);
        assert_eq!(filing.parents, [digest_of(&derivation)], "a filing names the read it came out of");
        assert!(!filing.is_instruction_capable());
        assert!(
            !filing.verify_authority(&FakeKeyProvider, AuthorityDoor::Approve, digest_of(&filing)),
            "a provider that accepts every message it is given still must not authorize this",
        );
        assert_eq!(
            intent_title(&filing.words).as_deref(),
            Some("a leak"),
            "the intent's heading is what the replica projection titles the filing by",
        );
    }

    #[test]
    fn a_filed_intent_projects_back_to_the_words_it_was_written_from() {
        // Tripwire: `intent_words` writes the work order and `FiledFinding`
        // reads it back, and the two are hand-written halves of one format.
        // Respell the heading, the bullet, or the section name on either side
        // and a console listing this bloom's pile loses the surface (or the
        // whole row) while every type still compiles.
        let receipt = Digest::from_bytes([7; 32]);
        let emission = RetrospectFinding::normalize(
            receipt,
            vec![claim("a leak", "the reader saw it", &["crates/aether-bloomery/**", "crates/aether-codec/**"])],
        )
        .expect("well-formed entries normalize");
        let finding = &emission.findings[0];

        let projected = FiledFinding::of_intent(&filed_intent(
            finding,
            &reader_derivation(Digest::from_bytes([8; 32]), receipt, None, Digest::from_bytes([2; 32]), &[]),
        ))
        .expect("a reader's filing projects");

        assert_eq!(projected.receipt, receipt, "the row names the landing receipt the read consumed");
        assert_eq!(projected.title, finding.title);
        assert_eq!(projected.surface, finding.surface, "the Surface section reads back as the globs it stated");
    }

    #[test]
    fn a_hand_filed_commission_is_not_a_reader_filing() {
        // Tripwire: the projection keys on provenance alone. Loosen it to "has
        // words shaped like a work order" and every hand-authored commission
        // starts rendering as a machine proposal on some bloom's receipt.
        let intent = Statement {
            words: b"# ship the store\n\n## Surface\n\n- `crates/aether-bloomery/**`\n".to_vec(),
            provenance: Provenance::ObservationAttestation(Observation { source: "an operator".to_string() }),
            parents: Vec::new(),
        };

        assert_eq!(FiledFinding::of_intent(&intent), None);
    }
}
