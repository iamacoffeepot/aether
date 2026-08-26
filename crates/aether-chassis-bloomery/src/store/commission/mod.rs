//! The commission repository (ADR-0199 slice 2).
//!
//! Authoring and query transactions live here, not on [`StoreBackend`](super::StoreBackend).
//! That trait is the journal; this one is the signed-intent store. Both share
//! the same [`SqliteStore`] connection and
//! `PRAGMA user_version` migration.

use std::error::Error;
use std::fmt;

use aether_bloomery::{
    AuthorityDoor, BloomId, CommissionApprovalTier, CommissionProjection, CommissionStatementRole, CommissionStatus,
    CommissionValueError, Digest, KeyProvider, Observation, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision,
    ScopeVerifyInput, ScopeVerifyReport, Statement, Topic, WorkpieceId, digest_of, intent_title, verify_scope,
};
use aether_data::wire::{from_bytes, to_vec};
use rusqlite::{Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};

use super::SqliteStore;
use super::membership;

mod kinds;
pub use kinds::{
    CancelCommission, CancelCommissionResult, CreateCommission, CreateCommissionResult, EnqueueScopeRun,
    EnqueueScopeRunResult, ListCommissions, ListCommissionsResult, ListedCommission, LoadCommission,
    LoadCommissionResult, RecordCommissionApproval, RecordCommissionApprovalResult, RecordCommissionProjection,
    RecordCommissionProjectionResult, ReopenCommission, ReopenCommissionResult, WriteScopeRevision,
    WriteScopeRevisionResult,
};

#[cfg(test)]
mod tests;

