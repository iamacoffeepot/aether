//! Offline import of GitHub-era planned issues into signed commissions.
//!
//! The command writes commissions and immutable scope revisions from an
//! explicit snapshot. GitHub provenance is observation metadata only:
//! `ObservationAttestation` cannot authorize, and this path never inserts an
//! approval from a hidden `aether-approval` marker. The operator signs the
//! imported digest through `AuthorityDoor::Approve` after review. The report
//! leads with imported count and each snapshot title so the operator can
//! verify the one-shot backfill against the open set.
//!
//! Sealed blooms are reconstructed as exact store rows for their pinned
//! [`Membership::scope_revision`](aether_bloomery::Membership::scope_revision)
//! and approval evidence. The [`BloomSpec`] is an input checked against, never
//! a value this module writes.

mod apply;
mod manifest;

use std::error::Error;
use std::fmt::{self, Display, Formatter};
use std::fs;
use std::path::Path;

use aether_bloomery::{BloomSpec, Digest, ScopeRevision, Statement, WorkpieceId};
use anyhow::{Context, Result, anyhow, bail};
use serde::Deserialize;

use crate::store::{CommissionError, SqliteStore};

pub use apply::import;
pub use manifest::{ManifestEntry, Trust};

/// One issue the operator named. The importer never enumerates a snapshot
/// directory on its own.
#[derive(Clone, Debug)]
pub struct IssueSnapshot {
    /// GitHub issue number the body was taken from.
    pub number: u64,
    /// Workpiece the imported commission is.
    pub workpiece: WorkpieceId,
    /// GitHub issue title at snapshot time. Recorded so the operator can
    /// verify count-and-title match against the open set; never an input to
    /// seal or dispatch.
    pub title: String,
    /// Exact issue-body bytes at snapshot time.
    pub body: String,
}

/// Exact store rows for one member of a surviving sealed bloom.
///
/// `revision` and `approval` must already hash to the membership's pinned
/// digests. The spec is carried so the importer can check that match; it is
/// never mutated.
#[derive(Clone, Debug, Deserialize)]
pub struct SealedWorkpiece {
    /// The sealed spec whose membership this reconstruction must match.
    pub spec: BloomSpec,
    /// Canonical revision whose digest equals the pinned `scope_revision`.
    pub revision: ScopeRevision,
    /// The approval statement the sealed evidence `detail` already names.
    pub approval: Statement,
}

/// The explicit set to import. Absence from this set is absence from the run.
#[derive(Clone, Debug, Default)]
pub struct ImportRequest {
    /// Open planned issues, named one by one.
    pub issues: Vec<IssueSnapshot>,
    /// Unlanded sealed members to reconstruct rather than re-parse.
    pub sealed: Vec<SealedWorkpiece>,
}

/// Outcome of one named workpiece.
#[derive(Clone, Debug)]
pub struct ImportReport {
    /// Per-issue manifest rows, in request order.
    pub entries: Vec<ManifestEntry>,
    /// Workpieces that received a commission row.
    pub imported: Vec<WorkpieceId>,
    /// Sealed members whose store rows match the pinned digests.
    pub reconstructed: Vec<WorkpieceId>,
}

/// Why an import was refused before or during the first write.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImportError {
    /// The request named no issues and no sealed members.
    EmptySet,
    /// A sealed reconstruction is not a member of the spec it claims.
    UnknownSealedMember(String),
    /// The supplied revision does not hash to the membership's pinned digest.
    PinnedDigestMismatch {
        /// The workpiece that failed the pin.
        workpiece: String,
    },
    /// The supplied approval does not hash to the membership's evidence detail.
    PinnedEvidenceMismatch {
        /// The workpiece that failed the pin.
        workpiece: String,
    },
    /// An already-stored tip is a different digest than the sealed pin.
    WouldDiverge {
        /// The workpiece whose chain would fork.
        workpiece: String,
    },
    /// A store write failed.
    Store(String),
}

