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
//! The host reads the file out of the sealed base's tree and seals the decoded
//! value into the bloom's registry; the seal door resolves it through
//! [`PipelineManifest::sealed_in`] and journals it onto the record, so the fold
//! reads the vocabulary the bloom sealed rather than the one its binary
//! happens to compile. A fresh seal that names no manifest is refused; the
//! compiled fallback here is only for records already journaled.

use alloc::collections::BTreeMap;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::error::Error;
use core::fmt;

use serde::{Deserialize, Serialize};

use super::{
    CONSTRUCT_IMPLEMENT_COMMAND, ConfigScopes, RETROSPECT_READ_COMMAND, REVIEW_CRITIC_COMMAND, ResolvedConfigs,
    SCOPE_FILL_COMMAND, VERIFY_BASE_COMMAND, VERIFY_CHECK_COMMAND, VERIFY_MEMBER_COMMAND, VerifyFailure,
    VerifyFailureSet,
};

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

/// The program the compiled lane entrypoint spawns, and the words it passes
/// before the work order's own argv.
///
/// Named here because [`PipelineManifest::compiled`] is the value a bloom
/// sealed before this vocabulary was declared folds against, and the entrypoint
/// is one of the five copies this record collapses: the host's own
/// `DEFAULT_LANE_PROGRAM` is gone, and a dispatch reads `[entrypoint]` from
/// the sealed manifest.
const COMPILED_ENTRYPOINT_PROGRAM: &str = "cargo";
const COMPILED_ENTRYPOINT_ARGS: [&str; 2] = ["xtask", "transform"];

/// The fan-out `verify.check` and `verify.base` run — documentation included.
///
/// Named here rather than read off a [`VerifyGateSet`](super::VerifyGateSet):
/// those constructors project this list, so rendering `compiled` from them
/// would be circular. `verify.containment` is a legal identity no lane runs
/// and is therefore absent; `verify.docs` belongs at the two whole-tree
/// positions because an intra-doc link resolves across crates.
const COMPILED_FOLD_RUNS: [VerifyFailure; 9] = [
    VerifyFailure::Preflight,
    VerifyFailure::Fmt,
    VerifyFailure::Clippy,
    VerifyFailure::Docs,
    VerifyFailure::Test,
    VerifyFailure::Dup,
    VerifyFailure::Deps,
    VerifyFailure::Suppress,
    VerifyFailure::Lock,
];