/// Schema fragment for the four commission tables plus the immutability
/// triggers. Applied by the store's versioned migration; `IF NOT EXISTS` so a
/// current-build file is a no-op.
pub const COMMISSION_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS commissions (
    id               TEXT PRIMARY KEY,
    intent           BLOB NOT NULL,
    current_revision BLOB,
    current_ordinal  INTEGER,
    status           TEXT NOT NULL CHECK (status IN ('open', 'cancelled', 'landed'))
);
CREATE TABLE IF NOT EXISTS commission_statements (
    digest     BLOB PRIMARY KEY,
    commission TEXT NOT NULL REFERENCES commissions(id),
    role       TEXT NOT NULL CHECK (role IN ('intent', 'cancel')),
    canonical  BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS scope_revisions (
    digest      BLOB PRIMARY KEY,
    commission  TEXT NOT NULL REFERENCES commissions(id),
    predecessor BLOB REFERENCES scope_revisions(digest),
    ordinal     INTEGER NOT NULL CHECK (ordinal >= 1),
    canonical   BLOB NOT NULL,
    UNIQUE (commission, ordinal)
);
CREATE TABLE IF NOT EXISTS commission_approvals (
    digest       BLOB PRIMARY KEY,
    commission   TEXT NOT NULL REFERENCES commissions(id),
    scope_digest BLOB NOT NULL REFERENCES scope_revisions(digest),
    tier         TEXT NOT NULL CHECK (tier IN ('signed', 'auto')),
    statement    BLOB NOT NULL,
    signature    BLOB,
    CHECK (
        (tier = 'signed' AND signature IS NOT NULL AND length(signature) > 0)
        OR (tier = 'auto' AND signature IS NULL)
    )
);
CREATE TRIGGER IF NOT EXISTS scope_revisions_no_update
BEFORE UPDATE ON scope_revisions
BEGIN
    SELECT RAISE(ABORT, 'scope_revisions rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS scope_revisions_no_delete
BEFORE DELETE ON scope_revisions
BEGIN
    SELECT RAISE(ABORT, 'scope_revisions rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS commission_approvals_no_update
BEFORE UPDATE ON commission_approvals
BEGIN
    SELECT RAISE(ABORT, 'commission_approvals rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS commission_approvals_no_delete
BEFORE DELETE ON commission_approvals
BEGIN
    SELECT RAISE(ABORT, 'commission_approvals rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS commission_statements_no_update
BEFORE UPDATE ON commission_statements
BEGIN
    SELECT RAISE(ABORT, 'commission_statements rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS commission_statements_no_delete
BEFORE DELETE ON commission_statements
BEGIN
    SELECT RAISE(ABORT, 'commission_statements rows are immutable');
END;
";

/// Schema fragment for the scope-verify report ledger (ADR-0208).
///
/// Keyed by the digest of the bytes verified, which the freeze computes before
/// the insert — so a *refused* revision still has an identity to file its report
/// under. Deliberately no foreign key to `scope_revisions`: the refused case has
/// no row there, and a refusal that vanished when the repaired revision landed
/// would make the refusal rate unrecoverable a second time.
pub const SCOPE_VERIFY_REPORTS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS scope_verify_reports (
    revision   BLOB PRIMARY KEY,
    commission TEXT NOT NULL REFERENCES commissions(id),
    refused    INTEGER NOT NULL CHECK (refused IN (0, 1)),
    canonical  BLOB NOT NULL
);
CREATE TRIGGER IF NOT EXISTS scope_verify_reports_no_update
BEFORE UPDATE ON scope_verify_reports
BEGIN
    SELECT RAISE(ABORT, 'scope_verify_reports rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS scope_verify_reports_no_delete
BEFORE DELETE ON scope_verify_reports
BEGIN
    SELECT RAISE(ABORT, 'scope_verify_reports rows are immutable');
END;
";

/// Schema fragment for the pre-bloom scoping-run ledger (ADR-0208, #5304).
///
/// Beside `scope_revisions` because that is where every other pre-bloom fact
/// already lives, and deliberately *not* in the reducer's journal:
/// [`Snapshot`](aether_bloomery::Snapshot) keys everything by `BloomId` and
/// every `Fact` carries one, so a scoping run there would mean either a
/// synthetic bloom contaminating membership, the view, and the metrics ledger,
/// or a second reducer.
///
/// **Append-only rows, one per transition**, rather than a mutated cursor —
/// ADR-0208's own record-shape argument. The transitions are `enqueued` (the
/// run and its outbox row, in one transaction), `dispatched` (the nonce the
/// drain minted, so a run stays addressable from its outbox row alone),
/// `verdict` (from intake), and `frozen` (the revision the run produced, the
/// terminal success).
///
/// Keyed on the commission plus an attempt ordinal — the identity that exists
/// before a workpiece is frozen. `commissions.id` is written by
/// `create_commission` before any revision exists; the intent is a *field* of
/// the run rather than its key, so a re-run against an unchanged intent is a
/// new ordinal rather than a collision. The nonce index is what the intake
/// walks back from an evidence upload.
///
/// The per-transition columns are nullable because a row carries only the
/// fields its own transition names; the `kind` CHECK is what keeps the set
/// closed.
pub const SCOPE_RUNS_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS scope_runs (
    sequence   INTEGER PRIMARY KEY AUTOINCREMENT,
    commission TEXT NOT NULL REFERENCES commissions(id),
    ordinal    INTEGER NOT NULL CHECK (ordinal >= 1),
    kind       TEXT NOT NULL CHECK (kind IN ('enqueued', 'dispatched', 'verdict', 'frozen')),
    nonce      TEXT,
    intent     BLOB,
    base       BLOB,
    subject    BLOB,
    verdict    TEXT,
    evidence   BLOB,
    revision   BLOB,
    UNIQUE (commission, ordinal, kind)
);
CREATE INDEX IF NOT EXISTS scope_runs_by_commission ON scope_runs (commission, ordinal);
CREATE INDEX IF NOT EXISTS scope_runs_by_nonce ON scope_runs (nonce);
CREATE TRIGGER IF NOT EXISTS scope_runs_no_update
BEFORE UPDATE ON scope_runs
BEGIN
    SELECT RAISE(ABORT, 'scope_runs rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS scope_runs_no_delete
BEFORE DELETE ON scope_runs
BEGIN
    SELECT RAISE(ABORT, 'scope_runs rows are immutable');
END;
";

/// Schema fragment for the persisted replica-issue number (ADR-0199). The
/// projector writes title and body only to a number stored here.
pub const COMMISSION_PROJECTION_TABLE: &str = "\
CREATE TABLE IF NOT EXISTS commission_projections (
    commission   TEXT PRIMARY KEY REFERENCES commissions(id),
    issue_number INTEGER NOT NULL CHECK (issue_number > 0)
);
";

/// Why a commission repository operation was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum CommissionError {
    /// No commission exists under this workpiece id.
    MissingCommission(String),
    /// A commission under this workpiece id already exists.
    DuplicateCommission(String),
    /// The named scope revision is not in the store.
    MissingRevision,
    /// The named scope revision exists but is not the commission's current tip.
    StaleRevision,
    /// The revision bytes are already stored under a different chain position.
    DuplicateRevision,
    /// Canonical bytes did not decode, or their recomputed digest / index
    /// columns did not match the row.
    MalformedCanonical,
    /// The bytes decoded as a schema this binary does not write.
    UnsupportedSchema(u32),
    /// The revision is not the next ordinal on this commission's chain.
    OrdinalViolation {
        /// The ordinal the tip requires next.
        expected: u64,
    },
    /// An UPDATE or DELETE hit an immutable table.
    Immutable,
    /// A signed approval's author signature did not verify.
    Unverified,
    /// The statement's words are not the scope digest they must approve.
    WrongSubject,
    /// The statement's provenance is neither an author signature nor an
    /// observation attestation.
    WrongProvenance,
    /// A store-level failure, including a CHECK or FOREIGN KEY backstop.
    Store(String),
    /// The commission is not open, so a revision, approval, or cancel cannot land.
    NotOpen,
    /// The commission is not landed, so a reopen has nothing to restore.
    NotLanded(CommissionStatus),
    /// A bloom resolved this workpiece, so its landing is the ordinary one and
    /// the work it names is in mainline. Reopening it would put resolved work
    /// back in the line.
    Resolved(BloomId),
    /// The workpiece's plan steps or inverse searches name paths its own
    /// declared surface does not cover (ADR-0208). The revision is not stored;
    /// its report is.
    SurfaceGap {
        /// Each uncovered path with the record that named it.
        paths: Vec<String>,
    },
}

impl fmt::Display for CommissionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingCommission(id) => write!(f, "no commission named {id}"),
            Self::DuplicateCommission(id) => write!(f, "commission {id} already exists"),
            Self::MissingRevision => write!(f, "scope revision is not in the store"),
            Self::StaleRevision => write!(f, "scope revision is not the commission's current revision"),
            Self::DuplicateRevision => write!(f, "scope revision is already stored"),
            Self::MalformedCanonical => write!(f, "canonical commission bytes are malformed"),
            Self::UnsupportedSchema(schema) => {
                write!(f, "scope revision schema {schema} is not {SCOPE_REVISION_SCHEMA}")
            }
            Self::OrdinalViolation { expected } => {
                write!(f, "scope revision is not the next ordinal (expected {expected})")
            }
            Self::Immutable => write!(f, "commission row is immutable"),
            Self::Unverified => write!(f, "approval signature did not verify"),
            Self::WrongSubject => write!(f, "approval words are not the scope digest"),
            Self::WrongProvenance => write!(f, "approval provenance is neither signed nor auto"),
            Self::Store(message) => write!(f, "commission store: {message}"),
            Self::NotOpen => write!(f, "commission is not open"),
            Self::NotLanded(status) => write!(f, "commission is {}, not landed", status.as_str()),
            Self::Resolved(bloom) => write!(f, "bloom {} resolved this workpiece", bloom.0.to_hex()),
            Self::SurfaceGap { paths } => {
                write!(f, "declared surface does not cover {}", paths.join(", "))
            }
        }
    }
}

impl Error for CommissionError {}

impl From<rusqlite::Error> for CommissionError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(error.to_string())
    }
}

impl From<CommissionValueError> for CommissionError {
    fn from(error: CommissionValueError) -> Self {
        match error {
            CommissionValueError::Malformed => Self::MalformedCanonical,
            CommissionValueError::UnsupportedSchema(schema) => Self::UnsupportedSchema(schema),
        }
    }
}

/// One commission's durable head: identity, intent digest, and the current
/// revision pointer. The pointer is an index; [`CommissionBackend::load`]
/// recomputes the current revision from its canonical bytes.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommissionHead {
    /// The workpiece this commission is.
    pub id: WorkpieceId,
    /// Digest of the stored intent statement, recomputed from its bytes.
    pub intent: Digest,
    /// Digest of the current scope revision, when one has been written.
    pub current_revision: Option<Digest>,
    /// Store-side chain position of the current revision.
    pub current_ordinal: Option<u64>,
    /// Lifecycle flag. Not signed.
    pub status: CommissionStatus,
}

