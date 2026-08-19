//! Architecture decision records as first-class store objects (ADR-0201).
//!
//! Authoring and query transactions live here, not on
//! [`StoreBackend`](super::StoreBackend). Status is an append-only transition
//! log: no unsigned status column on `adrs` is ever authoritative.

use std::error::Error;
use std::fmt::{self, Display, Formatter};

use aether_bloomery::{
    ADR_SCHEMA, ADR_TRANSITION_SCHEMA, Adr, AdrStatus, AdrTransition, AdrValueError, AuthorityDoor, Digest,
    KeyProvider, Provenance, Statement, digest_of,
};
use aether_data::wire::to_vec;
use rusqlite::{Connection, OptionalExtension, Transaction};

use super::SqliteStore;

#[cfg(test)]
mod tests;

/// Schema fragment for the ADR tables plus the immutability triggers.
/// Applied by the store's versioned migration; `IF NOT EXISTS` so a
/// current-build file is a no-op.
pub const ADR_TABLES: &str = "\
CREATE TABLE IF NOT EXISTS adrs (
    digest    BLOB PRIMARY KEY,
    number    INTEGER NOT NULL UNIQUE CHECK (number > 0),
    title     TEXT NOT NULL,
    canonical BLOB NOT NULL
);
CREATE TABLE IF NOT EXISTS adr_transitions (
    digest     BLOB PRIMARY KEY,
    adr        BLOB NOT NULL REFERENCES adrs(digest),
    status     TEXT NOT NULL CHECK (status IN ('proposed', 'provisional', 'accepted', 'superseded')),
    canonical  BLOB NOT NULL,
    statement  BLOB,
    signature  BLOB,
    successor  BLOB REFERENCES adrs(digest),
    CHECK (
        (status = 'accepted' AND signature IS NOT NULL AND length(signature) > 0 AND statement IS NOT NULL)
        OR (status IN ('proposed', 'provisional', 'superseded') AND signature IS NULL AND statement IS NULL)
    ),
    CHECK (
        (status = 'superseded' AND successor IS NOT NULL)
        OR (status != 'superseded' AND successor IS NULL)
    )
);
CREATE TRIGGER IF NOT EXISTS adrs_no_update
BEFORE UPDATE ON adrs
BEGIN
    SELECT RAISE(ABORT, 'adrs rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS adrs_no_delete
BEFORE DELETE ON adrs
BEGIN
    SELECT RAISE(ABORT, 'adrs rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS adr_transitions_no_update
BEFORE UPDATE ON adr_transitions
BEGIN
    SELECT RAISE(ABORT, 'adr_transitions rows are immutable');
END;
CREATE TRIGGER IF NOT EXISTS adr_transitions_no_delete
BEFORE DELETE ON adr_transitions
BEGIN
    SELECT RAISE(ABORT, 'adr_transitions rows are immutable');
END;
";

/// Why an ADR repository operation was refused.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum AdrError {
    /// No ADR exists under this digest.
    Missing,
    /// An ADR under this number already exists with different bytes.
    NumberTaken(u32),
    /// Canonical bytes did not decode, or their recomputed digest / index
    /// columns did not match the row.
    MalformedCanonical,
    /// The bytes decoded as a schema this binary does not write.
    UnsupportedSchema(u32),
    /// The transition is not legal from the ADR's current status.
    WrongStatus {
        /// The status the tip currently holds.
        current: AdrStatus,
    },
    /// A signed acceptance's author signature did not verify.
    Unverified,
    /// The statement's words are not the ADR digest they must accept.
    WrongSubject,
    /// The statement's provenance is not an author signature.
    WrongProvenance,
    /// The named successor is missing, or is the ADR being superseded.
    BadSuccessor,
    /// An UPDATE or DELETE hit an immutable table.
    Immutable,
    /// A store-level failure, including a CHECK or FOREIGN KEY backstop.
    Store(String),
}

impl Display for AdrError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::Missing => write!(f, "architecture decision record is not in the store"),
            Self::NumberTaken(number) => write!(f, "ADR-{number:04} already exists"),
            Self::MalformedCanonical => write!(f, "canonical ADR bytes are malformed"),
            Self::UnsupportedSchema(schema) => write!(f, "ADR schema {schema} is not {ADR_SCHEMA}"),
            Self::WrongStatus { current } => {
                write!(f, "architecture decision record is {}", current.as_str())
            }
            Self::Unverified => write!(f, "acceptance signature did not verify"),
            Self::WrongSubject => write!(f, "acceptance words are not the ADR digest"),
            Self::WrongProvenance => write!(f, "acceptance provenance is not a signature"),
            Self::BadSuccessor => write!(f, "supersession successor is missing or self"),
            Self::Immutable => write!(f, "ADR row is immutable"),
            Self::Store(message) => write!(f, "ADR store: {message}"),
        }
    }
}

