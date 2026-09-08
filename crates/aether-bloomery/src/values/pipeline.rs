//! The pipeline manifest: the lane vocabulary a checked-out repository declares
//! for itself (ADR-0215).
//!
//! The vocabulary the coordinator dispatches against is a property of the tree
//! being worked, not of the binary doing the dispatching — which is why holding
//! it compiled produced five copies of one list and three of them drifted. So
//! the repository states it once, in [`PIPELINE_MANIFEST_PATH`] at its root, and
//! the coordinator reads that statement out of the sealed base's tree.
//!
//! What is declared here is *vocabulary*: the lane entrypoint, which typed lane
//! commands exist and which of them run a model, the verifier identities and
//! what each verify position's fan-out runs, and the evidence-envelope version.
//! Semantics and confinement stay with the coordinator — a manifest carrying an
//! execution image or a network posture is refused by
//! [`deny_unknown_fields`](https://serde.rs/container-attrs.html), not ignored,
//! because a tree that named its own confinement would grant itself egress on
//! the coordinator's host.
//!
//! # Why the TOML reader lives beside the value
//!
//! [`ApprovalPolicy`](super::ApprovalPolicy) is the precedent for a checked-in
//! file the coordinator reads, and it splits the other way: the value lives here
//! and the host keeps the parse, because the host also keeps the *path* the text
//! comes off. This manifest has no host path — the text is fetched from the
//! sealed base's tree through the source port, so the only thing to place is
//! text-to-value, and that half is inseparable from the version refusal below.
//!
//! Nothing reads this yet. Resolving the manifest from the base, recording it on
//! the bloom, and enforcing it at the seal door are the slices that follow.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use serde::{Deserialize, Serialize};

/// Where a repository states its lanes: the root of the checkout, beside
/// `approval-policy.toml`.
///
/// Named once because the refusals below quote it and the host that fetches the
/// text out of a tree addresses it.
pub const PIPELINE_MANIFEST_PATH: &str = "pipeline.toml";

/// The lane entrypoint: the argv a dispatch spawns before the work order's own
/// arguments.
///
/// A word list rather than a shell string, spawned with the checkout as its
/// working directory. The coordinator appends the transform's argv after
/// [`args`](Self::args), so the child sees the declared words first and the
/// coordinator's second.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaneEntrypoint {
    /// The program to spawn.
    pub program: String,
    /// The arguments that precede the work order's own argv.
    pub args: Vec<String>,
}

/// The typed lane commands a repository implements, split by whether the lane
/// runs a model.
///
/// The split is the manifest's, not a host overlay's, because it decides which
/// dispatches carry a credential and a resolved model: a mechanical lane must
/// not be able to acquire model-lane treatment through host configuration.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredLanes {
    /// Lane commands whose worker runs a model.
    pub model: Vec<String>,
    /// Lane commands whose worker runs a compiler and nothing else.
    pub mechanical: Vec<String>,
}

/// The verifier vocabulary and what each verify position actually runs.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredVerifiers {
    /// Every verifier identity, in canonical order — a position is an
    /// identity's bit in a recorded failure set, so the order is append-only.
    pub identities: Vec<String>,
    /// The fan-out each verify lane command runs, keyed by that command.
    ///
    /// A subset of [`identities`](Self::identities): an identity the
    /// coordinator judges for itself is declared without appearing in any
    /// position's list, which is how "a legal identity no lane runs" is stated
    /// as data instead of remembered.
    pub runs: BTreeMap<String, Vec<String>>,
}

/// The evidence-envelope shape a lane writes and the intake admits.
///
/// A version the coordinator refuses when it does not implement it, never a
/// description it adapts to: the envelope's status vocabulary, nonce, subject
/// binding, failed-verifier list, and channel keys are the coordinator's.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeclaredEvidence {
    /// The envelope version the repository's lanes write.
    pub envelope: u32,
}

/// What a repository declares about the lanes it can run (ADR-0215).
///
/// Sealed into a bloom's [`ConfigRegistry`](super::ConfigRegistry) by the host
/// that read it, rather than authored by an operator: only a host holding both
/// the manifest and the tree can check that the vocabulary being attested is the
/// one the checkout carries.
///
/// The sealed bytes are the decoded value, not the file text, so reformatting
/// the file, reordering its tables, or editing a comment re-seals nothing. Only
/// a change in meaning moves the digest.
#[aether_data::kind(name = "aether.bloomery.pipeline_manifest", eq)]
#[serde(deny_unknown_fields)]
pub struct PipelineManifest {
    /// The manifest format version, refused by number when unimplemented.
    pub version: u32,
    /// The argv a lane dispatch spawns.
    pub entrypoint: LaneEntrypoint,
    /// The lane commands this repository implements.
    pub lanes: DeclaredLanes,
    /// The verifier vocabulary and per-position fan-out.
    pub verifiers: DeclaredVerifiers,
    /// The evidence envelope this repository's lanes write.
    pub evidence: DeclaredEvidence,
}