/// A commission head plus the decoded current revision, when present.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct CommissionView {
    /// The head row, with digests recomputed from stored bytes.
    pub head: CommissionHead,
    /// The current revision decoded from its canonical bytes.
    pub current: Option<ScopeRevision>,
    /// The scope-verify report journaled for the current revision (ADR-0208).
    /// `None` is **absent** — hand-forked, lane-produced-but-empty, and
    /// pre-migration revisions are honestly the same thing, and none of them is
    /// a clean report.
    pub scope_verify: Option<ScopeVerifyReport>,
    /// Why [`Self::current`] is absent even though the head names a tip.
    /// `None` when the tip is absent or readable. Trailing so an already-loaded
    /// view keeps its meaning: the head is still the commission.
    pub current_unreadable: Option<CommissionValueError>,
}

/// What is known about a revision without being part of it.
///
/// The revision's own bytes are the signed subject; nothing here is hashed
/// into them. Today's only field is the freeze-check projection (ADR-0208);
/// the next slice adds a field here rather than a parameter to the write.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RevisionEvidence {
    /// The workpiece's field records projected for the freeze check.
    /// `None` for a hand-authored revision, which writes no report.
    #[serde(default)]
    pub scope_verify: Option<ScopeVerifyInput>,
}

impl RevisionEvidence {
    /// Canonical aether-wire bytes of this sidecar.
    ///
    /// # Panics
    /// Panics if the value exceeds the ADR-0118 `u32` wire-length ceiling,
    /// which no revision sidecar does.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        to_vec(self).expect("revision evidence never exceeds the ADR-0118 u32 wire-length ceiling")
    }

    /// Decode sidecar bytes.
    ///
    /// # Errors
    /// [`CommissionError::MalformedCanonical`] when the bytes are not this type.
    pub fn decode(bytes: &[u8]) -> Result<Self, CommissionError> {
        from_bytes(bytes).map_err(|_| CommissionError::MalformedCanonical)
    }
}

/// Authoring and query transactions for signed commissions.
///
/// Separate from the journal [`StoreBackend`](super::StoreBackend) so that
/// trait does not absorb every authoring operation.
pub trait CommissionBackend {
    /// Persist a new open commission and its intent statement.
    ///
    /// # Errors
    /// [`CommissionError::DuplicateCommission`] when the id is taken.
    fn create(&mut self, id: &WorkpieceId, intent: &Statement) -> Result<Digest, CommissionError>;

    /// Store an immutable scope revision and advance `current` in one
    /// transaction. Index columns are filled from the decoded bytes.
    ///
    /// `evidence` is what is known about the revision without being part of it.
    /// The revision's own bytes are the signed subject; nothing in `evidence`
    /// is hashed into them.
    ///
    /// When `evidence.scope_verify` is `Some`, the workpiece's projected field
    /// records are checked against its own declared surface first (ADR-0208).
    /// The check is the freeze's, not the seal's: a refusal here costs an edit,
    /// where the same contradiction discovered at Member-Verify costs a whole
    /// construct budget. `None` is a hand-authored revision, which has no
    /// records to check — that writes no report, and absence is reported
    /// rather than passed.
    ///
    /// A refusal rolls the revision back and commits its report on its own, so
    /// the refusal survives the repaired re-freeze that follows it.
    ///
    /// # Errors
    /// Missing commission, not open, stale predecessor, ordinal skip, malformed
    /// bytes, a duplicate digest that is not already the current tip, or
    /// [`CommissionError::SurfaceGap`] when a named path is uncovered.
    fn write_revision(
        &mut self,
        revision: &ScopeRevision,
        evidence: &RevisionEvidence,
    ) -> Result<Digest, CommissionError>;

    /// Verify (when signed) and insert an approval for the current revision,
    /// in one transaction. Auto-tier rows carry observation provenance and no
    /// signature; signed rows must verify over
    /// [`authorization_message`](aether_bloomery::authorization_message)
    /// `(Approve, scope, scope.as_bytes())`.
    ///
    /// # Errors
    /// Missing or non-current revision, not open, wrong words, failed signature,
    /// or wrong provenance.
    fn insert_approval(&mut self, statement: &Statement, keys: &dyn KeyProvider) -> Result<Digest, CommissionError>;

    /// Load a commission and recompute its current revision from canonical
    /// bytes, or `None` when the id is unknown.
    ///
    /// An undecodable current revision is not a failure of the commission: the
    /// head is returned and [`CommissionView::current_unreadable`] names why
    /// the body could not be read. Approval, dependency, and seal paths keep
    /// using [`Self::load_revision`], which stays strict.
    fn load(&mut self, id: &WorkpieceId) -> Result<Option<CommissionView>, CommissionError>;

    /// Decode one revision from its stored bytes, recomputing the digest.
    fn load_revision(&mut self, digest: Digest) -> Result<Option<ScopeRevision>, CommissionError>;

    /// The approval statements stored for `scope`, in insert order. Each is
    /// decoded from the exact bytes that were written.
    fn load_approvals(&mut self, scope: Digest) -> Result<Vec<Statement>, CommissionError>;