impl Error for AdrError {}

impl From<rusqlite::Error> for AdrError {
    fn from(error: rusqlite::Error) -> Self {
        let message = error.to_string();
        if message.contains("immutable") {
            Self::Immutable
        } else {
            Self::Store(message)
        }
    }
}

impl From<AdrValueError> for AdrError {
    fn from(error: AdrValueError) -> Self {
        match error {
            AdrValueError::Malformed => Self::MalformedCanonical,
            AdrValueError::UnsupportedSchema(schema) => Self::UnsupportedSchema(schema),
        }
    }
}

/// An ADR plus the status its last transition asserts.
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct AdrView {
    /// Digest of the stored ADR, recomputed from its bytes.
    pub digest: Digest,
    /// The decoded ADR.
    pub adr: Adr,
    /// Status of the latest transition. Never read from an `adrs` column.
    pub status: AdrStatus,
    /// Evidence citations from an Accepted transition. Empty otherwise.
    pub citations: Vec<Digest>,
    /// Successor digest when the latest transition superseded this ADR.
    pub successor: Option<Digest>,
}

/// Authoring and query transactions for stored ADRs.
pub trait AdrBackend {
    /// Persist a Proposed ADR. The first transition is unsigned.
    ///
    /// # Errors
    /// Number taken by different bytes, malformed canonical, or store failure.
    fn propose(&mut self, adr: &Adr) -> Result<Digest, AdrError>;

    /// Append an unsigned Provisional transition. Work may proceed pending
    /// owner ratification.
    ///
    /// # Errors
    /// Missing ADR, or a tip that is not Proposed.
    fn mark_provisional(&mut self, adr: Digest) -> Result<Digest, AdrError>;

    /// Append an owner-signed Accepted transition over `adr`.
    ///
    /// The statement verifies over [`aether_bloomery::authorization_message`]
    /// `(Accept, adr, adr.as_bytes())`. `citations` may be empty.
    ///
    /// # Errors
    /// Missing ADR, tip that is not Provisional, wrong words, or failed
    /// signature.
    fn accept(
        &mut self,
        adr: Digest,
        statement: &Statement,
        citations: &[Digest],
        keys: &dyn KeyProvider,
    ) -> Result<Digest, AdrError>;

    /// Append a Superseded transition naming `successor`. Consumes Provisional.
    ///
    /// # Errors
    /// Missing ADR, tip that is not Provisional, or a missing/self successor.
    fn supersede(&mut self, adr: Digest, successor: Digest) -> Result<Digest, AdrError>;

    /// Load an ADR and recompute its digest and current status from stored
    /// bytes, or `None` when the digest is unknown.
    fn load(&mut self, digest: Digest) -> Result<Option<AdrView>, AdrError>;

    /// Load by sequential number.
    fn load_by_number(&mut self, number: u32) -> Result<Option<AdrView>, AdrError>;

    /// Every stored ADR, in number order.
    fn list(&mut self) -> Result<Vec<AdrView>, AdrError>;
}

impl AdrBackend for SqliteStore {
    fn propose(&mut self, adr: &Adr) -> Result<Digest, AdrError> {
        propose(&mut self.conn, adr)
    }

    fn mark_provisional(&mut self, adr: Digest) -> Result<Digest, AdrError> {
        mark_provisional(&mut self.conn, adr)
    }

    fn accept(
        &mut self,
        adr: Digest,
        statement: &Statement,
        citations: &[Digest],
        keys: &dyn KeyProvider,
    ) -> Result<Digest, AdrError> {
        accept(&mut self.conn, adr, statement, citations, keys)
    }

    fn supersede(&mut self, adr: Digest, successor: Digest) -> Result<Digest, AdrError> {
        supersede(&mut self.conn, adr, successor)
    }

    fn load(&mut self, digest: Digest) -> Result<Option<AdrView>, AdrError> {
        load_view(&self.conn, digest)
    }

    fn load_by_number(&mut self, number: u32) -> Result<Option<AdrView>, AdrError> {
        load_by_number(&self.conn, number)
    }

    fn list(&mut self) -> Result<Vec<AdrView>, AdrError> {
        list_adrs(&self.conn)
    }
}

