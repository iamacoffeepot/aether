//! Parse one snapshot body into a migration-manifest row.

use aether_bloomery::{Digest, Observation, Provenance, ScopeRevision, Statement, WorkpieceId, digest_of};

use super::{IssueSnapshot, ParseStatus};
use crate::commission::scope::parse_revision;

/// Intent, manifest row, and the revision when the body parsed cleanly.
pub struct ParsedIssue {
    /// Observation-attested intent. Never an author signature.
    pub intent: Statement,
    /// Source facts recorded for this issue.
    pub entry: ManifestEntry,
    /// The parsed revision, or `None` when the body is ambiguous.
    pub revision: Option<ScopeRevision>,
}

/// One issue's recorded source facts. Trust here is observation, never a signature.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Source GitHub issue number.
    pub issue: u64,
    /// Workpiece the import will create.
    pub workpiece: WorkpieceId,
    /// GitHub issue title at snapshot time. Verification only.
    pub title: String,
    /// sha256 of the exact snapshot body.
    pub body_digest: Digest,
    /// Content address of the parsed revision, when the body parsed cleanly.
    pub plan_digest: Option<Digest>,
    /// `base_sha` from the last well-formed hidden approval record, if any.
    pub base_commit: Option<String>,
    /// How GitHub provenance was classified. Never `AuthorSignature`.
    pub trust: Trust,
    /// Whether managed headings became a revision.
    pub parse: ParseStatus,
}

/// GitHub-era provenance recorded as observation metadata.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Trust {
    /// No hidden approval record was present.
    None,
    /// A well-formed marker was observed. It cannot authorize.
    GithubObservation {
        /// The marker's `plan_sha256` field, when present.
        plan_sha256: Option<String>,
    },
    /// A marker was present but not a well-formed v2 record.
    Unparseable,
}

/// Build the intent statement and manifest row for one named snapshot.
pub fn parse_issue(snapshot: &IssueSnapshot) -> ParsedIssue {
    let body_digest = Digest::of_wire_bytes(snapshot.body.as_bytes());
    let trust = classify_trust(&snapshot.body);
    let base_commit = github_base_commit(&snapshot.body);
    let (parse, plan_digest, revision) = match parse_revision(&snapshot.workpiece.0, &snapshot.body, None) {
        Ok(revision) => {
            let scope = digest_of(&revision);
            (ParseStatus::Clean { scope }, Some(scope), Some(revision))
        }
        Err(error) => (ParseStatus::Ambiguous { reason: error.to_string() }, None, None),
    };
    let intent = Statement {
        words: snapshot.body.as_bytes().to_vec(),
        provenance: Provenance::ObservationAttestation(Observation {
            source: format!("migration:github#{}", snapshot.number),
        }),
        parents: Vec::new(),
    };
    ParsedIssue {
        intent,
        entry: ManifestEntry {
            issue: snapshot.number,
            workpiece: snapshot.workpiece.clone(),
            title: snapshot.title.clone(),
            body_digest,
            plan_digest,
            base_commit,
            trust,
            parse,
        },
        revision,
    }
}

fn classify_trust(body: &str) -> Trust {
    let mut last = Trust::None;
    for line in prefix_lines(body) {
        if !line.contains("aether-approval:v2") {
            continue;
        }
        match approval_payload(line) {
            Some(_) => last = Trust::GithubObservation { plan_sha256: approval_field(line, "plan_sha256") },
            None => return Trust::Unparseable,
        }
    }
    last
}

fn github_base_commit(body: &str) -> Option<String> {
    prefix_lines(body).rev().find_map(|line| approval_field(line, "base_sha"))
}

fn prefix_lines(body: &str) -> impl DoubleEndedIterator<Item = &str> {
    let end = if body.starts_with("## ") {
        0
    } else {
        body.find("\n## ").unwrap_or(body.len())
    };
    body[..end].lines()
}

fn approval_payload(line: &str) -> Option<&str> {
    line.trim().strip_prefix("<!-- aether-approval:v2 ")?.strip_suffix(" -->")
}

fn approval_field(line: &str, field: &str) -> Option<String> {
    let payload = approval_payload(line)?;
    let value: serde_json::Value = serde_json::from_str(payload).ok()?;
    value.get(field)?.as_str().map(str::to_owned)
}