    /// Dependency ids named by this revision that have no row in `commissions`.
    /// Declaration order, each id once.
    ///
    /// # Errors
    /// [`CommissionError::MissingRevision`] when `scope` is not in the store.
    /// A store-level failure when a row cannot be read.
    fn unresolved_dependencies(&mut self, scope: Digest) -> Result<Vec<WorkpieceId>, CommissionError>;

    /// Every commission matching `status`, or every commission when `None`,
    /// in workpiece-id order.
    fn list(&mut self, status: Option<CommissionStatus>) -> Result<Vec<CommissionHead>, CommissionError>;

    /// Persist a statement whose signature the caller has already verified,
    /// after confirming the referenced revision is current and belongs to `id`.
    ///
    /// # Errors
    /// Missing or non-current revision, not open, wrong words, or wrong provenance.
    fn record_verified_approval(&mut self, id: &WorkpieceId, statement: &Statement) -> Result<Digest, CommissionError>;

    /// Store a signed cancel and close the commission in one transaction.
    ///
    /// # Errors
    /// Missing commission, not open, or words that are not the intent digest.
    fn cancel(&mut self, id: &WorkpieceId, statement: &Statement) -> Result<Digest, CommissionError>;

    /// Put a landed commission back in the line, when no bloom ever resolved
    /// its workpiece.
    ///
    /// The status is the only thing that moves: the intent, every revision, and
    /// every approval are already immutable rows, so a restored commission is
    /// the one that was stranded rather than a new one wearing its id.
    ///
    /// # Errors
    /// Missing commission, a status that is not landed, a workpiece some bloom
    /// resolved, or words that are not the intent digest.
    fn reopen(&mut self, id: &WorkpieceId, statement: &Statement) -> Result<Digest, CommissionError>;

    /// Mark an open commission landed and enqueue its replica projection.
    /// Missing or already-closed commissions are a no-op so a bloom that
    /// mixed local and GitHub-era workpieces can land without failing closed.
    ///
    /// # Errors
    /// A store-level failure writing the status or the outbox row.
    fn mark_landed(&mut self, id: &WorkpieceId) -> Result<(), CommissionError>;

    /// Persist the issue number a commission projector created.
    ///
    /// # Errors
    /// [`CommissionError::MissingCommission`] when the id is unknown.
    fn record_projection(&mut self, id: &WorkpieceId, issue_number: u64) -> Result<(), CommissionError>;

    /// The issue number recorded from this commission's own create, if any.
    fn load_projection(&mut self, id: &WorkpieceId) -> Result<Option<u64>, CommissionError>;
}

impl CommissionBackend for SqliteStore {
    fn create(&mut self, id: &WorkpieceId, intent: &Statement) -> Result<Digest, CommissionError> {
        create_commission(&mut self.conn, id, intent)
    }

    fn write_revision(
        &mut self,
        revision: &ScopeRevision,
        evidence: &RevisionEvidence,
    ) -> Result<Digest, CommissionError> {
        write_revision(&mut self.conn, revision, evidence)
    }

    fn insert_approval(&mut self, statement: &Statement, keys: &dyn KeyProvider) -> Result<Digest, CommissionError> {
        insert_approval(&mut self.conn, statement, keys)
    }

    fn load(&mut self, id: &WorkpieceId) -> Result<Option<CommissionView>, CommissionError> {
        load_commission(&mut self.conn, id)
    }

    fn load_revision(&mut self, digest: Digest) -> Result<Option<ScopeRevision>, CommissionError> {
        load_revision(&self.conn, digest)
    }

    fn load_approvals(&mut self, scope: Digest) -> Result<Vec<Statement>, CommissionError> {
        load_approvals(&self.conn, scope)
    }

    fn unresolved_dependencies(&mut self, scope: Digest) -> Result<Vec<WorkpieceId>, CommissionError> {
        unresolved_dependencies(&self.conn, scope)
    }

    fn list(&mut self, status: Option<CommissionStatus>) -> Result<Vec<CommissionHead>, CommissionError> {
        list_commissions(&self.conn, status)
    }

    fn record_verified_approval(&mut self, id: &WorkpieceId, statement: &Statement) -> Result<Digest, CommissionError> {
        persist_approval(&mut self.conn, Some(id), statement)
    }

    fn cancel(&mut self, id: &WorkpieceId, statement: &Statement) -> Result<Digest, CommissionError> {
        cancel_commission(&mut self.conn, id, statement)
    }

    fn reopen(&mut self, id: &WorkpieceId, statement: &Statement) -> Result<Digest, CommissionError> {
        // Read the head first: an unknown or not-landed commission is answered
        // from one row, and the journal replay behind the resolution guard is
        // only worth paying for once the commission is a candidate to restore.
        // The transaction below re-reads both, so this read is a filter and
        // never the authority.
        let Some(head) = load_head(&self.conn, &id.0)? else {
            return Err(CommissionError::MissingCommission(id.0.clone()));
        };
        if head.status != CommissionStatus::Landed {
            return Err(CommissionError::NotLanded(head.status));
        }
        if let Some(bloom) = membership::resolving_bloom(self, id)? {
            return Err(CommissionError::Resolved(bloom));
        }
        reopen_commission(&mut self.conn, id, statement)
    }

    fn mark_landed(&mut self, id: &WorkpieceId) -> Result<(), CommissionError> {
        mark_landed(&mut self.conn, id)
    }

    fn record_projection(&mut self, id: &WorkpieceId, issue_number: u64) -> Result<(), CommissionError> {
        record_projection(&mut self.conn, id, issue_number)
    }

    fn load_projection(&mut self, id: &WorkpieceId) -> Result<Option<u64>, CommissionError> {
        load_projection(&self.conn, id)
    }
}

impl SqliteStore {
    /// The report journaled for `revision`, or `None` when none was written.
    pub fn load_scope_verify_report(&self, revision: Digest) -> Result<Option<ScopeVerifyReport>, CommissionError> {
        load_scope_verify_report(&self.conn, revision)
    }
}