/// The fan-out `verify.member` runs — [`COMPILED_FOLD_RUNS`] less
/// [`VerifyFailure::Docs`].
const COMPILED_MEMBER_RUNS: [VerifyFailure; 8] = [
    VerifyFailure::Preflight,
    VerifyFailure::Fmt,
    VerifyFailure::Clippy,
    VerifyFailure::Test,
    VerifyFailure::Dup,
    VerifyFailure::Deps,
    VerifyFailure::Suppress,
    VerifyFailure::Lock,
];

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
    /// The vocabulary compiled into this binary — the value this repository's
    /// `pipeline.toml` transcribes, named the way
    /// [`StageCatalog::line`](super::StageCatalog::line) names the compiled
    /// line.
    ///
    /// Its only production reader is the fallback at record construction: a
    /// bloom sealed before ADR-0215 named no manifest, so the fold has to give
    /// its record *something*, and the honest something is the vocabulary that
    /// bloom actually ran under. Rendered from the compiled constants rather
    /// than from the three [`VerifyGateSet`](super::VerifyGateSet) positions —
    /// those constructors read `[verifiers.runs]` here, so rendering this from
    /// them would be circular. [`VerifyFailure::ALL`], the per-position run
    /// lists, and the lane command constants are the one source.
    ///
    /// It is deliberately *not* a fallback for a checkout that carries no
    /// `pipeline.toml`: a fresh seal that names no manifest is refused rather
    /// than silently borrowing this one, because a fallback is silent at
    /// exactly the moment the tree and the coordinator disagree most. This
    /// constructor remains the fold's answer for records already journaled.
    #[must_use]
    pub fn compiled() -> Self {
        Self {
            version: PIPELINE_MANIFEST_VERSION,
            entrypoint: LaneEntrypoint {
                program: String::from(COMPILED_ENTRYPOINT_PROGRAM),
                args: COMPILED_ENTRYPOINT_ARGS.iter().copied().map(String::from).collect(),
            },
            lanes: DeclaredLanes {
                model: commands(&[
                    CONSTRUCT_IMPLEMENT_COMMAND,
                    REVIEW_CRITIC_COMMAND,
                    SCOPE_FILL_COMMAND,
                    RETROSPECT_READ_COMMAND,
                ]),
                mechanical: commands(&[VERIFY_MEMBER_COMMAND, VERIFY_CHECK_COMMAND, VERIFY_BASE_COMMAND]),
            },
            verifiers: DeclaredVerifiers {
                identities: identities(VerifyFailure::ALL.into_iter()),
                runs: [
                    (VERIFY_MEMBER_COMMAND, COMPILED_MEMBER_RUNS.as_slice()),
                    (VERIFY_CHECK_COMMAND, COMPILED_FOLD_RUNS.as_slice()),
                    (VERIFY_BASE_COMMAND, COMPILED_FOLD_RUNS.as_slice()),
                ]
                .into_iter()
                .map(|(command, runs)| (String::from(command), identities(runs.iter().copied())))
                .collect(),
            },
            evidence: DeclaredEvidence { envelope: EVIDENCE_ENVELOPE_VERSION },
        }
    }

    /// The manifest `scopes` seals, or [`compiled`](Self::compiled) when it
    /// seals none.
    ///
    /// The one place the "which vocabulary does this bloom run" question is
    /// answered, so the seal door and the snapshot fold cannot give different
    /// answers for the same spec — the same shape
    /// [`StageCatalog::sealed_in`](super::StageCatalog::sealed_in) has, for the
    /// same reason. A present unresolved entry is refused before this lookup;
    /// only absence selects the compiled vocabulary.
    #[must_use]
    pub fn sealed_in(scopes: ConfigScopes<'_>, configs: &ResolvedConfigs) -> Self {
        configs.resolve::<Self>(scopes).ok().flatten().unwrap_or_else(Self::compiled)
    }

    /// Whether the repository implements `command` as a lane at all — model or
    /// mechanical.
    ///
    /// The declared half of the split ADR-0215 draws through the compiled
    /// `is_known_process` match: a binding's host position names the
    /// coordinator's own code and stays compiled, while the lane it dispatches
    /// is the repository's and is answered here.
    /// [`StageCatalog::validate_against`](super::StageCatalog::validate_against)
    /// is its one production caller.
    #[must_use]
    pub fn declares_lane(&self, command: &str) -> bool {
        self.lane_commands().any(|declared| declared == command)
    }

    /// Whether the repository declares `command` a **model lane** — the
    /// declared answer to [`is_model_lane`](super::is_model_lane)'s question.
    ///
    /// It decides which dispatches carry a credential and a resolved model, so
    /// it has to come from the value the bloom sealed: a mechanical lane that
    /// could acquire model-lane treatment through host configuration is exactly
    /// what the compiled disjunction was written to prevent, and reading the
    /// split off a sealed manifest preserves it rather than loosening it.
    #[must_use]
    pub fn is_model_lane(&self, command: &str) -> bool {
        self.lanes.model.iter().any(|declared| declared == command)
    }

    /// How many verifier identities this vocabulary declares — the `N` of
    /// ADR-0178's `N + B` forgiveness bound, read off the value a bloom sealed
    /// rather than off whatever a binary happens to compile.
    #[must_use]
    pub fn identity_count(&self) -> usize {
        self.verifiers.identities.len()
    }

    /// Every verifier identity this vocabulary declares, in declaration order —
    /// which is bit order, and what a refusal names back.
    pub fn identities(&self) -> impl Iterator<Item = &str> {
        self.verifiers.identities.iter().map(String::as_str)
    }

    /// The identity `name` interned against this vocabulary: its declared
    /// position, carried on the value.
    ///
    /// `None` when this manifest does not declare `name` at all, and equally
    /// when it declares it at a position this coordinator cannot represent — a
    /// compiled identity moved off its own bit, or a new name below the compiled
    /// vocabulary's width. Both are answered the same way because both mean the
    /// same thing to an admission door: the vocabulary in the row is not the
    /// vocabulary the bloom sealed, and a mask interned against it would name
    /// gates that never ran.
    #[must_use]
    pub fn intern(&self, name: &str) -> Option<VerifyFailure> {
        let position = u8::try_from(self.identities().position(|declared| declared == name)?).ok()?;
        VerifyFailure::from_name(name)
            .filter(|compiled| compiled.position() == position)
            .or_else(|| VerifyFailure::declared(position, name))
    }

    /// Whether this vocabulary declares `failure` — the same identity at the
    /// same position.
    #[must_use]
    pub fn declares_verifier(&self, failure: VerifyFailure) -> bool {
        self.intern(failure.as_str()) == Some(failure)
    }

    /// Every position in `failures` this vocabulary does not declare, named as
    /// best this reader can, in the set's own canonical order.
    ///
    /// The refusal's evidence: a verdict naming one of these is refused at
    /// admission, where the bloom — and so the vocabulary it sealed — is in hand
    /// (ADR-0215). Judged on *positions* rather than on names, because a mask is
    /// a mask over positions and a reader without the declaring vocabulary
    /// cannot spell a position past the one it compiles. A position at or past
    /// the declared width is undeclared; so is one whose declared identity is
    /// not the identity this binary compiles at that position, which is a
    /// vocabulary that moved a compiled identity off its own bit and would
    /// re-key every mask already journaled.
    #[must_use]
    pub fn undeclared_verifiers(&self, failures: VerifyFailureSet) -> Vec<String> {
        failures.positions().filter(|position| !self.declares_position(*position)).map(name_of).collect()
    }

    /// Whether this vocabulary declares the identity at `position`.
    fn declares_position(&self, position: u8) -> bool {
        let Some(declared) = self.verifiers.identities.get(usize::from(position)) else {
            return false;
        };
        VerifyFailure::ALL.get(usize::from(position)).is_none_or(|compiled| compiled.as_str() == declared.as_str())
    }

    /// Every lane command the repository declares, model lanes first.
    ///
    /// What a refusal names back: an operator told their catalog names a lane
    /// this base does not carry can see the alternatives without going to read
    /// the file out of a tree.
    #[must_use]
    pub fn declared_lanes(&self) -> Vec<String> {
        self.lane_commands().map(String::from).collect()
    }

    /// Both halves of the declared split, in declaration order.
    fn lane_commands(&self) -> impl Iterator<Item = &str> {
        self.lanes.model.iter().chain(&self.lanes.mechanical).map(String::as_str)
    }

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

