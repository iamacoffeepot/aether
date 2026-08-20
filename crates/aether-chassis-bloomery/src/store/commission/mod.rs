//! The commission repository (ADR-0199 slice 2).
//!
//! Authoring and query transactions live here, not on [`StoreBackend`](super::StoreBackend).
//! That trait is the journal; this one is the signed-intent store. Both share
//! the same [`SqliteStore`] connection and
//! `PRAGMA user_version` migration.

use std::error::Error;
use std::fmt;

use aether_bloomery::{
    AuthorityDoor, CommissionApprovalTier, CommissionProjection, CommissionStatementRole, CommissionStatus,
    CommissionValueError, Digest, KeyProvider, Observation, Provenance, SCOPE_REVISION_SCHEMA, ScopeRevision,
    Statement, Topic, WorkpieceId, digest_of,
};
use aether_data::wire::{from_bytes, to_vec};
use rusqlite::{Connection, OptionalExtension, Transaction};

use super::SqliteStore;

mod kinds;
pub use kinds::{
    CancelCommission, CancelCommissionResult, CreateCommission, CreateCommissionResult, ListCommissions,
    ListCommissionsResult, ListedCommission, LoadCommission, LoadCommissionResult, RecordCommissionApproval,
    RecordCommissionApprovalResult, RecordCommissionProjection, RecordCommissionProjectionResult, WriteScopeRevision,
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
    /// # Errors
    /// Missing commission, not open, stale predecessor, ordinal skip, malformed
    /// bytes, or a duplicate digest that is not already the current tip.
    fn write_revision(&mut self, revision: &ScopeRevision) -> Result<Digest, CommissionError>;

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

    fn write_revision(&mut self, revision: &ScopeRevision) -> Result<Digest, CommissionError> {
        write_revision(&mut self.conn, revision)
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

fn write_revision(conn: &mut Connection, revision: &ScopeRevision) -> Result<Digest, CommissionError> {
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
    enqueue_projection(&txn, &decoded.workpiece.0)?;
    txn.commit()?;
    Ok(digest)
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
    to_vec(&CommissionProjection {
        workpiece: head.id,
        intent: head.intent,
        scope_revision: head.current_revision,
        approval_signer,
        approval_digest,
        status: head.status.as_str().to_owned(),
        recorded_issue,
    })
    .map_err(|error| CommissionError::Store(error.to_string()))
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
    let current = match head.current_revision {
        Some(digest) => Some(load_revision(conn, digest)?.ok_or(CommissionError::MalformedCanonical)?),
        None => None,
    };
    Ok(Some(CommissionView { head, current }))
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
    let Some(revision) = load_revision(conn, scope)? else {
        return Err(CommissionError::MissingRevision);
    };
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