fn create_commission(conn: &mut Connection, id: &WorkpieceId, intent: &Statement) -> Result<Digest, CommissionError> {
    let intent_digest = digest_of(intent);
    let intent_bytes = encode_statement(intent);
    let txn = conn.transaction()?;
    let exists: Option<String> =
        txn.query_row("SELECT id FROM commissions WHERE id = ?1", [&id.0], |row| row.get(0)).optional()?;
    if exists.is_some() {
        return Err(CommissionError::DuplicateCommission(id.0.clone()));
    }
    txn.execute(
        "INSERT INTO commissions (id, intent, current_revision, current_ordinal, status) VALUES (?1, ?2, NULL, NULL, ?3)",
        rusqlite::params![id.0, intent_digest.as_bytes().as_slice(), CommissionStatus::Open.as_str()],
    )?;
    txn.execute(
        "INSERT INTO commission_statements (digest, commission, role, canonical) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            intent_digest.as_bytes().as_slice(),
            id.0,
            CommissionStatementRole::Intent.as_str(),
            intent_bytes
        ],
    )?;
    enqueue_projection(&txn, &id.0)?;
    txn.commit()?;
    Ok(intent_digest)
}

fn write_revision(
    conn: &mut Connection,
    revision: &ScopeRevision,
    evidence: &RevisionEvidence,
) -> Result<Digest, CommissionError> {
    let canonical = revision.to_canonical();
    let decoded = decode_revision(&canonical)?;
    if decoded != *revision {
        return Err(CommissionError::MalformedCanonical);
    }
    let digest = digest_of(&decoded);
    let txn = conn.transaction()?;
    let Some(head) = load_head(&txn, &decoded.workpiece.0)? else {
        return Err(CommissionError::MissingCommission(decoded.workpiece.0));
    };
    if head.status != CommissionStatus::Open {
        return Err(CommissionError::NotOpen);
    }

    if revision_exists(&txn, digest)? {
        if head.current_revision == Some(digest) {
            return Ok(digest);
        }
        return Err(CommissionError::DuplicateRevision);
    }

    let report = evidence.scope_verify.as_ref().map(verify_scope);
    if let Some(report) = &report
        && report.refused()
    {
        drop(txn);
        let refusal = conn.transaction()?;
        insert_scope_verify_report(&refusal, digest, &decoded.workpiece.0, report)?;
        refusal.commit()?;
        return Err(CommissionError::SurfaceGap { paths: report.refusal_paths() });
    }

    let expected_ordinal = match (&head.current_revision, decoded.predecessor) {
        (None, None) => 1,
        (Some(current), Some(predecessor)) if *current == predecessor => {
            head.current_ordinal.expect("a current revision always carries its ordinal") + 1
        }
        (Some(_), None) => {
            return Err(CommissionError::OrdinalViolation {
                expected: head.current_ordinal.expect("a current revision always carries its ordinal") + 1,
            });
        }
        (Some(_), Some(_)) => return Err(CommissionError::StaleRevision),
        (None, Some(_)) => return Err(CommissionError::MissingRevision),
    };

    let predecessor = decoded.predecessor.map(|digest| digest.as_bytes().to_vec());
    let ordinal = i64::try_from(expected_ordinal).unwrap_or(i64::MAX);
    txn.execute(
        "INSERT INTO scope_revisions (digest, commission, predecessor, ordinal, canonical) VALUES (?1, ?2, ?3, ?4, ?5)",
        rusqlite::params![digest.as_bytes().as_slice(), decoded.workpiece.0, predecessor, ordinal, canonical],
    )
    .map_err(map_write)?;
    txn.execute(
        "UPDATE commissions SET current_revision = ?1, current_ordinal = ?2 WHERE id = ?3",
        rusqlite::params![digest.as_bytes().as_slice(), ordinal, decoded.workpiece.0],
    )?;
    if let Some(report) = &report {
        insert_scope_verify_report(&txn, digest, &decoded.workpiece.0, report)?;
    }
    enqueue_projection(&txn, &decoded.workpiece.0)?;
    txn.commit()?;
    Ok(digest)
}

/// File one scope-verify report under the digest of the bytes it verified.
///
/// `INSERT OR IGNORE` because the report is a pure function of those bytes: a
/// re-submitted revision recomputes the identical report, and the row is
/// immutable by trigger. Silently keeping the first is correct; an UPDATE would
/// abort.
fn insert_scope_verify_report(
    txn: &Transaction<'_>,
    revision: Digest,
    commission: &str,
    report: &ScopeVerifyReport,
) -> Result<(), CommissionError> {
    txn.execute(
        "INSERT OR IGNORE INTO scope_verify_reports (revision, commission, refused, canonical) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            revision.as_bytes().as_slice(),
            commission,
            i64::from(report.refused()),
            report.to_canonical()
        ],
    )?;
    Ok(())
}

