//! Freezing the revision a passing pre-bloom scoping run produced (ADR-0208).
//!
//! The lane replays its own call log, verifies the workpiece it assembled, and
//! binds the frozen [`ScopeRevision`]'s canonical bytes into `evidence.json` at
//! `result_record.revision`. The intake records the run's `verdict` row and
//! stops, because a scoping run names no bloom and there is no `Fact` for its
//! verdict to be — so without this pass a passing run leaves the commission
//! with no revision at all, exactly as if it had never run.
//!
//! # The ledger is the trigger, not the admission
//!
//! This pass selects from `scope_runs` — every run that answered and holds no
//! `frozen` row — rather than being called with the upload the intake just
//! admitted. Three things follow, and all three are the reason:
//!
//! - **Redelivery is a no-op.** A commission that froze is not selected again,
//!   so a replayed verdict cannot write a second revision onto the chain.
//! - **The boot backfill is the same code.** A run that answered while an
//!   older binary was deployed is indistinguishable, from here, from one that
//!   answered a second ago.
//! - **A fault retries.** A freeze that could not complete leaves the run
//!   selected, so the next pass tries it again rather than losing it.
//!
//! # Where the revision comes from
//!
//! From the run's retained evidence directory, read by the dispatch nonce its
//! `dispatched` row named. The intake's `UploadedEvidence` carries only the
//! evidence body's content address, and nothing on the local lane's path puts
//! those bytes anywhere addressable — the directory the lane wrote them to is
//! the retained copy, and it is the same one the war-room evidence reads serve.

use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};

use aether_bloomery::{RevisionEvidence, ScopeRevision, ScopeVerifyInput, StageVerdict, decode_hex};

use crate::store::{ScopeVerdictRow, StoreBackend};

/// The directory suffix a lane's evidence is retained under, and the archive
/// tier's own subdirectory for the same (ADR-0211).
const EVIDENCE_SUFFIX: &str = "-evidence";
const ARCHIVE_EVIDENCE_DIR: &str = "evidence";
const EVIDENCE_FILE: &str = "evidence.json";

/// The freeze pass's own state: where retained evidence lives, and which runs
/// it has already explained itself about.
///
/// `reported` is what keeps a pass that runs every tick quiet. A run that
/// answered and cannot freeze — a failing verdict, a reclaimed evidence
/// directory, a refused report — stays selected forever by design, and warning
/// about it once a poll interval would bury the line that matters under its own
/// repetition. One warning per run per process says it exactly once and still
/// says it again after a restart, which is when an operator is looking.
pub(super) struct ScopeFreeze {
    worktree_base: PathBuf,
    archive_base: PathBuf,
    reported: BTreeSet<(String, u64)>,
}

impl ScopeFreeze {
    /// Build the pass over the coordinator's evidence roots. An empty
    /// `archive_base` resolves to `<worktree_base>/archive`, the same default
    /// the API cap's evidence reads resolve.
    pub(super) fn new(worktree_base: &str, archive_base: &str) -> Self {
        let worktree_base = PathBuf::from(worktree_base);
        let archive_base = if archive_base.is_empty() {
            worktree_base.join("archive")
        } else {
            PathBuf::from(archive_base)
        };
        Self { worktree_base, archive_base, reported: BTreeSet::new() }
    }

    /// Freeze every scoping run that passed and has not frozen, returning how
    /// many revisions this pass stored.
    ///
    /// Never fails the caller: a run that cannot freeze is one commission left
    /// unscoped, and the coordinator tick that hosts this pass also drains
    /// every dispatch topic on the board.
    pub(super) fn run(&mut self, store: &mut dyn StoreBackend) -> usize {
        let answered = match store.list_unfrozen_scope_verdicts() {
            Ok(answered) => answered,
            Err(error) => {
                tracing::warn!(
                    target: "aether_chassis_bloomery::executor",
                    %error,
                    "unfrozen scoping runs could not be listed; the freeze retries next tick",
                );
                return 0;
            }
        };
        let mut frozen = 0;
        for run in answered {
            match self.freeze(store, &run) {
                Ok(()) => frozen += 1,
                Err(why) => self.report(&run, &why),
            }
        }
        frozen
    }