/// The manifest version this binary reads.
///
/// An integer rather than a schema digest because the file is an *input* a
/// human edits in a pull request: a number the reader names back is legible at
/// the point of the mistake, and a digest is not.
pub const PIPELINE_MANIFEST_VERSION: u32 = 1;

/// The evidence-envelope version this binary implements.
pub const EVIDENCE_ENVELOPE_VERSION: u32 = 1;

/// The most verifier identities a manifest may declare.
///
/// Sixteen keeps a failure set a `u16` and its artifact token four hex digits,
/// and keeps the forgiveness bound a small number. A repository wanting a
/// seventeenth identity is a further decision, exactly as the ninth was.
pub const MAX_VERIFIER_IDENTITIES: usize = 16;

/// The version probe: what a reader must decode before it can honestly refuse.
///
/// Deliberately tolerant where [`PipelineManifest`] is strict. A manifest from a
/// future version carries tables this reader has never heard of, and the useful
/// answer to one is its version number — not the first unknown key that a strict
/// decode happens to trip over.
#[derive(Deserialize)]
struct VersionProbe {
    version: u32,
}

impl PipelineManifest {
    /// Read a manifest from its TOML text.
    ///
    /// Refuses, in this order: text that does not carry a version at all, a
    /// version this binary does not implement, text that is not a manifest of
    /// that version (an unknown table or key included), a vocabulary past
    /// [`MAX_VERIFIER_IDENTITIES`], and an evidence envelope this binary does
    /// not implement.
    pub fn from_toml(text: &str) -> Result<Self, PipelineManifestError> {
        let probe: VersionProbe = toml::from_str(text).map_err(PipelineManifestError::malformed)?;
        if probe.version != PIPELINE_MANIFEST_VERSION {
            return Err(PipelineManifestError::UnsupportedVersion { declared: probe.version });
        }

        let manifest: Self = toml::from_str(text).map_err(PipelineManifestError::malformed)?;
        let declared = manifest.verifiers.identities.len();
        if declared > MAX_VERIFIER_IDENTITIES {
            return Err(PipelineManifestError::TooManyIdentities { declared });
        }
        if manifest.evidence.envelope != EVIDENCE_ENVELOPE_VERSION {
            return Err(PipelineManifestError::UnsupportedEnvelope { declared: manifest.evidence.envelope });
        }
        Ok(manifest)
    }
}

/// Why a `pipeline.toml` cannot become a usable [`PipelineManifest`].
///
/// Every case is a refusal, never a fall back to a compiled vocabulary: a
/// fallback would be silent at exactly the moment the tree and the coordinator
/// disagree most.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum PipelineManifestError {
    /// The text is not a well-formed manifest of the supported version — a
    /// syntax error, a missing table, or a table or key this version does not
    /// declare.
    Malformed(String),
    /// The file declares a manifest version this binary does not implement.
    UnsupportedVersion {
        /// The version the file declared.
        declared: u32,
    },
    /// The file declares an evidence-envelope version this binary does not
    /// implement.
    UnsupportedEnvelope {
        /// The envelope version the file declared.
        declared: u32,
    },
    /// The file declares more verifier identities than the vocabulary bound.
    TooManyIdentities {
        /// How many identities the file declared.
        declared: usize,
    },
}

impl PipelineManifestError {
    fn malformed(error: impl fmt::Display) -> Self {
        Self::Malformed(error.to_string())
    }
}

impl fmt::Display for PipelineManifestError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed(detail) => write!(f, "`{PIPELINE_MANIFEST_PATH}` is not a well-formed manifest: {detail}"),
            Self::UnsupportedVersion { declared } => write!(
                f,
                "`{PIPELINE_MANIFEST_PATH}` declares version {declared}; this coordinator reads version \
                 {PIPELINE_MANIFEST_VERSION}"
            ),
            Self::UnsupportedEnvelope { declared } => write!(
                f,
                "`{PIPELINE_MANIFEST_PATH}` declares evidence envelope {declared}; this coordinator implements \
                 envelope {EVIDENCE_ENVELOPE_VERSION}"
            ),
            Self::TooManyIdentities { declared } => write!(
                f,
                "`{PIPELINE_MANIFEST_PATH}` declares {declared} verifier identities; at most \
                 {MAX_VERIFIER_IDENTITIES} may be declared"
            ),
        }
    }
}