/// The report journaled for `revision`, or `None` when none was written.
fn load_scope_verify_report(conn: &Connection, revision: Digest) -> Result<Option<ScopeVerifyReport>, CommissionError> {
    let canonical: Option<Vec<u8>> = conn
        .query_row(
            "SELECT canonical FROM scope_verify_reports WHERE revision = ?1",
            [revision.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    canonical.map(|bytes| ScopeVerifyReport::from_canonical(&bytes).map_err(CommissionError::from)).transpose()
}

fn insert_approval(
    conn: &mut Connection,
    statement: &Statement,
    keys: &dyn KeyProvider,
) -> Result<Digest, CommissionError> {
    let scope = Digest::from_slice(&statement.words).ok_or(CommissionError::WrongSubject)?;
    let (tier, _) = classify_approval(statement)?;
    if tier == CommissionApprovalTier::Signed && !statement.verify_authority(keys, AuthorityDoor::Approve, scope) {
        return Err(CommissionError::Unverified);
    }
    persist_approval(conn, None, statement)
}

fn persist_approval(
    conn: &mut Connection,
    expected: Option<&WorkpieceId>,
    statement: &Statement,
) -> Result<Digest, CommissionError> {
    let scope = Digest::from_slice(&statement.words).ok_or(CommissionError::WrongSubject)?;
    let (tier, signature) = classify_approval(statement)?;
    let statement_digest = digest_of(statement);
    let statement_bytes = encode_statement(statement);
    let txn = conn.transaction()?;
    let Some(revision) = load_revision(&txn, scope)? else {
        return Err(CommissionError::MissingRevision);
    };
    let Some(head) = load_head(&txn, &revision.workpiece.0)? else {
        return Err(CommissionError::MissingCommission(revision.workpiece.0));
    };
    if expected.is_some_and(|id| id.0 != revision.workpiece.0) {
        return Err(CommissionError::WrongSubject);
    }
    if head.status != CommissionStatus::Open {
        return Err(CommissionError::NotOpen);
    }
    if head.current_revision != Some(scope) {
        return Err(CommissionError::StaleRevision);
    }

    if approval_exists(&txn, statement_digest)? {
        txn.commit()?;
        return Ok(statement_digest);
    }

    txn.execute(
        "INSERT INTO commission_approvals (digest, commission, scope_digest, tier, statement, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        rusqlite::params![
            statement_digest.as_bytes().as_slice(),
            revision.workpiece.0,
            scope.as_bytes().as_slice(),
            tier.as_str(),
            statement_bytes,
            signature,
        ],
    )
    .map_err(map_write)?;
    enqueue_projection(&txn, &revision.workpiece.0)?;
    txn.commit()?;
    Ok(statement_digest)
}

fn cancel_commission(
    conn: &mut Connection,
    id: &WorkpieceId,
    statement: &Statement,
) -> Result<Digest, CommissionError> {
    let intent = Digest::from_slice(&statement.words).ok_or(CommissionError::WrongSubject)?;
    let statement_digest = digest_of(statement);
    let statement_bytes = encode_statement(statement);
    let txn = conn.transaction()?;
    let Some(head) = load_head(&txn, &id.0)? else {
        return Err(CommissionError::MissingCommission(id.0.clone()));
    };
    if head.status != CommissionStatus::Open {
        return Err(CommissionError::NotOpen);
    }
    if head.intent != intent {
        return Err(CommissionError::WrongSubject);
    }

    txn.execute(
        "INSERT INTO commission_statements (digest, commission, role, canonical) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![
            statement_digest.as_bytes().as_slice(),
            id.0,
            CommissionStatementRole::Cancel.as_str(),
            statement_bytes
        ],
    )
    .map_err(map_write)?;
    txn.execute(
        "UPDATE commissions SET status = ?1 WHERE id = ?2",
        rusqlite::params![CommissionStatus::Cancelled.as_str(), id.0],
    )?;
    enqueue_projection(&txn, &id.0)?;
    txn.commit()?;
    Ok(statement_digest)
}

/// Flip a landed commission back to open under one transaction, re-checking
/// the status and the intent binding the caller filtered on.
///
/// The reopen statement is not filed beside the intent and the cancel:
/// `commission_statements.role` is a closed `CHECK` on a table that already
/// exists on every live journal, so adding a role means migrating a running
/// coordinator's store to record an act that restores rather than concludes.
/// The signature is verified at the Reopen door before this is reached and the
/// route logs the signer and the reason, so what a stored row would add is a
/// second copy of an authorization that has already been checked.
fn reopen_commission(
    conn: &mut Connection,
    id: &WorkpieceId,
    statement: &Statement,
) -> Result<Digest, CommissionError> {
    let intent = Digest::from_slice(&statement.words).ok_or(CommissionError::WrongSubject)?;
    let statement_digest = digest_of(statement);
    let txn = conn.transaction()?;
    let Some(head) = load_head(&txn, &id.0)? else {
        return Err(CommissionError::MissingCommission(id.0.clone()));
    };
    if head.status != CommissionStatus::Landed {
        return Err(CommissionError::NotLanded(head.status));
    }
    if head.intent != intent {
        return Err(CommissionError::WrongSubject);
    }

    txn.execute(
        "UPDATE commissions SET status = ?1 WHERE id = ?2",
        rusqlite::params![CommissionStatus::Open.as_str(), id.0],
    )?;
    enqueue_projection(&txn, &id.0)?;
    txn.commit()?;
    Ok(statement_digest)
}

fn mark_landed(conn: &mut Connection, id: &WorkpieceId) -> Result<(), CommissionError> {
    let txn = conn.transaction()?;
    let Some(head) = load_head(&txn, &id.0)? else {
        return Ok(());
    };
    if head.status != CommissionStatus::Open {
        return Ok(());
    }
    txn.execute(
        "UPDATE commissions SET status = ?1 WHERE id = ?2",
        rusqlite::params![CommissionStatus::Landed.as_str(), id.0],
    )?;
    enqueue_projection(&txn, &id.0)?;
    txn.commit()?;
    Ok(())
}

fn record_projection(conn: &mut Connection, id: &WorkpieceId, issue_number: u64) -> Result<(), CommissionError> {
    let txn = conn.transaction()?;
    if load_head(&txn, &id.0)?.is_none() {
        return Err(CommissionError::MissingCommission(id.0.clone()));
    }
    txn.execute(
        "INSERT INTO commission_projections (commission, issue_number) VALUES (?1, ?2)
         ON CONFLICT(commission) DO UPDATE SET issue_number = excluded.issue_number",
        rusqlite::params![id.0, i64::try_from(issue_number).unwrap_or(i64::MAX)],
    )?;
    txn.commit()?;
    Ok(())
}

fn load_projection(conn: &Connection, id: &WorkpieceId) -> Result<Option<u64>, CommissionError> {
    load_recorded_issue(conn, &id.0)
}

fn enqueue_projection(txn: &Transaction<'_>, id: &str) -> Result<(), CommissionError> {
    let payload = snapshot_projection(txn, id)?;
    txn.execute(
        "INSERT INTO outbox (topic, payload) VALUES (?1, ?2)",
        rusqlite::params![Topic::Commission.as_str(), payload],
    )?;
    Ok(())
}