    /// Freeze one run. Every path that does not store a revision is an `Err`
    /// carrying what to tell the operator — including the ordinary ones, a
    /// failing verdict and a reclaimed evidence directory, because "this run
    /// answered and its commission still has no revision" is the thing an
    /// operator needs said whatever the reason.
    fn freeze(&self, store: &mut dyn StoreBackend, run: &ScopeVerdictRow) -> Result<(), String> {
        if run.verdict != passed_spelling() {
            return Err(format!("the run's verdict is {} and only a passing run freezes", run.verdict));
        }
        let Some(nonce) = run.nonce.as_deref() else {
            return Err("the run has no dispatched row, so its evidence cannot be located".to_owned());
        };
        let Some(bytes) = self.retained_evidence(nonce) else {
            return Err(format!("no retained evidence for nonce {nonce}"));
        };
        let (revision, verify_input) = bound_revision(&bytes)?;
        // The lane names its workpiece from its own task text, so a mismatch is
        // a revision about some other commission. Refused here rather than
        // handed to the write, which would chain it onto the wrong commission's
        // history — the same guard the hand path's REST door applies.
        if revision.workpiece.0 != run.commission {
            return Err(format!("the run's revision names workpiece {}", revision.workpiece.0));
        }
        let evidence = RevisionEvidence { scope_verify: verify_input };
        let digest = store
            .freeze_scope_revision(&run.commission, run.ordinal, &revision, &evidence)
            .map_err(|error| error.to_string())?;
        tracing::info!(
            target: "aether_chassis_bloomery::executor",
            commission = %run.commission,
            ordinal = run.ordinal,
            revision = %digest.to_hex(),
            "scoping run froze its revision",
        );
        Ok(())
    }

    /// The evidence body a run wrote, from the working root first and the
    /// archive tier second.
    fn retained_evidence(&self, nonce: &str) -> Option<Vec<u8>> {
        let working = self.worktree_base.join(format!("{nonce}{EVIDENCE_SUFFIX}"));
        let archived = self.archive_base.join(ARCHIVE_EVIDENCE_DIR).join(format!("{nonce}{EVIDENCE_SUFFIX}"));
        [working, archived].into_iter().find_map(|dir| read_evidence(&dir))
    }

    /// Say once per process why a run that answered did not freeze.
    fn report(&mut self, run: &ScopeVerdictRow, why: &str) {
        if !self.reported.insert((run.commission.clone(), run.ordinal)) {
            return;
        }
        tracing::warn!(
            target: "aether_chassis_bloomery::executor",
            commission = %run.commission,
            ordinal = run.ordinal,
            why,
            "scoping run did not freeze a revision; the commission keeps its existing tip",
        );
    }
}

fn read_evidence(dir: &Path) -> Option<Vec<u8>> {
    fs::read(dir.join(EVIDENCE_FILE)).ok()
}

/// The ledger's spelling of a passing verdict, taken from the value the intake
/// records rather than a literal, so the two cannot drift apart.
fn passed_spelling() -> String {
    format!("{:?}", StageVerdict::VerificationPassed)
}

/// The revision a lane bound into its evidence, with the projection it was
/// verified over.
///
/// The projection rides along because it is what the store's freeze check needs
/// to re-verify the workpiece against its own declared surface: absent, the
/// write stores a revision nobody checked.
fn bound_revision(evidence: &[u8]) -> Result<(ScopeRevision, Option<ScopeVerifyInput>), String> {
    let body: serde_json::Value =
        serde_json::from_slice(evidence).map_err(|error| format!("the evidence is not JSON: {error}"))?;
    let record = &body["result_record"];
    let Some(hex) = record["revision"].as_str() else {
        return Err("the evidence binds no result_record.revision".to_owned());
    };
    let canonical = decode_hex(hex).ok_or("result_record.revision is not hex")?;
    let revision = ScopeRevision::from_canonical(&canonical)
        .map_err(|error| format!("result_record.revision did not decode: {error}"))?;
    Ok((revision, verify_input(record)))
}

/// The freeze-check projection, when the lane bound one. Absent is honest — a
/// revision with no projection writes no report, and the store says so rather
/// than inventing a clean one.
fn verify_input(record: &serde_json::Value) -> Option<ScopeVerifyInput> {
    let hex = record["verify_input"].as_str()?;
    ScopeVerifyInput::from_canonical(&decode_hex(hex)?).ok()
}