/// How a refusal spells one verifier position: the compiled identity's name
/// when this binary has one, and the bare position when it does not — the
/// bloom's recorded manifest is what names the rest.
fn name_of(position: u8) -> String {
    VerifyFailure::ALL
        .get(usize::from(position))
        .map_or_else(|| format!("verifier position {position}"), |identity| String::from(identity.as_str()))
}

/// The declared spelling of each compiled lane command, in the order given.
fn commands(compiled: &[&str]) -> Vec<String> {
    compiled.iter().copied().map(String::from).collect()
}

/// The declared spelling of each compiled verifier identity, in canonical
/// order — the order both [`VerifyFailure::ALL`] and a
/// [`crate::VerifyGateSet`]'s set iterate in.
fn identities(compiled: impl Iterator<Item = VerifyFailure>) -> Vec<String> {
    compiled.map(|identity| String::from(identity.as_str())).collect()
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
    use std::fs;
    use std::path::Path;

    use super::{PipelineManifest, PipelineManifestError};
    use crate::values::{VerifyFailure, VerifyFailureSet, is_model_lane};

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
        // `compiled` renders the compiled halves rather than restating them, so
        // this is one comparison and not a per-axis list to keep in step.
        assert_eq!(manifest, PipelineManifest::compiled());

        // The declared split has to agree with the compiled disjunction that
        // decides which dispatch carries a credential, since that disjunction is
        // what the manifest is about to replace.
        assert!(manifest.lanes.model.iter().all(|command| is_model_lane(command)));
        assert!(!manifest.lanes.mechanical.iter().any(|command| is_model_lane(command)));
    }

    #[test]
    fn the_compiled_entrypoint_is_the_argv_the_deleted_default_spawned() {
        // Tripwire: `DEFAULT_LANE_PROGRAM` used to be `"cargo xtask transform"`.
        // Pre-manifest blooms fold against `compiled()`, so that entrypoint must
        // stay the invocation the deleted default named. Changing it rewrites
        // the program those records dispatched.
        assert_eq!(super::COMPILED_ENTRYPOINT_PROGRAM, "cargo");
        assert_eq!(super::COMPILED_ENTRYPOINT_ARGS, ["xtask", "transform"]);
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
    fn an_identity_is_interned_against_the_declared_vocabulary() {
        // The whole of ADR-0215's move for verifier identity: membership stops
        // being a compiled table lookup and becomes a position in the vocabulary
        // the bloom sealed. A `declares_verifier` that reached for
        // `VerifyFailure::from_name` would pass every assertion this repository
        // can make about its own manifest, because the two agree today — so the
        // vocabularies below deliberately do not.
        let mut nine = PipelineManifest::compiled();
        nine.verifiers.identities.retain(|identity| identity != VerifyFailure::Lock.as_str());

        assert_eq!(nine.identity_count(), 9);
        assert!(nine.declares_verifier(VerifyFailure::Clippy));
        assert!(!nine.declares_verifier(VerifyFailure::Lock), "an identity the base dropped is not declared");
        assert_eq!(
            nine.undeclared_verifiers([VerifyFailure::Clippy, VerifyFailure::Lock].into_iter().collect()),
            [VerifyFailure::Lock.as_str()],
        );

        // A vocabulary that appends past the compiled one interns the new
        // identity at the position it declared, which is its bit.
        let mut eleven = PipelineManifest::compiled();
        eleven.verifiers.identities.push(String::from("verify.novel"));
        let novel = eleven.intern("verify.novel").expect("an appended identity interns");
        assert_eq!(novel.position(), 10);
        assert!(eleven.declares_verifier(novel));
        assert!(eleven.undeclared_verifiers(VerifyFailureSet::one(novel)).is_empty());

        // A vocabulary that moves a compiled identity off its own bit is
        // refused rather than re-interned: every stored mask was written
        // against the position it is being moved away from.
        let mut reordered = PipelineManifest::compiled();
        reordered.verifiers.identities.swap(0, 1);
        assert!(!reordered.declares_verifier(VerifyFailure::Fmt));
        assert_eq!(reordered.intern("verify.fmt"), None);
    }

    #[test]
    fn a_vocabulary_past_the_bound_is_refused() {
        let identities = (0..17).map(|index| format!("\"verify.v{index}\"")).collect::<Vec<_>>().join(", ");
        assert_eq!(
            PipelineManifest::from_toml(&manifest_text(1, 1, &identities)),
            Err(PipelineManifestError::TooManyIdentities { declared: 17 })
        );
    }
}