fn snapshot_projection(conn: &Connection, id: &str) -> Result<Vec<u8>, CommissionError> {
    let Some(head) = load_head(conn, id)? else {
        return Err(CommissionError::MissingCommission(id.to_owned()));
    };
    let recorded_issue = load_recorded_issue(conn, id)?;
    let (approval_signer, approval_digest) = match head.current_revision {
        Some(scope) => first_approval(conn, scope)?,
        None => (None, None),
    };
    // Read out of the stored intent bytes rather than carried on the head row:
    // the head is an index, and a title recomputed from the bytes cannot drift
    // from the intent the commission was created with.
    let title = load_statement(conn, head.intent)?.and_then(|intent| intent_title(&intent.words)).unwrap_or_default();
    to_vec(&CommissionProjection {
        workpiece: head.id,
        intent: head.intent,
        scope_revision: head.current_revision,
        approval_signer,
        approval_digest,
        status: head.status.as_str().to_owned(),
        recorded_issue,
        title,
    })
    .map_err(|error| CommissionError::Store(error.to_string()))
}

/// One stored commission statement, decoded from the exact bytes written.
fn load_statement(conn: &Connection, digest: Digest) -> Result<Option<Statement>, CommissionError> {
    let canonical: Option<Vec<u8>> = conn
        .query_row(
            "SELECT canonical FROM commission_statements WHERE digest = ?1",
            [digest.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .optional()?;
    canonical.map(|bytes| from_bytes::<Statement>(&bytes).map_err(|_| CommissionError::MalformedCanonical)).transpose()
}

fn first_approval(conn: &Connection, scope: Digest) -> Result<(Option<String>, Option<Digest>), CommissionError> {
    let approvals = load_approvals(conn, scope)?;
    let Some(statement) = approvals.first() else {
        return Ok((None, None));
    };
    let signer = match &statement.provenance {
        Provenance::AuthorSignature(envelope) => Some(envelope.signer.0.clone()),
        Provenance::ObservationAttestation(observation) => Some(observation.source.clone()),
        Provenance::StageReceipt(_) => None,
    };
    Ok((signer, Some(digest_of(statement))))
}

fn load_recorded_issue(conn: &Connection, id: &str) -> Result<Option<u64>, CommissionError> {
    let number: Option<i64> = conn
        .query_row("SELECT issue_number FROM commission_projections WHERE commission = ?1", [id], |row| row.get(0))
        .optional()?;
    match number {
        Some(value) => Ok(Some(u64::try_from(value).map_err(|_| CommissionError::MalformedCanonical)?)),
        None => Ok(None),
    }
}

/// Overlay the store's recorded replica-issue number onto a drained
/// commission payload. The outbox row is a frozen snapshot; this table is
/// the create-vs-update authority for a later entry of the same commission.
pub fn overlay_recorded_projection(conn: &Connection, payload: &mut Vec<u8>) {
    let Ok(mut document) = from_bytes::<CommissionProjection>(payload) else {
        return;
    };
    let Ok(Some(number)) = load_recorded_issue(conn, &document.workpiece.0) else {
        return;
    };
    if document.recorded_issue == Some(number) {
        return;
    }
    document.recorded_issue = Some(number);
    if let Ok(bytes) = to_vec(&document) {
        *payload = bytes;
    }
}

fn load_commission(conn: &mut Connection, id: &WorkpieceId) -> Result<Option<CommissionView>, CommissionError> {
    let Some(head) = load_head(conn, &id.0)? else {
        return Ok(None);
    };
    let (current, current_unreadable) = match head.current_revision {
        Some(digest) => match load_revision(conn, digest) {
            Ok(Some(revision)) => (Some(revision), None),
            Ok(None) => return Err(CommissionError::MalformedCanonical),
            Err(CommissionError::MalformedCanonical) => (None, Some(CommissionValueError::Malformed)),
            Err(CommissionError::UnsupportedSchema(schema)) => {
                (None, Some(CommissionValueError::UnsupportedSchema(schema)))
            }
            Err(error) => return Err(error),
        },
        None => (None, None),
    };
    let scope_verify = match head.current_revision {
        Some(digest) => load_scope_verify_report(conn, digest)?,
        None => None,
    };
    Ok(Some(CommissionView { head, current, scope_verify, current_unreadable }))
}

fn load_revision(conn: &Connection, digest: Digest) -> Result<Option<ScopeRevision>, CommissionError> {
    let row = conn
        .query_row(
            "SELECT digest, commission, predecessor, ordinal, canonical FROM scope_revisions WHERE digest = ?1",
            [digest.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<Vec<u8>>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_digest, commission, predecessor, _ordinal, canonical)) = row else {
        return Ok(None);
    };
    let decoded = decode_revision(&canonical)?;
    if digest_of(&decoded) != digest
        || Digest::from_slice(&stored_digest) != Some(digest)
        || decoded.workpiece.0 != commission
        || decoded.predecessor.map(|item| item.as_bytes().to_vec()) != predecessor
    {
        return Err(CommissionError::MalformedCanonical);
    }
    Ok(Some(decoded))
}

fn unresolved_dependencies(conn: &Connection, scope: Digest) -> Result<Vec<WorkpieceId>, CommissionError> {
    let revision = load_revision(conn, scope)?.ok_or(CommissionError::MissingRevision)?;
    let mut unresolved = Vec::new();
    for id in &revision.dependencies {
        if unresolved.contains(id) {
            continue;
        }
        let found: Option<i64> =
            conn.query_row("SELECT 1 FROM commissions WHERE id = ?1", [&id.0], |row| row.get(0)).optional()?;
        if found.is_none() {
            unresolved.push(id.clone());
        }
    }
    Ok(unresolved)
}

fn load_approvals(conn: &Connection, scope: Digest) -> Result<Vec<Statement>, CommissionError> {
    let mut stmt =
        conn.prepare("SELECT digest, statement FROM commission_approvals WHERE scope_digest = ?1 ORDER BY rowid")?;
    let rows = stmt
        .query_map([scope.as_bytes().as_slice()], |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)))?;
    let mut approvals = Vec::new();
    for row in rows {
        let (digest_bytes, canonical) = row?;
        let statement: Statement = from_bytes(&canonical).map_err(|_| CommissionError::MalformedCanonical)?;
        let stored = Digest::from_slice(&digest_bytes).ok_or(CommissionError::MalformedCanonical)?;
        if digest_of(&statement) != stored {
            return Err(CommissionError::MalformedCanonical);
        }
        approvals.push(statement);
    }
    Ok(approvals)
}