impl Display for ImportError {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySet => {
                write!(f, "import names no workpieces; the explicit set is required")
            }
            Self::UnknownSealedMember(id) => {
                write!(f, "sealed reconstruction {id} is not a member of the supplied bloom")
            }
            Self::PinnedDigestMismatch { workpiece } => {
                write!(f, "sealed reconstruction {workpiece} does not match the pinned scope digest")
            }
            Self::PinnedEvidenceMismatch { workpiece } => {
                write!(f, "sealed reconstruction {workpiece} does not match the pinned approval evidence")
            }
            Self::WouldDiverge { workpiece } => {
                write!(f, "sealed reconstruction {workpiece} would rewrite an existing different revision")
            }
            Self::Store(message) => write!(f, "commission store: {message}"),
        }
    }
}

impl Error for ImportError {}

impl From<CommissionError> for ImportError {
    fn from(error: CommissionError) -> Self {
        Self::Store(error.to_string())
    }
}

/// Load an explicit manifest and write it into `store_path`.
pub fn import_paths(manifest: &Path, store_path: &Path, sealed: Option<&Path>) -> Result<String> {
    let request = load_request(manifest, sealed)?;
    let path = store_path.to_str().ok_or_else(|| anyhow!("store path is not UTF-8"))?;
    let mut store = SqliteStore::open(path).map_err(|error| anyhow!("open {path}: {error}"))?;
    let report = import(&mut store, &request)?;
    Ok(format_report(&report))
}

#[derive(serde::Deserialize)]
struct ManifestFile {
    issues: Vec<ManifestIssue>,
}

#[derive(serde::Deserialize)]
struct ManifestIssue {
    number: u64,
    id: String,
    #[serde(default)]
    title: String,
    body: String,
}

fn load_request(manifest: &Path, sealed: Option<&Path>) -> Result<ImportRequest> {
    let parent = manifest.parent().unwrap_or_else(|| Path::new("."));
    let parsed: ManifestFile =
        serde_json::from_slice(&fs::read(manifest).with_context(|| format!("read {}", manifest.display()))?)
            .with_context(|| format!("parse {}", manifest.display()))?;

    let mut issues = Vec::with_capacity(parsed.issues.len());
    for issue in parsed.issues {
        let path = parent.join(&issue.body);
        let body = fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
        issues.push(IssueSnapshot { number: issue.number, workpiece: WorkpieceId(issue.id), title: issue.title, body });
    }

    let sealed = match sealed {
        Some(path) => serde_json::from_slice(&fs::read(path).with_context(|| format!("read {}", path.display()))?)
            .with_context(|| format!("parse {}", path.display()))?,
        None => Vec::new(),
    };

    if issues.is_empty() && sealed.is_empty() {
        bail!("import names no workpieces; the explicit set is required");
    }
    Ok(ImportRequest { issues, sealed })
}

fn format_report(report: &ImportReport) -> String {
    let mut out = String::new();
    out.push_str("imported ");
    out.push_str(&report.imported.len().to_string());
    out.push('\n');
    for entry in &report.entries {
        out.push_str(&entry.workpiece.0);
        out.push(' ');
        if !entry.title.is_empty() {
            out.push_str(&entry.title);
            out.push(' ');
        }
        match &entry.parse {
            ParseStatus::Clean { scope } => {
                out.push_str("clean unsigned scope ");
                out.push_str(&hex(scope.as_bytes()));
            }
            ParseStatus::Ambiguous { reason } => {
                out.push_str("ambiguous (");
                out.push_str(reason);
                out.push(')');
            }
        }
        if matches!(&entry.trust, Trust::Unparseable) {
            out.push_str(" unparseable-marker");
        }
        out.push('\n');
    }
    for id in &report.reconstructed {
        out.push_str(&id.0);
        out.push_str(" reconstructed\n");
    }
    out
}

/// How a snapshot body parsed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseStatus {
    /// Managed headings became a scope revision. Still unsigned.
    Clean {
        /// Digest of the written revision.
        scope: Digest,
    },
    /// The body is imported as intent only and cannot be approved or sealed.
    Ambiguous {
        /// Why the managed headings did not become a revision.
        reason: String,
    },
}

fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(DIGITS[usize::from(byte >> 4)] as char);
        out.push(DIGITS[usize::from(byte & 0x0f)] as char);
    }
    out
}

#[cfg(test)]
mod tests;