fn propose(conn: &mut Connection, adr: &Adr) -> Result<Digest, AdrError> {
    let canonical = adr.to_canonical();
    let decoded = decode_adr(&canonical)?;
    if decoded != *adr {
        return Err(AdrError::MalformedCanonical);
    }
    if decoded.number == 0 || decoded.title.is_empty() {
        return Err(AdrError::MalformedCanonical);
    }
    let digest = digest_of(&decoded);
    let txn = conn.transaction()?;
    if let Some(existing) = load_digest_for_number(&txn, decoded.number)? {
        if existing == digest {
            txn.commit()?;
            return Ok(digest);
        }
        return Err(AdrError::NumberTaken(decoded.number));
    }
    if adr_exists(&txn, digest)? {
        txn.commit()?;
        return Ok(digest);
    }

    txn.execute(
        "INSERT INTO adrs (digest, number, title, canonical) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![digest.as_bytes().as_slice(), decoded.number, decoded.title, canonical],
    )?;
    insert_transition(
        &txn,
        &AdrTransition {
            schema: ADR_TRANSITION_SCHEMA,
            adr: digest,
            status: AdrStatus::Proposed,
            citations: Vec::new(),
            successor: None,
        },
        None,
        None,
    )?;
    txn.commit()?;
    Ok(digest)
}

fn mark_provisional(conn: &mut Connection, adr: Digest) -> Result<Digest, AdrError> {
    let txn = conn.transaction()?;
    let current = current_status(&txn, adr)?.ok_or(AdrError::Missing)?;
    if current != AdrStatus::Proposed {
        return Err(AdrError::WrongStatus { current });
    }
    let digest = insert_transition(
        &txn,
        &AdrTransition {
            schema: ADR_TRANSITION_SCHEMA,
            adr,
            status: AdrStatus::Provisional,
            citations: Vec::new(),
            successor: None,
        },
        None,
        None,
    )?;
    txn.commit()?;
    Ok(digest)
}

fn accept(
    conn: &mut Connection,
    adr: Digest,
    statement: &Statement,
    citations: &[Digest],
    keys: &dyn KeyProvider,
) -> Result<Digest, AdrError> {
    let words = Digest::from_slice(&statement.words).ok_or(AdrError::WrongSubject)?;
    if words != adr {
        return Err(AdrError::WrongSubject);
    }
    let signature = match &statement.provenance {
        Provenance::AuthorSignature(envelope) => {
            if envelope.signature.is_empty() {
                return Err(AdrError::WrongProvenance);
            }
            envelope.signature.clone()
        }
        Provenance::ObservationAttestation(_) | Provenance::StageReceipt(_) => {
            return Err(AdrError::WrongProvenance);
        }
    };
    let statement_bytes = to_vec(statement).map_err(|error| AdrError::Store(error.to_string()))?;
    if !statement.verify_authority(keys, AuthorityDoor::Accept, adr) {
        return Err(AdrError::Unverified);
    }

    let txn = conn.transaction()?;
    let current = current_status(&txn, adr)?.ok_or(AdrError::Missing)?;
    if current != AdrStatus::Provisional {
        return Err(AdrError::WrongStatus { current });
    }
    let digest = insert_transition(
        &txn,
        &AdrTransition {
            schema: ADR_TRANSITION_SCHEMA,
            adr,
            status: AdrStatus::Accepted,
            citations: citations.to_vec(),
            successor: None,
        },
        Some(statement_bytes.as_slice()),
        Some(&signature),
    )?;
    txn.commit()?;
    Ok(digest)
}

fn supersede(conn: &mut Connection, adr: Digest, successor: Digest) -> Result<Digest, AdrError> {
    if successor == adr {
        return Err(AdrError::BadSuccessor);
    }
    let txn = conn.transaction()?;
    let current = current_status(&txn, adr)?.ok_or(AdrError::Missing)?;
    if current != AdrStatus::Provisional {
        return Err(AdrError::WrongStatus { current });
    }
    if !adr_exists(&txn, successor)? {
        return Err(AdrError::BadSuccessor);
    }
    let digest = insert_transition(
        &txn,
        &AdrTransition {
            schema: ADR_TRANSITION_SCHEMA,
            adr,
            status: AdrStatus::Superseded,
            citations: Vec::new(),
            successor: Some(successor),
        },
        None,
        None,
    )?;
    txn.commit()?;
    Ok(digest)
}