fn list_commissions(
    conn: &Connection,
    status: Option<CommissionStatus>,
) -> Result<Vec<CommissionHead>, CommissionError> {
    let mut stmt = conn.prepare(
        "SELECT id, intent, current_revision, current_ordinal, status FROM commissions
             WHERE (?1 IS NULL OR status = ?1) ORDER BY id",
    )?;
    let status = status.map(CommissionStatus::as_str);
    let rows = stmt.query_map([status], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<Vec<u8>>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    let mut heads = Vec::new();
    for row in rows {
        let (id, intent, current, ordinal, status) = row?;
        heads.push(recompute_head(conn, id, &intent, current, ordinal, &status)?);
    }
    Ok(heads)
}

fn load_head(conn: &Connection, id: &str) -> Result<Option<CommissionHead>, CommissionError> {
    let row = conn
        .query_row(
            "SELECT intent, current_revision, current_ordinal, status FROM commissions WHERE id = ?1",
            [id],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()?;
    match row {
        Some((intent, current, ordinal, status)) => {
            Ok(Some(recompute_head(conn, id.to_owned(), &intent, current, ordinal, &status)?))
        }
        None => Ok(None),
    }
}

fn recompute_head(
    conn: &Connection,
    id: String,
    intent_bytes: &[u8],
    current: Option<Vec<u8>>,
    ordinal: Option<i64>,
    status: &str,
) -> Result<CommissionHead, CommissionError> {
    let stored_intent = Digest::from_slice(intent_bytes).ok_or(CommissionError::MalformedCanonical)?;
    let intent = recompute_intent(conn, &id, stored_intent)?;
    let current_revision = match current {
        Some(bytes) => Some(Digest::from_slice(&bytes).ok_or(CommissionError::MalformedCanonical)?),
        None => None,
    };
    let current_ordinal = match ordinal {
        Some(value) => Some(u64::try_from(value).map_err(|_| CommissionError::MalformedCanonical)?),
        None => None,
    };
    let status = CommissionStatus::parse(status).ok_or(CommissionError::MalformedCanonical)?;
    Ok(CommissionHead { id: WorkpieceId(id), intent, current_revision, current_ordinal, status })
}

fn recompute_intent(conn: &Connection, commission: &str, stored: Digest) -> Result<Digest, CommissionError> {
    let canonical: Vec<u8> = conn
        .query_row(
            "SELECT canonical FROM commission_statements WHERE digest = ?1 AND commission = ?2 AND role = ?3",
            rusqlite::params![stored.as_bytes().as_slice(), commission, CommissionStatementRole::Intent.as_str()],
            |row| row.get(0),
        )
        .map_err(|_| CommissionError::MalformedCanonical)?;
    let statement: Statement = from_bytes(&canonical).map_err(|_| CommissionError::MalformedCanonical)?;
    let digest = digest_of(&statement);
    if digest != stored {
        return Err(CommissionError::MalformedCanonical);
    }
    Ok(digest)
}

fn revision_exists(conn: &Transaction<'_>, digest: Digest) -> Result<bool, CommissionError> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM scope_revisions WHERE digest = ?1", [digest.as_bytes().as_slice()], |row| row.get(0))
        .optional()?;
    Ok(found.is_some())
}

fn approval_exists(conn: &Transaction<'_>, digest: Digest) -> Result<bool, CommissionError> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM commission_approvals WHERE digest = ?1", [digest.as_bytes().as_slice()], |row| {
            row.get(0)
        })
        .optional()?;
    Ok(found.is_some())
}

fn classify_approval(statement: &Statement) -> Result<(CommissionApprovalTier, Option<Vec<u8>>), CommissionError> {
    match &statement.provenance {
        Provenance::AuthorSignature(envelope) => {
            if envelope.signature.is_empty() {
                return Err(CommissionError::Unverified);
            }
            Ok((CommissionApprovalTier::Signed, Some(envelope.signature.clone())))
        }
        Provenance::ObservationAttestation(Observation { .. }) => Ok((CommissionApprovalTier::Auto, None)),
        Provenance::StageReceipt(_) => Err(CommissionError::WrongProvenance),
    }
}

fn decode_revision(bytes: &[u8]) -> Result<ScopeRevision, CommissionError> {
    ScopeRevision::from_canonical(bytes).map_err(|error| match error {
        CommissionValueError::Malformed => CommissionError::MalformedCanonical,
        CommissionValueError::UnsupportedSchema(schema) => CommissionError::UnsupportedSchema(schema),
    })
}

fn encode_statement(statement: &Statement) -> Vec<u8> {
    to_vec(statement).expect("commission statements never exceed the ADR-0118 u32 wire-length ceiling")
}

fn map_write(error: rusqlite::Error) -> CommissionError {
    if let rusqlite::Error::SqliteFailure(_, Some(message)) = &error {
        if message.contains("immutable") {
            return CommissionError::Immutable;
        }
        if message.contains("UNIQUE constraint failed") {
            return CommissionError::DuplicateRevision;
        }
        if message.contains("FOREIGN KEY constraint failed") {
            return CommissionError::MissingRevision;
        }
        if message.contains("CHECK constraint failed") {
            return CommissionError::Store(message.clone());
        }
    }
    error.into()
}