impl Error for PipelineManifestError {}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::Path;

    use super::{PipelineManifest, PipelineManifestError};
    use crate::values::{
        CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, SCOPE_FILL_COMMAND, VERIFY_BASE_COMMAND,
        VERIFY_CHECK_COMMAND, VERIFY_MEMBER_COMMAND, VerifyFailure, VerifyGateSet, is_model_lane,
    };

    fn checked_in_manifest() -> PipelineManifest {
        let text = fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../pipeline.toml"))
            .expect("this repository's own pipeline manifest is checked in at the root");
        PipelineManifest::from_toml(&text).expect("the checked-in manifest reads")
    }

    fn manifest_text(version: u32, envelope: u32, identities: &str) -> String {
        format!(
            "version = {version}\n\
             [entrypoint]\nprogram = \"cargo\"\nargs = [\"xtask\", \"transform\"]\n\
             [lanes]\nmodel = []\nmechanical = []\n\
             [verifiers]\nidentities = [{identities}]\n[verifiers.runs]\n\
             [evidence]\nenvelope = {envelope}\n"
        )
    }

    #[test]
    fn the_checked_in_manifest_transcribes_the_compiled_vocabulary() {
        let manifest = checked_in_manifest();

        // Tripwire: the manifest and the compiled vocabulary are two statements
        // of one thing until the later ADR-0215 slices delete the compiled half,
        // and two copies of a list is the shape that has already drifted three
        // times. An identity appended to `VerifyFailure::ALL`, a lane command
        // respelled, or a gate set's fan-out changed without the matching edit
        // to `pipeline.toml` fails here rather than at a seal door months later.
        assert_eq!(manifest.verifiers.identities, names(VerifyFailure::ALL.into_iter()));
        assert_eq!(manifest.lanes.model, [CONSTRUCT_IMPLEMENT_COMMAND, REVIEW_CRITIC_COMMAND, SCOPE_FILL_COMMAND]);
        assert_eq!(manifest.lanes.mechanical, [VERIFY_MEMBER_COMMAND, VERIFY_CHECK_COMMAND, VERIFY_BASE_COMMAND]);
        let runs: BTreeMap<String, Vec<String>> =
            [VerifyGateSet::member(), VerifyGateSet::fold(), VerifyGateSet::base()]
                .into_iter()
                .map(|gates| (gates.command, names(gates.verifiers.iter())))
                .collect();
        assert_eq!(manifest.verifiers.runs, runs);

        // The declared split has to agree with the compiled disjunction that
        // decides which dispatch carries a credential, since that disjunction is
        // what the manifest is about to replace.
        assert!(manifest.lanes.model.iter().all(|command| is_model_lane(command)));
        assert!(!manifest.lanes.mechanical.iter().any(|command| is_model_lane(command)));
    }

    #[test]
    fn a_version_this_reader_does_not_implement_is_refused_by_number() {
        // A manifest from a later version carries tables this reader has never
        // seen, and naming the version is the only answer that tells its editor
        // what happened; the strict decode's first unknown key does not.
        assert_eq!(
            PipelineManifest::from_toml("version = 2\n[lanes.future]\nshape = \"unknown\"\n"),
            Err(PipelineManifestError::UnsupportedVersion { declared: 2 })
        );
        assert_eq!(
            PipelineManifest::from_toml(&manifest_text(1, 2, "\"verify.fmt\"")),
            Err(PipelineManifestError::UnsupportedEnvelope { declared: 2 })
        );
    }

    #[test]
    fn an_unknown_table_or_key_is_refused() {
        // Confinement is the coordinator's: a manifest naming the image or the
        // network posture a lane runs under is refused, not ignored, because
        // ignoring it reads to its author as having been honoured.
        let base = manifest_text(1, 1, "\"verify.fmt\"");
        assert!(PipelineManifest::from_toml(&base).is_ok(), "the fixture itself must read");
        for surprise in ["[network]\negress = true\n", "[image]\nname = \"iama/verify:1\"\n", "image = 1\n"] {
            let text = base.clone() + surprise;
            assert!(
                matches!(PipelineManifest::from_toml(&text), Err(PipelineManifestError::Malformed(_))),
                "`{surprise}` must be refused"
            );
        }
    }

    #[test]
    fn a_vocabulary_past_the_bound_is_refused() {
        let identities = (0..17).map(|index| format!("\"verify.v{index}\"")).collect::<Vec<_>>().join(", ");
        assert_eq!(
            PipelineManifest::from_toml(&manifest_text(1, 1, &identities)),
            Err(PipelineManifestError::TooManyIdentities { declared: 17 })
        );
    }

    fn names(identities: impl Iterator<Item = VerifyFailure>) -> Vec<String> {
        identities.map(|identity| identity.as_str().to_owned()).collect()
    }
}