fn insert_transition(
    txn: &Transaction<'_>,
    transition: &AdrTransition,
    statement: Option<&[u8]>,
    signature: Option<&[u8]>,
) -> Result<Digest, AdrError> {
    let canonical = transition.to_canonical();
    let decoded = AdrTransition::from_canonical(&canonical)?;
    if decoded != *transition {
        return Err(AdrError::MalformedCanonical);
    }
    let digest = digest_of(&decoded);
    let successor = decoded.successor.map(|item| item.as_bytes().to_vec());
    txn.execute(
        "INSERT INTO adr_transitions (digest, adr, status, canonical, statement, signature, successor)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        rusqlite::params![
            digest.as_bytes().as_slice(),
            decoded.adr.as_bytes().as_slice(),
            decoded.status.as_str(),
            canonical,
            statement,
            signature,
            successor,
        ],
    )?;
    Ok(digest)
}

fn load_view(conn: &Connection, digest: Digest) -> Result<Option<AdrView>, AdrError> {
    let Some(adr) = load_adr(conn, digest)? else {
        return Ok(None);
    };
    Ok(Some(view_of(conn, digest, adr)?))
}

fn load_by_number(conn: &Connection, number: u32) -> Result<Option<AdrView>, AdrError> {
    let Some(digest) = load_digest_for_number(conn, number)? else {
        return Ok(None);
    };
    load_view(conn, digest)
}

fn list_adrs(conn: &Connection) -> Result<Vec<AdrView>, AdrError> {
    let mut stmt = conn.prepare("SELECT digest FROM adrs ORDER BY number")?;
    let rows = stmt.query_map([], |row| row.get::<_, Vec<u8>>(0))?;
    let mut views = Vec::new();
    for row in rows {
        let bytes = row?;
        let digest = Digest::from_slice(&bytes).ok_or(AdrError::MalformedCanonical)?;
        views.push(load_view(conn, digest)?.ok_or(AdrError::MalformedCanonical)?);
    }
    Ok(views)
}

fn view_of(conn: &Connection, digest: Digest, adr: Adr) -> Result<AdrView, AdrError> {
    let latest = latest_transition(conn, digest)?.ok_or(AdrError::MalformedCanonical)?;
    Ok(AdrView {
        digest,
        adr,
        status: latest.status,
        citations: if latest.status == AdrStatus::Accepted {
            latest.citations
        } else {
            Vec::new()
        },
        successor: latest.successor,
    })
}

fn load_adr(conn: &Connection, digest: Digest) -> Result<Option<Adr>, AdrError> {
    let row = conn
        .query_row(
            "SELECT digest, number, title, canonical FROM adrs WHERE digest = ?1",
            [digest.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Vec<u8>>(3)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_digest, number, title, canonical)) = row else {
        return Ok(None);
    };
    let decoded = decode_adr(&canonical)?;
    if digest_of(&decoded) != digest
        || Digest::from_slice(&stored_digest) != Some(digest)
        || i64::from(decoded.number) != number
        || decoded.title != title
    {
        return Err(AdrError::MalformedCanonical);
    }
    Ok(Some(decoded))
}

fn latest_transition(conn: &Connection, adr: Digest) -> Result<Option<AdrTransition>, AdrError> {
    let row = conn
        .query_row(
            "SELECT canonical FROM adr_transitions WHERE adr = ?1 ORDER BY rowid DESC LIMIT 1",
            [adr.as_bytes().as_slice()],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    match row {
        Some(canonical) => Ok(Some(decode_transition(&canonical)?)),
        None => Ok(None),
    }
}

fn current_status(conn: &Connection, adr: Digest) -> Result<Option<AdrStatus>, AdrError> {
    Ok(latest_transition(conn, adr)?.map(|transition| transition.status))
}

fn load_digest_for_number(conn: &Connection, number: u32) -> Result<Option<Digest>, AdrError> {
    let bytes: Option<Vec<u8>> =
        conn.query_row("SELECT digest FROM adrs WHERE number = ?1", [number], |row| row.get(0)).optional()?;
    match bytes {
        Some(bytes) => Ok(Some(Digest::from_slice(&bytes).ok_or(AdrError::MalformedCanonical)?)),
        None => Ok(None),
    }
}

fn adr_exists(conn: &Connection, digest: Digest) -> Result<bool, AdrError> {
    let found: Option<i64> = conn
        .query_row("SELECT 1 FROM adrs WHERE digest = ?1", [digest.as_bytes().as_slice()], |row| row.get(0))
        .optional()?;
    Ok(found.is_some())
}

fn decode_adr(bytes: &[u8]) -> Result<Adr, AdrError> {
    Ok(Adr::from_canonical(bytes)?)
}

fn decode_transition(bytes: &[u8]) -> Result<AdrTransition, AdrError> {
    Ok(AdrTransition::from_canonical(bytes)?)
}
