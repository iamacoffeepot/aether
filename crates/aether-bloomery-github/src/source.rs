//! The git source-port backend (ADR-0149 §The boundary, [#3465]).
//!
//! Implements [`SourceBackend`] over GitHub's Git Data REST API ([`GitDataApi`])
//! — blob/tree/commit/ref reads and writes and the compare-and-swap ref update
//! all over HTTP, **no working copy on disk and no `git` / `git2` / `gix`
//! dependency**. Branch names are working handles, never identity: every value
//! the port returns is digest-addressed ([`SourceSnapshot`] / [`Checkpoint`] /
//! [`IntegrateOutcome`] / [`LandOutcome`]), and no GitHub type crosses into a
//! core `aether_bloomery` module.
//!
//! # Digests map to git objects through persisted correspondence
//!
//! A bloomery [`Digest`] is a 32-byte sha256 content-address of a bloom *value*
//! — never the sha of any git object. Real GitHub is sha1 (20-byte / 40-hex)
//! object format, so this backend cannot hex-pun a digest into an object sha
//! (ADR-0150, amended 2026-07-18 for [#3590]). The mainline paths — `snapshot`,
//! `create_namespace`, `integrate`, and the CAS `land` — resolve real git shas
//! through a persisted [`aether_bloomery::Correspondence`] handle held next to the client:
//! forward ([`aether_bloomery::Correspondence::resolve_backend_object`]) to turn a digest into the git
//! object that carries it, reverse ([`aether_bloomery::Correspondence::resolve_digest`]) to read a
//! real object sha back to its digest. An object the store never recorded is the
//! clean [`SourceError::UnresolvedCorrespondence`] — the honest boundary this
//! slice draws — in place of the old fixed-64-hex `Malformed` that a real sha1
//! repo tripped before any swap. `to_hex` survives only where a digest names a
//! **ref-name segment** (the branch namespace below), never an object sha.
//!
//! The claim-registry paths carry the bloom id on a `Bloom-Id` commit-message
//! line over the well-known empty tree (`parse_bloom_line` /
//! `render_claim_message`) — the empty-tree-plus-message-line encoding
//! ADR-0150 mandates for them, delivered by the sibling claim-encoding slice
//! ([#3590]'s other child) and composed with this correspondence slice here.
//!
//! [#3590]: https://github.com/iamacoffeepot/aether/issues/3590
//!
//! # The branch namespace
//!
//! Per bloom, under `heads/bloom/<hex(bloom)>/`:
//!
//! - `integration` — the single-writer integration branch `integrate` advances.
//! - `attempt/<n>` — a per-attempt working handle.
//! - `checkpoint/<hex(tree)>` — one ref per recorded checkpoint. The **tree
//!   digest is encoded in the ref name**, so `checkpoints` enumerates them by
//!   listing the prefix and reuse across a successor bloom is a digest match —
//!   the queryable, reusable property ADR-0149's successor-reuse clause and
//!   this slice's first-class-checkpoint mandate require, which a same-call
//!   guard value could never satisfy.
//! - `member-checkpoint/<workpiece>` — a dead construct lane's partial
//!   worktree. Sibling of `checkpoint/`, never nested under it:
//!   [`SourceBackend::checkpoints`] strips `checkpoint/` and hard-errors
//!   [`SourceError::Malformed`] on any remainder that is not a hex tree digest,
//!   so a nested `checkpoint/member/<workpiece>` name would break
//!   integration-checkpoint enumeration.
//!
//! [#3465]: https://github.com/iamacoffeepot/aether/issues/3465

use std::error::Error;
use std::fmt;

use aether_bloomery::{
    BackendObjectId, BloomId, Checkpoint, ClaimHolder, ClaimOutcome, ClaimRefKind, ClaimRefState, ClaimReleaseOutcome,
    ContentAddressed, CorrespondenceError, Digest, IntegrateOutcome, IntegrationPosition, LandOutcome, LandProposal,
    LandingReceipt, SharedCorrespondence, Snapshot, SourceBackend, SourceSnapshot, WorkpieceId, digest_of,
};
use serde::Serialize;

use crate::client::{
    ChecksState, GitDataApi, GitRef, GithubApi, GithubError, IssueStateApi, MergeResult, NewComment, NewPullRequest,
    PullMergeResult, PullRequestApi, PullRequestState, strip_heads,
};
use crate::correspondence::GitObjectId;
use crate::mainline::MainlineRef;
use crate::short_hex;

/// The value the integrated-head digest content-addresses (issue #3615): a
/// bloom plus its integrated artifact tree. Its digest is distinct from the
/// artifact `tree`'s own digest by construction — a separate
/// [`ContentAddressed`] domain — so recording `head ↔ commit` in [`integrate`]
/// never collides with the `tree ↔ tree-object` correspondence that `snapshot`
/// and `StaleCheckpoint` reverse-read. The value is never re-derived by any
/// other reader: `integrate` computes it once, records it, and hands it back in
/// [`IntegrateOutcome::Integrated`] to carry through the core to `land`.
///
/// [`integrate`]: GitSource::integrate
#[derive(Serialize)]
struct IntegratedHead {
    bloom: BloomId,
    tree: Digest,
}

impl ContentAddressed for IntegratedHead {
    const DOMAIN: &'static str = "aether.bloomery.github.integrated-head";
}

/// The minted digest of an integration-branch tree no digest names yet — the
/// freshly-bootstrapped branch still at the base commit (ADR-0152). A
/// content-address over the git tree object id, so re-minting the same tree
/// yields the identical digest and the first `integrate`'s expected-compare
/// resolves. Distinct by domain from every other digest over the same bytes.
#[derive(Serialize)]
struct IntegrationTreeAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for IntegrationTreeAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.github.integration-tree";
}

/// The bloom-namespace ref an admitted candidate is pushed to —
/// `refs/heads/bloom/<short bloom hex>/candidate/<workpiece>` (ADR-0152),
/// force-updated because refinement supersedes.
///
/// Public because both ends of that ref live outside this module: the executor
/// pushes an admitted capture to it, and a fold merges it. One spelling, so the
/// two ends cannot drift — a mismatch here does not fail loudly, it addresses a
/// branch that is not there.
#[must_use]
pub fn candidate_ref_name(bloom: &BloomId, workpiece: &str) -> String {
    format!("refs/{}", candidate_ref(bloom, workpiece))
}

/// The same ref in the `heads/…` short form the Git Data surface takes. The
/// workpiece segment is sanitized to git-safe ref characters; ids are
/// machine-authored, so this is a tripwire, not a codec.
///
/// The bloom segment is [`short_hex`] — the same rendering the integration /
/// attempt / checkpoint / landing refs use, so one bloom's whole ref namespace
/// reads as one namespace.
fn candidate_ref(bloom: &BloomId, workpiece: &str) -> String {
    format!("heads/bloom/{}/candidate/{}", short_hex(&bloom.0), sanitize_ref_segment(workpiece))
}

/// The bloom-namespace ref a dead construct lane's partial worktree is pushed
/// to — `refs/heads/bloom/<short bloom hex>/member-checkpoint/<workpiece>`.
///
/// Public for the same reason [`candidate_ref_name`] is: the executor pushes
/// the capture and the prune path reclaims it, and one spelling keeps the two
/// from drifting. The workpiece segment goes through the same sanitizer
/// [`candidate_ref_name`] uses so a workpiece that is safe on one ref is safe on
/// the other.
#[must_use]
pub fn member_checkpoint_ref_name(bloom: &BloomId, workpiece: &str) -> String {
    format!("refs/{}", member_checkpoint_ref(bloom, workpiece))
}

fn member_checkpoint_ref(bloom: &BloomId, workpiece: &str) -> String {
    format!("heads/bloom/{}/member-checkpoint/{}", short_hex(&bloom.0), sanitize_ref_segment(workpiece))
}

/// Git-safe ref segment for a machine-authored workpiece id. Shared by the
/// candidate and member-checkpoint spellings so the two cannot drift.
fn sanitize_ref_segment(workpiece: &str) -> String {
    workpiece
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The branch a bloom's landing is proposed from — a sibling of `integration`
/// in the same per-bloom namespace, so a landing branch is swept by the same
/// namespace cleanup and reads as part of the bloom rather than a parallel
/// invention. Returned in the `bloom/…` form a pull request's `head` wants (no
/// `heads/` prefix); the source port's `landing_ref` is the same name as a ref.
///
/// Public and crate-level for the same reason [`candidate_ref_name`] is: both
/// ends of the name live outside one module. `land` proposes the branch, and
/// the outward projection locates that same proposal to comment a landing
/// receipt on it. One spelling, so the two cannot drift — a mismatch here does
/// not fail loudly, it looks up a pull request that is not there.
#[must_use]
pub fn landing_branch(bloom: &BloomId) -> String {
    format!("bloom/{}/landing", short_hex(&bloom.0))
}

/// A source-port fault, distinct from the clean [`IntegrateOutcome`] /
/// [`LandOutcome`] refusals (which are not errors). Its own type because the
/// port needs a variant the value vocabulary does not carry — the gated-land
/// refusal — alongside the underlying transport faults.
#[derive(Debug)]
pub enum SourceError {
    /// The underlying Git Data call failed (transport or non-2xx status).
    Github(GithubError),
    /// `land` was called while compare-and-swap mainline landing is gated off
    /// (`cas_land_enabled` is false). The gate defaults on since ADR-0149
    /// migration step 3, so this is now the explicit kill-switch state, not the
    /// default. Not a [`LandOutcome`]: the swap was never attempted.
    LandingDisabled,
    /// An operation needed a ref that does not exist — e.g. `integrate` before
    /// the bloom's integration namespace was created, or `land` with no
    /// mainline ref.
    MissingRef(String),
    /// A ref name or object sha did not parse as the expected hex-of-`Digest`
    /// form — a malformed checkpoint ref, or a git object sha that is not a
    /// 40-hex sha1 / 64-hex sha256 (git never hands back such a string, so this
    /// is a genuine transport-garbage fault, not an expected miss).
    Malformed(String),
    /// A mainline path resolved a digest ↔ git object through the
    /// [`aether_bloomery::Correspondence`] store and found none recorded — the honest boundary
    /// this slice draws (ADR-0150): the object was never materialized or its
    /// correspondence never seeded, so the port refuses cleanly rather than
    /// hex-punning a digest git cannot resolve. `what` names which resolution
    /// missed (mainline head, candidate tree, …).
    UnresolvedCorrespondence(String),
    /// The [`aether_bloomery::Correspondence`] store itself faulted (a durable read/write failed),
    /// distinct from a clean absent correspondence.
    Correspondence(CorrespondenceError),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Github(error) => write!(f, "git source backend: {error}"),
            Self::LandingDisabled => {
                write!(f, "compare-and-swap land is disabled (cas_land_enabled kill switch is off)")
            }
            Self::MissingRef(name) => write!(f, "git source backend: required ref `{name}` does not exist"),
            Self::Malformed(what) => write!(f, "git source backend: malformed {what}"),
            Self::UnresolvedCorrespondence(what) => {
                write!(f, "git source backend: no git-object correspondence recorded for {what}")
            }
            Self::Correspondence(error) => write!(f, "git source backend: {error}"),
        }
    }
}

impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Github(error) => Some(error),
            Self::Correspondence(error) => Some(error),
            _ => None,
        }
    }
}

impl From<GithubError> for SourceError {
    fn from(error: GithubError) -> Self {
        Self::Github(error)
    }
}

impl From<CorrespondenceError> for SourceError {
    fn from(error: CorrespondenceError) -> Self {
        Self::Correspondence(error)
    }
}

/// The content-derived address of the commit mainline became when a landing
/// was accepted. The accepting backend produces that commit (a squash accept
/// produces one that is *not* the proposed head), so nothing on our side has
/// recorded a correspondence for it — this is the digest the landing receipt
/// attests and the next bloom's base check reverse-resolves.
///
/// Domain-tagged like the candidate addresses (ADR-0152), so a landed head can
/// never collide with a candidate tree or checkout over equal object bytes.
#[derive(Serialize)]
struct LandedHeadAddress<'a> {
    object: &'a [u8],
}

impl ContentAddressed for LandedHeadAddress<'_> {
    const DOMAIN: &'static str = "aether.bloomery.landed.head";
}

/// The prose a landing proposal is opened with, assembled by the caller that can
/// see the bloom's membership — the messages its lanes wrote and the objects its
/// workpieces address. This port sees neither, so the text arrives here already
/// composed rather than being derived from the three digests `land` is given.
///
/// The title is optional because a bloom does not always have one to offer: a
/// member whose lane wrote no usable subject, and a multi-member bloom whose
/// several messages name no single change, both land under
/// [`landing_floor_title`] instead. The caller says which by leaving it `None`,
/// so the floor keeps one spelling — here, beside the proposal it opens.
#[derive(Clone, PartialEq, Eq, Debug, Default)]
pub struct LandingProposal {
    /// The proposal's title, or `None` to land under the floor.
    pub title: Option<String>,
    /// The caller's half of the body. The provenance footer is appended below
    /// it, so an assembly never restates the digests.
    pub body: String,
}

/// What asking the port to accept a bloom's own landing proposal did
/// (issue #4953).
///
/// The coordinator opens the proposal and, once its gate is green, merges it —
/// the same trust decision the pipeline already made, since ADR-0186 gives the
/// daily ref no required checks precisely because bloomery's own verify and
/// aggregate gates prove each landing. What is automated is the button press,
/// not the judgement, so this vocabulary is about whether the thing being
/// merged is still the thing that was proven.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LandAcceptance {
    /// The proposal merged, or had already merged. Either way the watch's next
    /// poll reads the commit mainline actually became; this reports only that
    /// there is nothing left to press.
    Accepted,
    /// Nothing to accept yet: the proposal's gate has not reported green.
    ///
    /// A gate that has concluded *red* also lands here. Classifying a red gate
    /// is [`SourceBackend::poll_land`]'s job — the watch consults it first, and
    /// a second classification here would be a second place to keep in step.
    /// What this answers is only whether the green a merge requires is in hand.
    Pending,
    /// Refused, and why. The landing does not proceed under this proposal.
    Refused(LandingRefusal),
}

/// Why a landing acceptance refused. Each variant is a different thing for an
/// operator to do, which is why they are not one string.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum LandingRefusal {
    /// The proposal no longer proposes what the bloom proved — its head is not
    /// the resolved head (it gained commits, or the branch was repointed), it
    /// aims at a branch other than this coordinator's mainline, or it is no
    /// longer open.
    Drifted {
        /// What moved, in the terms a reader of the journal needs.
        detail: String,
    },
    /// Mainline is no longer the base the bloom sealed against. It moved after
    /// the proposal was opened, so merging now would land the bloom's work onto
    /// a base it was never built or verified against.
    BaseMoved {
        /// The sealed base the bloom proved against.
        expected: Digest,
        /// The base mainline actually stands at now.
        actual: Digest,
    },
    /// The source itself refused the merge — a conflict, a protection rule, or
    /// a head that moved between the guard read and the merge call.
    Merge {
        /// The refusing status.
        status: u16,
        /// The refusal body, verbatim.
        detail: String,
    },
}

/// The drift refusal, spelled once so the several ways a proposal can stop
/// being the one that was proven all read the same in the journal.
fn refused_drift(detail: String) -> LandAcceptance {
    LandAcceptance::Refused(LandingRefusal::Drifted { detail })
}

impl fmt::Display for LandingRefusal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Drifted { detail } => write!(f, "the landing proposal drifted off the proven head ({detail})"),
            Self::BaseMoved { expected, actual } => write!(
                f,
                "mainline moved off the sealed base after the proposal opened (sealed `{}`, now `{}`)",
                to_hex(expected),
                to_hex(actual),
            ),
            Self::Merge { status, detail } => write!(f, "the source refused the merge with {status}: {detail}"),
        }
    }
}

/// The title a landing proposal falls back to: the bloom named by its short hex,
/// under the repository's `chore(meta)` type. Always a lint-valid Conventional
/// Commits header, which is what makes it a floor rather than a guess — mainline
/// squash-merges with this as the commit subject.
#[must_use]
pub fn landing_floor_title(bloom: &BloomId) -> String {
    format!("chore(meta): land bloom {}", short_hex(&bloom.0))
}

/// The provenance footer every landing proposal body ends on: what is being
/// landed, onto what, and the digests a reader verifies it against. Prose for a
/// person — the machine side reads the proposal's number, never this text — so
/// it names the bloom and both ends of the swap rather than restating the whole
/// spec, and it sits below the assembled body because the change is what a
/// reader came for and the provenance is what they check afterwards.
///
/// It names the mainline ref alongside the two digests (ADR-0186): which branch
/// bloomery integrates on is boot configuration and moves per day, so a reader
/// of an older proposal should not have to reconstruct which ref it was aimed at
/// from the coordinator's configuration at the time.
fn render_provenance_footer(
    bloom: &BloomId,
    expected_base: &Digest,
    new_head: &Digest,
    mainline: &MainlineRef,
) -> String {
    format!(
        "---\n\n\
         Landing bloom `{}` onto `{mainline}`.\n\n\
         - sealed base: `{}`\n\
         - resolved head: `{}`\n\n\
         Bloomery opened this proposal after the bloom resolved and its aggregate \
         review passed. Merging it is what lands the bloom: the merge is observed, \
         a `Fact::Land` is admitted against the commit mainline actually becomes, \
         and the next bloom seals on that receipt.\n\n\
         Closing it without merging leaves the bloom resolved and supersedable.\n",
        to_hex(&bloom.0),
        to_hex(expected_base),
        to_hex(new_head),
    )
}

/// The title and body a proposal is opened with: the caller's assembly when it
/// made one, the floor plus a bare footer when it did not.
fn render_landing_proposal(
    bloom: &BloomId,
    expected_base: &Digest,
    new_head: &Digest,
    mainline: &MainlineRef,
    proposal: Option<&LandingProposal>,
) -> (String, String) {
    let footer = render_provenance_footer(bloom, expected_base, new_head, mainline);
    let title = proposal.and_then(|proposal| proposal.title.clone()).unwrap_or_else(|| landing_floor_title(bloom));
    let body = match proposal.map(|proposal| proposal.body.trim()) {
        Some(assembled) if !assembled.is_empty() => format!("{assembled}\n\n{footer}"),
        _ => footer,
    };
    (title, body)
}

/// The single mainline-admission claim ref (short `heads/…`-style form, no
/// leading `refs/` — the client prepends it), realizing Bloomery's "one sealed,
/// unlanded bloom per mainline" invariant as a shared repository ref
/// (ADR-0150 §The claim registry).
const ADMISSION_REF: &str = "bloomery/admission/mainline";

/// Git's canonical sha1 empty-tree object — always resolvable with no prior
/// tree/blob write, so every claim commit points here instead of at a per-claim
/// tree. Correct for a real (sha1) GitHub repo today; the amendment defers the
/// object-format transition, so a future sha256 repo's empty tree is a
/// different sha (§Side findings).
pub const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

/// The `Bloom-Id` message-line prefix a claim commit carries its holder on
/// (ADR-0150 amendment, #3598): `Bloom-Id: sha256-<hex>` for a live hold,
/// `Bloom-Id: tombstone` for the sweep-me sentinel. Never a bare object sha —
/// GitHub's Git Data API 500s on a `tree` naming a non-existent tree and its
/// pre-receive hook rejects bare 64-hex in a ref name, the failures the first
/// live bloom trial (2026-07-17) hit tree-punning a bloom id.
const BLOOM_ID_PREFIX: &str = "Bloom-Id: ";
const BLOOM_ID_TOMBSTONE: &str = "tombstone";
const BLOOM_ID_SHA256_PREFIX: &str = "sha256-";

/// Render a claim commit's message: a legible lead line plus the parseable
/// `Bloom-Id: sha256-<hex>` line [`parse_bloom_line`] resolves back.
pub fn render_claim_message(bloom: &BloomId) -> String {
    format!("bloomery claim\n\n{BLOOM_ID_PREFIX}{BLOOM_ID_SHA256_PREFIX}{}", to_hex(&bloom.0))
}

/// Render a tombstone claim commit's message: the same shape as
/// [`render_claim_message`], carrying the `tombstone` sentinel instead of a
/// bloom id.
pub fn render_tombstone_message() -> String {
    format!("bloomery claim tombstone\n\n{BLOOM_ID_PREFIX}{BLOOM_ID_TOMBSTONE}")
}

/// Resolve a claim commit's holder from its message's `Bloom-Id` line — the
/// inverse of [`render_claim_message`] / [`render_tombstone_message`].
/// `SourceError::Malformed` for a message carrying no `Bloom-Id` line, or one
/// whose value is neither `tombstone` nor a well-formed `sha256-<hex>` id.
fn parse_bloom_line(message: &str) -> Result<ClaimHolder, SourceError> {
    let Some(line) = message.lines().find_map(|line| line.strip_prefix(BLOOM_ID_PREFIX)) else {
        return Err(SourceError::Malformed(format!("commit message `{message}` carries no Bloom-Id line")));
    };
    if line == BLOOM_ID_TOMBSTONE {
        return Ok(ClaimHolder::Tombstoned);
    }
    line.strip_prefix(BLOOM_ID_SHA256_PREFIX)
        .and_then(digest_from_hex)
        .map(|digest| ClaimHolder::Held(BloomId(digest)))
        .ok_or_else(|| SourceError::Malformed(format!("Bloom-Id line `{line}`")))
}

/// Render a digest as 64 lowercase hex — the form a digest takes when it names
/// a **ref-name segment** in the branch namespace (`heads/bloom/<hex>/…`), the
/// one place a digest is still hex-rendered now that object shas resolve through
/// the [`aether_bloomery::Correspondence`] store. `pub` (not `pub(crate)`) because it lives in a
/// private module — its reach is already crate-internal, and `pub(crate)` here
/// would be redundant.
#[must_use]
pub fn to_hex(digest: &Digest) -> String {
    let mut out = String::with_capacity(64);
    for byte in digest.as_bytes() {
        out.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
        out.push(char::from_digit(u32::from(byte & 0x0f), 16).unwrap_or('0'));
    }
    out
}

// One hex character to its 0..=15 nibble, or `None` for a non-hex byte.
fn hex_nibble(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

/// Parse a 64-hex ref-name segment back into a digest, or `None` if it is not
/// exactly 64 hex characters — the inverse of [`to_hex`] for the branch
/// namespace's digest segments (checkpoint enumeration) and the claim-registry
/// synthetic-tree encoding. **Not** for a git object sha: those are sha1/40 or
/// sha256/32 and resolve through the [`aether_bloomery::Correspondence`] store. `pub` for the
/// same private-module reason as [`to_hex`].
pub fn digest_from_hex(sha: &str) -> Option<Digest> {
    if sha.len() != 64 {
        return None;
    }
    let mut bytes = [0u8; 32];
    let raw = sha.as_bytes();
    for (i, byte) in bytes.iter_mut().enumerate() {
        *byte = (hex_nibble(raw[i * 2])? << 4) | hex_nibble(raw[i * 2 + 1])?;
    }
    Some(Digest::from_bytes(bytes))
}

/// The git source backend over a [`GitDataApi`] client and a persisted
/// [`aether_bloomery::Correspondence`] handle (ADR-0150). The correspondence is the seam the
/// mainline paths resolve real git shas through; the `cas_land_enabled` gate is
/// on by default since ADR-0149 migration step 3 made the CAS `land` the landing
/// of record — a `false` gate is the explicit kill switch under which `land`
/// refuses.
///
/// The [`GithubApi`] bound is the landing assembly's: a proposal whose member
/// named no commit message falls back to that member's source issue title, so
/// the backend that opens the proposal has to be able to read one.
pub struct GitSource<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi> {
    client: C,
    correspondence: SharedCorrespondence,
    cas_land_enabled: bool,
    mainline: MainlineRef,
}

impl<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi> GitSource<C> {
    /// Build a source backend over `client` and `correspondence`, with mainline
    /// landing gated by `cas_land_enabled` (`false` is the kill switch under
    /// which `land` refuses; production wires it on, per ADR-0149 migration
    /// step 3) and every mainline read aimed at `mainline` (ADR-0186 — the host
    /// resolves which ref that is at boot).
    pub fn new(client: C, correspondence: SharedCorrespondence, cas_land_enabled: bool, mainline: MainlineRef) -> Self {
        Self { client, correspondence, cas_land_enabled, mainline }
    }

    // The live mainline ref, or the clean `MissingRef` naming the ref that is
    // not there — the one read every mainline path goes through, so observation
    // and the land compare can never end up aimed at different branches.
    fn mainline_head(&self) -> Result<GitRef, SourceError> {
        self.client
            .get_ref(self.mainline.git_ref())?
            .ok_or_else(|| SourceError::MissingRef(self.mainline.git_ref().to_owned()))
    }

    /// Borrow the underlying client (test introspection).
    #[must_use]
    pub const fn client(&self) -> &C {
        &self.client
    }

    // Forward-resolve a bloom digest to the real git object sha it corresponds
    // to, or the clean `UnresolvedCorrespondence` when none was recorded. `what`
    // names the resolution for the error message.
    fn resolve_git_sha(&self, digest: &Digest, what: &str) -> Result<String, SourceError> {
        self.correspondence
            .resolve_backend_object(digest)?
            .map(GitObjectId::try_from)
            .transpose()?
            .map(|git| git.to_hex())
            .ok_or_else(|| SourceError::UnresolvedCorrespondence(what.to_owned()))
    }

    // Reverse-resolve a real git object sha to the bloom digest it corresponds
    // to. A sha that is not a 40/64-hex object id is `Malformed` (transport
    // garbage); a well-formed sha with no recorded correspondence is the clean
    // `UnresolvedCorrespondence` — the real-repo boundary that retires the old
    // fixed-64-hex `Malformed` gate.
    fn resolve_object_digest(&self, sha: &str, what: &str) -> Result<Digest, SourceError> {
        let git =
            GitObjectId::from_hex(sha).ok_or_else(|| SourceError::Malformed(format!("git object sha `{sha}`")))?;
        self.correspondence
            .resolve_digest(&BackendObjectId::from(git))?
            .ok_or_else(|| SourceError::UnresolvedCorrespondence(what.to_owned()))
    }

    // The digest naming mainline's current head, minting and recording one when
    // the head is a commit Bloomery did not produce.
    //
    // Anything merged outside a bloom moves mainline to an object with no
    // recorded correspondence, so demanding one here refuses to name the ordinary
    // state of a shared repository — and refuses it as a *fault*, which the land
    // reactor re-drives, so the coordinator spins on a condition no retry can
    // resolve: nothing will ever record a correspondence for a commit made
    // outside Bloomery. Minting the address is what [`poll_land`] already does
    // for a squash-merge commit, for the same reason; doing it here lets an
    // unrecognized head be reported as the moved base it is.
    fn mainline_digest(&self, sha: &str) -> Result<Digest, SourceError> {
        let object =
            GitObjectId::from_hex(sha).ok_or_else(|| SourceError::Malformed(format!("git object sha `{sha}`")))?;
        if let Some(known) = self.correspondence.resolve_digest(&BackendObjectId::from(&object))? {
            return Ok(known);
        }

        let minted = digest_of(&LandedHeadAddress { object: object.bytes() });
        self.correspondence.record(&minted, &BackendObjectId::from(object))?;
        Ok(minted)
    }

    // The tree digest at real commit `sha`: read the commit for its real tree
    // object sha, then reverse-resolve that object to its tree digest. The
    // integration-branch path reads a real git tree this way; the claim path
    // carries the bloom id on a `Bloom-Id` message line instead (`parse_bloom_line`).
    fn integration_tree(&self, sha: &str) -> Result<Digest, SourceError> {
        let commit = self.client.get_commit(sha)?;
        self.resolve_object_digest(&commit.tree, "integration branch tree object")
    }

    // The bloom digest naming real git tree object `sha`, minting and recording
    // one when the tree is new. A tree-replace integrate never needs this — its
    // result is the candidate's own already-recorded tree — but a bootstrapped
    // branch and a merge both produce trees no digest has ever named, and an
    // unnamed integration tree is unreachable: `snapshot` and the
    // stale-checkpoint compare both reverse-resolve the branch through it.
    fn integration_tree_digest(&self, sha: &str) -> Result<Digest, SourceError> {
        let object = GitObjectId::from_hex(sha)
            .ok_or_else(|| SourceError::Malformed(format!("integration tree sha `{sha}`")))?;
        if let Some(known) = self.correspondence.resolve_digest(&BackendObjectId::from(&object))? {
            return Ok(known);
        }

        let minted = digest_of(&IntegrationTreeAddress { object: object.bytes() });
        self.correspondence.record(&minted, &BackendObjectId::from(object))?;
        Ok(minted)
    }

    fn integration_ref(bloom: &BloomId) -> String {
        format!("heads/bloom/{}/integration", short_hex(&bloom.0))
    }

    fn attempt_ref(bloom: &BloomId, attempt: u32) -> String {
        format!("heads/bloom/{}/attempt/{attempt}", short_hex(&bloom.0))
    }

    fn checkpoint_prefix(bloom: &BloomId) -> String {
        format!("heads/bloom/{}/checkpoint/", short_hex(&bloom.0))
    }

    fn checkpoint_ref(bloom: &BloomId, tree: &Digest) -> String {
        format!("{}{}", Self::checkpoint_prefix(bloom), to_hex(tree))
    }

    fn landing_ref(bloom: &BloomId) -> String {
        format!("heads/{}", landing_branch(bloom))
    }

    fn working_ref_prefix(bloom: &BloomId) -> String {
        format!("heads/bloom/{}/", short_hex(&bloom.0))
    }

    /// Whether `name` is a candidate, integration, checkpoint, or member-checkpoint
    /// ref under `prefix` — the working refs a terminal bloom no longer needs.
    /// Landing and attempt refs stay: a landing proposal may still be open, and
    /// this issue does not retire them. Claim refs live in a different namespace
    /// (`bloomery/claims/`, `bloomery/admission/`) and cannot match the prefix.
    fn is_reclaimable_working_ref(name: &str, prefix: &str) -> bool {
        name.strip_prefix(prefix).is_some_and(|rest| {
            rest == "integration"
                || rest.starts_with("candidate/")
                || rest.starts_with("checkpoint/")
                || rest.starts_with("member-checkpoint/")
        })
    }

    /// Delete `bloom`'s candidate, integration, checkpoint, and member-checkpoint
    /// refs. Idempotent: a name that is already gone is a success. Does not touch
    /// claim refs (ADR-0150 — those have their own release reactor) or the landing
    /// branch.
    ///
    /// # Errors
    /// A transport or backend fault other than an already-absent ref.
    pub fn prune_working_refs(&self, bloom: &BloomId) -> Result<usize, SourceError> {
        LandingSource::prune_working_refs(self, bloom)
    }

    // Point the bloom's landing branch at `sha`, creating it or moving it. The
    // force is deliberate and safe: this ref is per-bloom and Bloomery-owned,
    // and the only way to reach it with the branch already elsewhere is a prior
    // attempt that failed after the ref write and before the proposal (an open
    // proposal is adopted before this runs). Fast-forward-only would wedge that
    // bloom's landing forever on a ref nobody else reads.
    fn point_landing_branch(&self, bloom: &BloomId, sha: &str) -> Result<(), SourceError> {
        let name = Self::landing_ref(bloom);
        match self.client.get_ref(&name)? {
            Some(existing) if existing.sha == sha => Ok(()),
            Some(_) => self.client.update_ref(&name, sha, true).map(|_| ()).map_err(SourceError::Github),
            None => self.client.create_ref(&name, sha).map(|_| ()).map_err(SourceError::Github),
        }
    }

    /// Create the bloom's integration namespace at `base`: the single-writer
    /// `integration` branch and its first `attempt` handle, both at the base
    /// commit. Idempotent — an already-present ref is left as is, so a
    /// re-created namespace is a no-op rather than a conflict.
    ///
    /// # Errors
    /// The Git Data ref reads/writes failed.
    pub fn create_namespace(&self, bloom: &BloomId, base: &Digest) -> Result<(), SourceError> {
        let base_sha = self.resolve_git_sha(base, "namespace base head digest")?;
        self.ensure_ref(&Self::integration_ref(bloom), &base_sha)?;
        self.ensure_ref(&Self::attempt_ref(bloom, 1), &base_sha)?;
        Ok(())
    }

    // The integration branch's current position: its tree digest —
    // minting-and-recording a correspondence for a tree no digest names yet
    // (the freshly-created branch still at the base commit — the base's head
    // digest maps to its *commit*, never its tree; minted content-derived over
    // the tree object id, so a re-read returns the identical digest and the
    // first integrate's expected-compare resolves) — plus the landable head the
    // current commit reverse-resolves to once the branch has advanced past the
    // base (each integrate records `head ↔ commit`), so an interrupted fold
    // recovers its resolve head (ADR-0152).
    fn current_integration_position(
        &self,
        bloom: &BloomId,
        base_sha: &str,
    ) -> Result<IntegrationPosition, SourceError> {
        let integration = Self::integration_ref(bloom);
        let current = self.client.get_ref(&integration)?.ok_or(SourceError::MissingRef(integration))?;
        let commit = self.client.get_commit(&current.sha)?;
        let tree = self.integration_tree_digest(&commit.tree)?;
        let head = if current.sha == base_sha {
            // The un-advanced branch's commit is the base commit, whose
            // correspondence names the base digest — not a landable head.
            None
        } else {
            let commit_object = GitObjectId::from_hex(&current.sha)
                .ok_or_else(|| SourceError::Malformed(format!("integration commit sha `{}`", current.sha)))?;
            self.correspondence.resolve_digest(&BackendObjectId::from(commit_object))?
        };
        Ok(IntegrationPosition { checkpoint: Checkpoint { bloom: *bloom, tree }, head })
    }

    fn ensure_ref(&self, name: &str, sha: &str) -> Result<(), SourceError> {
        if self.client.get_ref(name)?.is_none() {
            self.client.create_ref(name, sha)?;
        }
        Ok(())
    }

    // Built from `claims_prefix` so the writer and the `enumerate_claims`
    // reader cannot drift byte-wise (#3668) — a mismatched prefix would
    // mis-parse every enumerated workpiece id.
    fn workpiece_claim_ref(workpiece: &WorkpieceId) -> String {
        format!("{}{}", Self::claims_prefix(), workpiece.0)
    }

    // The `heads/…`-form ref name a single `ClaimRefKind` addresses — the per-ref
    // dual of `claim_targets`, for the boot-reconcile deep-heal ops that act on
    // one enumerated ref at a time (ADR-0150 §The claim registry, amended PR #3556).
    fn ref_name(ref_kind: &ClaimRefKind) -> String {
        match ref_kind {
            ClaimRefKind::Workpiece(workpiece) => Self::workpiece_claim_ref(workpiece),
            ClaimRefKind::MainlineAdmission => ADMISSION_REF.to_owned(),
        }
    }

    // The prefix every per-workpiece claim ref lives under — the enumeration base
    // `enumerate_claims` walks, mirroring `checkpoint_prefix`.
    fn claims_prefix() -> &'static str {
        "bloomery/claims/"
    }

    // The typed claim targets an acquire / transfer / release walks: the
    // per-workpiece claim refs paired with their `ClaimRefKind`, and — appended
    // when `with_admission` — the single mainline-admission ref. Pairing the
    // kind with the name lets a conflict report which ref it was on without a
    // second name→kind parse.
    fn claim_targets(workpieces: &[WorkpieceId], with_admission: bool) -> Vec<(ClaimRefKind, String)> {
        let mut targets: Vec<(ClaimRefKind, String)> =
            workpieces.iter().map(|w| (ClaimRefKind::Workpiece(w.clone()), Self::workpiece_claim_ref(w))).collect();
        if with_admission {
            targets.push((ClaimRefKind::MainlineAdmission, ADMISSION_REF.to_owned()));
        }
        targets
    }

    // A claim commit points at the well-known empty tree and carries the
    // claiming bloom's id on a parseable `Bloom-Id` message line — never a real
    // per-claim tree, which GitHub's Git Data API 500s on for a bloom id (a
    // sha256 digest never names a real sha1 tree) and whose bare-hex ref-name
    // form its pre-receive hook rejects outright. Returns the created commit's
    // sha (the value a ref is pointed at).
    fn create_claim_commit(&self, bloom: &BloomId, parents: &[String]) -> Result<String, SourceError> {
        Ok(self.client.create_commit(&render_claim_message(bloom), EMPTY_TREE, parents)?.sha)
    }

    // Resolve a claim commit's occupant the same way every non-tombstone-aware
    // call site (claim_holder, transfer_seal, complete_transfer) has always
    // read it: a live hold resolves to its bloom id; a lingering tombstone
    // (an interrupted release whose cleanup delete never ran) resolves to a
    // sentinel distinct from any real bloom id — grandfathering the pre-message
    // "any current occupant" read those sites still expect, without ever
    // writing the sentinel to git. `classify_holder` is the tombstone-aware
    // entry point call sites that actually branch on tombstoned-vs-held
    // (`release_targets`, `enumerate_claims`) use instead.
    fn resolve_holder(&self, sha: &str) -> Result<BloomId, SourceError> {
        Ok(match self.classify_holder(sha)? {
            ClaimHolder::Held(bloom) => bloom,
            ClaimHolder::Tombstoned => BloomId(Digest::default()),
        })
    }

    // The bloom currently holding `name`, resolved from its claim commit's
    // message, or `None` if the ref does not exist.
    fn claim_holder(&self, name: &str) -> Result<Option<BloomId>, SourceError> {
        match self.client.get_ref(name)? {
            None => Ok(None),
            Some(git_ref) => Ok(Some(self.resolve_holder(&git_ref.sha)?)),
        }
    }

    // Classify the ref pointing at `sha` for enumeration: parses the claim
    // commit's message `Bloom-Id` line — `tombstone` is a swept-me marker,
    // `sha256-<hex>` the holding bloom's id.
    fn classify_holder(&self, sha: &str) -> Result<ClaimHolder, SourceError> {
        let commit = self.client.get_commit(sha)?;
        parse_bloom_line(&commit.message)
    }

    // Resolve the holder of a ref an acquire/transfer/release just found held —
    // the ref must exist (a 422/absent-holder here is a race, surfaced as a
    // fault rather than a fabricated holder).
    fn require_holder(&self, name: &str) -> Result<BloomId, SourceError> {
        self.claim_holder(name)?.ok_or_else(|| SourceError::MissingRef(name.to_owned()))
    }

    // Delete every ref an acquire created before it hit a conflict — attempt all
    // of them (no first-error short-circuit), surfacing the first delete error
    // only after the whole rollback is attempted so one wedged delete cannot
    // strand the rest.
    fn rollback(&self, created: &[String]) -> Result<(), SourceError> {
        let mut first_error: Option<GithubError> = None;
        for name in created {
            if let Err(error) = self.client.delete_ref(name)
                && first_error.is_none()
            {
                first_error = Some(error);
            }
        }
        first_error.map_or(Ok(()), |error| Err(SourceError::Github(error)))
    }

    // Release each of `targets` the named `owner` holds by a fast-forward CAS to a
    // tombstone child commit (the linearization point) then a name-only cleanup
    // delete. An absent ref is skipped (idempotent); a ref already at the tombstone
    // commit is *already released* — an interrupted release's CAS linearized but its
    // cleanup delete did not run — so the cleanup delete is finished (behind the
    // same CAS reassertion, so a fresh claim that reused the name in the read→delete
    // window is spared rather than clobbered) and the walk continues (ADR-0150 §The
    // claim registry, amended PR #3556: the release path is idempotent over its own
    // tombstone). A ref a *different* bloom holds is spared and reported as `Held` —
    // the CAS read-guard that retires the check-then-delete TOCTOU.
    //
    // `owner` is `Some(bloom)` for the whole-op release paths — `release_seal`
    // (workpieces + admission) and `transfer_seal`'s dropped-member cleanup
    // (workpieces only) — and `None` for the boot-reconcile tombstone sweep, which
    // authorizes no live holder: a `None` owner sweeps a tombstone and spares any
    // live ref rather than deleting it.
    fn release_targets(
        &self,
        owner: Option<&BloomId>,
        targets: &[(ClaimRefKind, String)],
    ) -> Result<ClaimOutcome, SourceError> {
        for (kind, name) in targets {
            match self.release_target(owner, name)? {
                // Both clean terminals mean this ref is no longer held by the
                // owner, which is all an all-or-nothing release needs; the walk
                // continues.
                ClaimReleaseOutcome::Released | ClaimReleaseOutcome::AlreadyAbsent => {}
                ClaimReleaseOutcome::Changed { observed_holder } => {
                    return Ok(ClaimOutcome::Held { ref_kind: kind.clone(), held_by: observed_holder });
                }
            }
        }
        Ok(ClaimOutcome::Acquired)
    }

    // Release exactly one ref by name, reporting which of the three terminals it
    // reached. The single-ref body [`release_targets`] walks and
    // [`complete_release`] returns directly, so the CAS-to-tombstone guard,
    // the tombstone-race reassertion, and the lost-CAS re-read live in one place
    // rather than being written twice with a different result type each.
    fn release_target(&self, owner: Option<&BloomId>, name: &str) -> Result<ClaimReleaseOutcome, SourceError> {
        let Some(current) = self.client.get_ref(name)? else {
            return Ok(ClaimReleaseOutcome::AlreadyAbsent);
        };
        // An already-tombstoned ref is released regardless of `owner` — the
        // tombstone *is* the released state, so finishing the interrupted cleanup
        // delete is pure name reclamation, safe for any instance. But guard the
        // cleanup with the same CAS the live-holder branch uses: a blind name-only
        // delete would race a fresh claim that reused the name in the window since
        // the read, so re-assert the observed tombstone via a no-op fast-forward
        // first. If the ref moved to an unrelated fresh claim commit that
        // reassertion 422s (the new commit is not a fast-forward descendant of the
        // stale tombstone), so the name has already been reclaimed by that holder —
        // skip the delete rather than clobbering a claim we never owned. Either way
        // the holder this call was authorized against is gone, which is
        // `AlreadyAbsent`.
        let holder = match self.classify_holder(&current.sha)? {
            ClaimHolder::Tombstoned => {
                match self.client.update_ref(name, &current.sha, false) {
                    Ok(_) => self.client.delete_ref(name)?,
                    Err(GithubError::Status { status: 422, .. }) => {}
                    Err(error) => return Err(SourceError::Github(error)),
                }
                return Ok(ClaimReleaseOutcome::AlreadyAbsent);
            }
            ClaimHolder::Held(holder) => holder,
        };

        // A live ref is released only when `owner` names its holder; a `None`
        // sweep, or a mismatch, spares it and reports the holder it found.
        if owner != Some(&holder) {
            return Ok(ClaimReleaseOutcome::Changed { observed_holder: holder });
        }

        let tombstone = self.client.create_commit(&render_tombstone_message(), EMPTY_TREE, &[current.sha])?;
        match self.client.update_ref(name, &tombstone.sha, false) {
            Ok(_) => {}
            // Lost the fast-forward CAS to a concurrent mutation — re-read the
            // holder and report it rather than deleting a ref we no longer own.
            Err(GithubError::Status { status: 422, .. }) => {
                return Ok(ClaimReleaseOutcome::Changed { observed_holder: self.require_holder(name)? });
            }
            Err(error) => return Err(SourceError::Github(error)),
        }
        self.client.delete_ref(name)?;
        Ok(ClaimReleaseOutcome::Released)
    }
}

impl<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi> SourceBackend for GitSource<C> {
    type Error = SourceError;

    fn snapshot(&self, base: &Digest) -> Result<SourceSnapshot, Self::Error> {
        // Forward-resolve the base head digest to its real commit, read that
        // commit, and reverse-resolve its real tree sha back to the tree digest.
        let base_sha = self.resolve_git_sha(base, "snapshot base head digest")?;
        let commit = self.client.get_commit(&base_sha)?;
        let tree = self.resolve_object_digest(&commit.tree, "snapshot base tree object")?;
        Ok(SourceSnapshot { head: *base, tree })
    }

    fn mainline_head_sha(&self) -> Result<String, Self::Error> {
        // Read the live mainline ref, resolving no correspondence — the genesis
        // reconcile seeds the base↔head correspondence *from* this sha, so it
        // cannot itself depend on a recorded correspondence (#3615).
        Ok(self.mainline_head()?.sha)
    }

    fn observe_mainline_head(&self) -> Result<Digest, Self::Error> {
        // `mainline_digest` is exactly the reverse-resolve-or-mint this needs,
        // and is what `poll_land` already runs over a merge commit — an observed
        // head and a landed head are the same object seen from two sides, so
        // they must mint the same digest or the land following an observation
        // would compare-and-swap against a base it had itself just renamed.
        self.mainline_digest(&self.mainline_head_sha()?)
    }

    fn is_fast_forward(&self, from: &Digest, to: &Digest) -> Result<bool, Self::Error> {
        if from == to || *from == Snapshot::GENESIS_MAINLINE {
            return Ok(true);
        }
        let from_sha = self.resolve_git_sha(from, "mainline correspondence")?;
        let to_sha = self.resolve_git_sha(to, "observed head")?;
        Ok(self.client.is_ancestor(&from_sha, &to_sha)?)
    }

    fn checkpoint(&self, bloom: &BloomId, tree: &Digest) -> Result<Checkpoint, Self::Error> {
        let name = Self::checkpoint_ref(bloom, tree);
        // Idempotent: the checkpoint's identity is its tree, encoded in the ref
        // name, so a re-record of the same tree is the same checkpoint.
        if self.client.get_ref(&name)?.is_none() {
            // Pin the checkpoint at the integration branch's current commit —
            // the commit whose tree this is, just advanced there by integrate.
            let integration = self
                .client
                .get_ref(&Self::integration_ref(bloom))?
                .ok_or_else(|| SourceError::MissingRef(Self::integration_ref(bloom)))?;
            self.client.create_ref(&name, &integration.sha)?;
        }
        Ok(Checkpoint { bloom: *bloom, tree: *tree })
    }

    fn checkpoints(&self, bloom: &BloomId) -> Result<Vec<Checkpoint>, Self::Error> {
        let prefix = Self::checkpoint_prefix(bloom);
        let mut out = Vec::new();
        for git_ref in self.client.list_matching_refs(&prefix)? {
            let hex = git_ref.name.strip_prefix(&prefix).unwrap_or(&git_ref.name);
            let tree = digest_from_hex(hex)
                .ok_or_else(|| SourceError::Malformed(format!("checkpoint ref `{}`", git_ref.name)))?;
            out.push(Checkpoint { bloom: *bloom, tree });
        }
        Ok(out)
    }

    fn integration_checkpoint(&self, bloom: &BloomId, base: &Digest) -> Result<IntegrationPosition, Self::Error> {
        let base_sha = self.resolve_git_sha(base, "namespace base head digest")?;
        self.create_namespace(bloom, base)?;
        self.current_integration_position(bloom, &base_sha)
    }

    fn integrate(
        &self,
        bloom: &BloomId,
        candidate: &Digest,
        expected: &Checkpoint,
    ) -> Result<IntegrateOutcome, Self::Error> {
        let integration = Self::integration_ref(bloom);
        let current = self.client.get_ref(&integration)?.ok_or_else(|| SourceError::MissingRef(integration.clone()))?;
        let current_tree = self.integration_tree(&current.sha)?;

        // Single-writer CAS: if the branch has advanced past the expected
        // checkpoint, refuse rather than clobber the concurrent advance.
        if current_tree != expected.tree {
            return Ok(IntegrateOutcome::StaleCheckpoint { actual: current_tree });
        }

        // Resolve the candidate tree digest to its real git tree and create the
        // integration commit over it. The resulting integrated tree stays
        // resolvable through its own tree correspondence — which is exactly what
        // `StaleCheckpoint` reverse-resolves the advanced branch's tree back to,
        // and what `snapshot` reads. The produced commit is the landable *head*:
        // record it under a *distinct* head digest (a content-address over a
        // head marker, uncollidable with the tree digest by construction, #3615)
        // so `head ↔ commit` reverse-/forward-resolves independently of `tree ↔
        // tree-object`, and carry that head back in the outcome for the core to
        // thread through `land`'s `new_head`.
        let candidate_tree_sha = self.resolve_git_sha(candidate, "candidate tree digest")?;
        let commit = self.client.create_commit("bloomery integrate", &candidate_tree_sha, &[current.sha])?;
        let head = digest_of(&IntegratedHead { bloom: *bloom, tree: *candidate });
        let commit_object = GitObjectId::from_hex(&commit.sha)
            .ok_or_else(|| SourceError::Malformed(format!("integrate commit sha `{}`", commit.sha)))?;

        // Record the correspondence *before* advancing the ref, never after
        // (#3667). The two writes are not atomic, so one of them observes a
        // fault first, and the orders are not symmetric:
        //
        //   record → advance:  a fault leaves a correspondence for a commit no
        //                      ref names. Nothing resolves it, and the retry
        //                      re-creates a byte-identical commit (same tree,
        //                      same parent, so git hands back the same sha) and
        //                      re-records it idempotently. Recoverable.
        //   advance → record:  a fault leaves the head with no correspondence,
        //                      and the retry now reads `current_tree ==
        //                      candidate`, so the stale-checkpoint guard above
        //                      returns before reaching the record. The head is
        //                      never landable (`land` faults on an unresolved
        //                      correspondence) and never re-integratable — an
        //                      absorbing state with no recovery path.
        //
        // The commit exists either way by this point; only its reachability is
        // in question, and an unreferenced commit is ordinary git garbage.
        self.correspondence.record(&head, &BackendObjectId::from(commit_object))?;
        match self.client.update_ref(&integration, &commit.sha, false) {
            Ok(_) => Ok(IntegrateOutcome::Integrated { tree: *candidate, head }),
            // A 422 is GitHub's non-fast-forward refusal: a concurrent writer
            // moved the branch between our read and our update. That is the
            // same stale-checkpoint condition — re-read and report it as such.
            Err(GithubError::Status { status: 422, .. }) => {
                let advanced = self.client.get_ref(&integration)?.ok_or(SourceError::MissingRef(integration))?.sha;
                let actual = self.integration_tree(&advanced)?;
                Ok(IntegrateOutcome::StaleCheckpoint { actual })
            }
            Err(error) => Err(SourceError::Github(error)),
        }
    }

    fn integrate_merge(
        &self,
        bloom: &BloomId,
        candidate_ref: &str,
        expected: &Checkpoint,
    ) -> Result<IntegrateOutcome, Self::Error> {
        let integration = Self::integration_ref(bloom);
        let current = self.client.get_ref(&integration)?.ok_or_else(|| SourceError::MissingRef(integration.clone()))?;
        let current_tree = self.integration_tree(&current.sha)?;

        // Same single-writer CAS the tree-replace path runs. It is a pre-check
        // rather than a swap here: the merge endpoint commits onto the branch
        // itself and takes no expected-sha, so unlike `integrate`'s
        // `update_ref` there is no 422 to catch a writer that raced in behind
        // the read. The fold owning this branch alone is what makes that safe;
        // the pre-check catches the case that actually happens, a restart
        // resuming against a checkpoint the branch has already passed.
        if current_tree != expected.tree {
            return Ok(IntegrateOutcome::StaleCheckpoint { actual: current_tree });
        }

        let commit = match self.client.merge(&integration, candidate_ref, &format!("bloomery fold {candidate_ref}"))? {
            MergeResult::Merged(commit) => commit,
            // The branch already carries this candidate — a fold resuming after
            // an interrupted run re-offering a member it already folded. Report
            // where the branch stands so the fold advances past it instead of
            // stalling. Reaching this at the *base* commit would resolve a base
            // digest as a landable head, but that needs a candidate whose tree
            // equals the base tree, and capture refuses an empty diff.
            MergeResult::AlreadyUpToDate => {
                let head = self.resolve_object_digest(&current.sha, "integration head commit")?;
                return Ok(IntegrateOutcome::Integrated { tree: current_tree, head });
            }
            // A cross-member collision: an owner decision, not a fault to
            // retry. The client follows a 409 with a compare so the paths and
            // the candidate's remaining diff reach the reconcile overlay.
            MergeResult::Conflict { paths, patch, .. } => {
                return Ok(IntegrateOutcome::Conflict { at: current_tree, paths, diff: patch });
            }
        };

        // The merged tree is new to the correspondence — it is neither the
        // candidate's tree nor the branch's previous one — so it has to be
        // named here or the next snapshot could not reverse-resolve the branch.
        // The head stays a distinct content-address over the tree, the same way
        // the tree-replace path keeps `head ↔ commit` from clobbering
        // `tree ↔ tree-object`.
        let tree = self.integration_tree_digest(&commit.tree)?;
        let head = digest_of(&IntegratedHead { bloom: *bloom, tree });
        let commit_object = GitObjectId::from_hex(&commit.sha)
            .ok_or_else(|| SourceError::Malformed(format!("merge commit sha `{}`", commit.sha)))?;
        self.correspondence.record(&head, &BackendObjectId::from(commit_object))?;
        Ok(IntegrateOutcome::Integrated { tree, head })
    }

    fn adopt_candidate(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        workpiece: &str,
    ) -> Result<bool, Self::Error> {
        // Adopt what is absent, and only that. The successor's namespace is
        // written by its own captures and by this adoption alone, so a ref
        // already sitting at this address is one of exactly two things, and
        // neither wants a predecessor's sha written over it: an earlier lap's
        // adoption, which wrote the very sha this one would write, or a capture
        // the successor produced itself. The second is the mixed supersession
        // (#4903) — some members re-ran under the successor while others
        // arrived on inherited claims — where a force silently replaces fresh
        // work with the superseded candidate it was re-run to supersede.
        //
        // The force this replaces existed for the re-drained fold that re-adopts
        // a ref it already wrote; skipping is idempotent for that case too, so
        // the guarantee is kept and the clobber is structurally unreachable
        // rather than avoided by a caller getting the member set right.
        let target = candidate_ref(successor, workpiece);
        if self.client.get_ref(&target)?.is_some() {
            return Ok(true);
        }

        let Some(source) = self.client.get_ref(&candidate_ref(predecessor, workpiece))? else {
            return Ok(false);
        };

        self.client.create_ref(&target, &source.sha).map(|_| true).map_err(SourceError::Github)
    }

    fn land(&self, bloom: &BloomId, expected_base: &Digest, new_head: &Digest) -> Result<LandOutcome, Self::Error> {
        self.land_proposal(bloom, expected_base, new_head, None)
    }

    fn poll_land(&self, bloom: &BloomId, expected_base: &Digest, number: u64) -> Result<LandProposal, Self::Error> {
        // A proposal that no longer exists cannot land and will never resolve,
        // so it reads as declined rather than leaving a watch spinning forever
        // on a number nothing answers.
        let Some(pull) = self.client.get_pull_request(number)? else {
            return Ok(LandProposal::Declined);
        };
        let Some(merge_commit) = pull.merge_commit_sha else {
            if pull.state != PullRequestState::Open {
                return Ok(LandProposal::Declined);
            }
            // An open proposal that has not merged is only *waiting* while its
            // gate might still pass. Once a check has concluded red the
            // proposal cannot merge, and reporting that as `Open` is what left
            // a bloom polling something nothing would ever accept. Anything
            // pending — or no check reported yet — stays open: a partial gate
            // is not a verdict.
            return Ok(match self.client.checks_for_ref(&pull.head_sha)? {
                ChecksState::Failed { failing } => LandProposal::ChecksFailed { failing },
                ChecksState::Absent | ChecksState::Pending | ChecksState::Passed => LandProposal::Open,
            });
        };

        // Mainline became the *merge* commit, which under a squash accept is a
        // commit nothing on our side produced — so there is no correspondence
        // for it yet, and the next bloom's `land` would fail to reverse-resolve
        // mainline's digest. Mint its address and record it here, so the chain
        // from this receipt to the next bloom's base check closes.
        let object = GitObjectId::from_hex(&merge_commit)
            .ok_or_else(|| SourceError::Malformed(format!("landing merge commit sha `{merge_commit}`")))?;
        let new_head = digest_of(&LandedHeadAddress { object: object.bytes() });
        self.correspondence.record(&new_head, &BackendObjectId::from(object))?;
        Ok(LandProposal::Landed(LandingReceipt { bloom: *bloom, previous_base: *expected_base, new_head }))
    }

    fn claim_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, Self::Error> {
        let targets = Self::claim_targets(workpieces, true);
        let mut created: Vec<String> = Vec::new();
        for (kind, name) in &targets {
            // A fresh root claim commit per member, then an atomic create. Every
            // failure path — the commit create, the ref create, and (below) the
            // holder resolution — first rolls back every ref this acquire already
            // created, so an aborted acquire never leaks a partial claim.
            // Rollback is best-effort here: a rollback fault must never mask the
            // triggering error/outcome below, which is what the caller needs to
            // see — so its own Result is deliberately dropped, not `?`-propagated.
            let commit = match self.create_claim_commit(bloom, &[]) {
                Ok(commit) => commit,
                Err(error) => {
                    let _ = self.rollback(&created);
                    return Err(error);
                }
            };
            match self.client.create_ref(name, &commit) {
                Ok(_) => created.push(name.clone()),
                // A 422 is another bloom's existing hold. Roll back our own refs
                // first — the conflicting ref is another bloom's, never among
                // them — then resolve and report the first conflict.
                Err(GithubError::Status { status: 422, .. }) => {
                    let _ = self.rollback(&created);
                    return Ok(ClaimOutcome::Held { ref_kind: kind.clone(), held_by: self.require_holder(name)? });
                }
                Err(error) => {
                    let _ = self.rollback(&created);
                    return Err(SourceError::Github(error));
                }
            }
        }
        Ok(ClaimOutcome::Acquired)
    }

    fn transfer_seal(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        carried: &[WorkpieceId],
        net_new: &[WorkpieceId],
        dropped: &[WorkpieceId],
    ) -> Result<ClaimOutcome, Self::Error> {
        // Fast-forward-CAS the carried workpiece refs + the admission ref from
        // predecessor to successor.
        for (kind, name) in Self::claim_targets(carried, true) {
            let current = self.client.get_ref(&name)?.ok_or_else(|| SourceError::MissingRef(name.clone()))?;
            let holder = self.resolve_holder(&current.sha)?;
            if holder != *predecessor {
                // A concurrent mutation moved the ref off the predecessor — the
                // CAS loses cleanly, the ref never momentarily absent.
                return Ok(ClaimOutcome::Held { ref_kind: kind, held_by: holder });
            }
            // The successor claim commit is parented on the predecessor's, so the
            // commit chain IS the claim lineage and `update_ref(force:false)` is a
            // genuine fast-forward CAS.
            let successor_commit = self.create_claim_commit(successor, &[current.sha])?;
            match self.client.update_ref(&name, &successor_commit, false) {
                Ok(_) => {}
                Err(GithubError::Status { status: 422, .. }) => {
                    return Ok(ClaimOutcome::Held { ref_kind: kind, held_by: self.require_holder(&name)? });
                }
                Err(error) => return Err(SourceError::Github(error)),
            }
        }

        // Fresh-acquire the successor's net-new workpieces (conflicting only on a
        // foreign hold, since the carried refs already named the predecessor).
        for workpiece in net_new {
            let name = Self::workpiece_claim_ref(workpiece);
            let commit = self.create_claim_commit(successor, &[])?;
            match self.client.create_ref(&name, &commit) {
                Ok(_) => {}
                Err(GithubError::Status { status: 422, .. }) => {
                    return Ok(ClaimOutcome::Held {
                        ref_kind: ClaimRefKind::Workpiece(workpiece.clone()),
                        held_by: self.require_holder(&name)?,
                    });
                }
                Err(error) => return Err(SourceError::Github(error)),
            }
        }

        // Release the members the successor drops (the predecessor's holds). This
        // is best-effort cleanup of refs the predecessor owns, so its outcome is
        // not reported — a genuine fault still propagates.
        self.release_targets(Some(predecessor), &Self::claim_targets(dropped, false))?;

        Ok(ClaimOutcome::Acquired)
    }

    fn release_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, Self::Error> {
        self.release_targets(Some(bloom), &Self::claim_targets(workpieces, true))
    }

    fn enumerate_claims(&self) -> Result<Vec<ClaimRefState>, Self::Error> {
        // Mirror `checkpoints`: list every per-workpiece claim ref under the shared
        // prefix, then read the single admission ref by name. Classify each by its
        // commit tree — the tombstone sentinel is the all-zero tree `release_seal`
        // writes, everything else is the holding bloom's id (ADR-0150 §The claim
        // registry, amended PR #3556).
        let prefix = Self::claims_prefix();
        let mut states = Vec::new();
        for git_ref in self.client.list_matching_refs(prefix)? {
            let raw = git_ref.name.strip_prefix(prefix).unwrap_or(&git_ref.name);
            let ref_kind = ClaimRefKind::Workpiece(WorkpieceId(raw.to_owned()));
            states.push(ClaimRefState { ref_kind, holder: self.classify_holder(&git_ref.sha)? });
        }
        if let Some(admission) = self.client.get_ref(ADMISSION_REF)? {
            states.push(ClaimRefState {
                ref_kind: ClaimRefKind::MainlineAdmission,
                holder: self.classify_holder(&admission.sha)?,
            });
        }
        Ok(states)
    }

    fn complete_transfer(
        &self,
        predecessor: &BloomId,
        successor: &BloomId,
        ref_kind: &ClaimRefKind,
    ) -> Result<ClaimOutcome, Self::Error> {
        let name = Self::ref_name(ref_kind);
        let Some(current) = self.client.get_ref(&name)? else {
            // A half-transfer never leaves a carried ref absent — `transfer_seal`
            // fast-forward-CASes each ref (never delete-then-create), so a ref is
            // always at the predecessor or the successor mid-flight, never gone.
            // An absent ref is thus nothing to converge: the convergent no-op.
            return Ok(ClaimOutcome::Acquired);
        };
        let holder = self.resolve_holder(&current.sha)?;
        // Already at the successor — the ref this crash-interrupted transfer already
        // moved. The idempotent no-op that lets the boot re-drive converge.
        if holder == *successor {
            return Ok(ClaimOutcome::Acquired);
        }
        // At any holder other than the predecessor — a foreign hold or a concurrent
        // mutation. The clean `Held`, never a stomp.
        if holder != *predecessor {
            return Ok(ClaimOutcome::Held { ref_kind: ref_kind.clone(), held_by: holder });
        }
        // At the predecessor — finish the fast-forward CAS to the successor, the
        // successor commit parented on the current one so `update_ref(force:false)`
        // is a genuine fast-forward (the same lineage `transfer_seal` builds).
        let successor_commit = self.create_claim_commit(successor, &[current.sha])?;
        match self.client.update_ref(&name, &successor_commit, false) {
            Ok(_) => Ok(ClaimOutcome::Acquired),
            Err(GithubError::Status { status: 422, .. }) => {
                Ok(ClaimOutcome::Held { ref_kind: ref_kind.clone(), held_by: self.require_holder(&name)? })
            }
            Err(error) => Err(SourceError::Github(error)),
        }
    }

    fn complete_release(
        &self,
        expected_holder: Option<&BloomId>,
        ref_kind: &ClaimRefKind,
    ) -> Result<ClaimReleaseOutcome, Self::Error> {
        self.release_target(expected_holder, &Self::ref_name(ref_kind))
    }
}

/// The landing-assembly face of the source port: what a caller that can see a
/// bloom's membership needs, over and above the digest-only
/// [`SourceBackend::land`].
///
/// Its own trait rather than a wider `land` on the port, because the port's
/// vocabulary is digests and outcomes and this is prose: a title, a body, and
/// the issue titles the body falls back to. The reactor holds a
/// [`LandingSource`] and calls [`land_proposal`](Self::land_proposal); the port
/// contract stays exactly as narrow as it was, and `land` delegates here with no
/// proposal, which is the floor the assembly falls back to anyway.
pub trait LandingSource: SourceBackend<Error = SourceError> {
    /// The human-authored title of issue `number`, or `None` when the repository
    /// holds no such object.
    ///
    /// # Errors
    /// The surface is unreachable or returned a non-404 error status.
    fn issue_title(&self, number: u64) -> Result<Option<String>, SourceError>;

    /// Propose landing `new_head` onto mainline under caller-assembled prose,
    /// guarded by `expected_base` exactly as [`SourceBackend::land`] is.
    ///
    /// # Errors
    /// [`SourceError::LandingDisabled`] while the land gate is off, or a
    /// transport/backend fault (a moved base is the clean
    /// [`LandOutcome::BaseMoved`], not an error).
    fn land_proposal(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        proposal: Option<&LandingProposal>,
    ) -> Result<LandOutcome, SourceError>;

    /// Accept the landing proposal numbered `number` — merge the pull request
    /// the port itself opened for `bloom`, once its gate is green and it still
    /// proposes exactly what the bloom proved (issue #4953).
    ///
    /// Gated by the same `cas_land_enabled` kill switch
    /// [`land_proposal`](Self::land_proposal) is, and guarded by the same
    /// compare-and-swap the proposal was opened under: `expected_base` has to
    /// still be mainline, `new_head` has to still be the proposal's head, and
    /// the merge itself is issued against that head sha so the source refuses
    /// it if the branch moves in between. Refuse-and-surface is the answer to
    /// every one of those moving — never merge-anyway.
    ///
    /// # Errors
    /// [`SourceError::LandingDisabled`] while the land gate is off, or a
    /// transport/backend fault. A refusal is [`LandAcceptance::Refused`], not an
    /// error: nothing about it gets better by re-driving the same call.
    fn accept_land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        number: u64,
    ) -> Result<LandAcceptance, SourceError>;

    /// Delete `bloom`'s candidate, integration, and checkpoint refs. Claim refs
    /// and the landing branch are spared. See [`GitSource::prune_working_refs`].
    ///
    /// # Errors
    /// A transport or backend fault other than an already-absent ref.
    fn prune_working_refs(&self, bloom: &BloomId) -> Result<usize, SourceError>;

    /// Close issue `number` after leaving `comment` on it — the land reactor's
    /// human-facing close so GitHub agrees with a day-branch land that closing
    /// keywords will not see until nightly sync-back.
    ///
    /// Both writes are attempted: a comment the repository refuses still closes,
    /// and a close that fails still leaves the comment when one was accepted.
    ///
    /// # Errors
    /// The surface is unreachable, the issue is absent, or either write was refused.
    fn close_issue(&self, number: u64, comment: &str) -> Result<(), SourceError>;
}

impl<C: GitDataApi + PullRequestApi + GithubApi + IssueStateApi> LandingSource for GitSource<C> {
    fn issue_title(&self, number: u64) -> Result<Option<String>, SourceError> {
        Ok(self.client.issue_title(number)?.map(|title| title.trim().to_owned()).filter(|title| !title.is_empty()))
    }

    fn close_issue(&self, number: u64, comment: &str) -> Result<(), SourceError> {
        let commented = self.client.create_comment(&NewComment { issue_number: number, body: comment.to_owned() });
        let closed = self.client.close_issue(number);
        commented?;
        Ok(closed?)
    }

    fn land_proposal(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        proposal: Option<&LandingProposal>,
    ) -> Result<LandOutcome, SourceError> {
        if !self.cas_land_enabled {
            return Err(SourceError::LandingDisabled);
        }
        // Adopt before anything else: a re-drained land entry (or a crash-and-
        // replay) must find the proposal it already opened rather than opening a
        // second one. This is what makes issuing a land idempotent, and it runs
        // before any write so a redrive touches nothing.
        //
        // Ahead of the base check, deliberately. The base check decides whether to
        // *open* a landing; once one is open its fate belongs to the proposal, and
        // re-deciding it here would abandon the bloom the moment mainline moved —
        // including when it moved *because this very proposal merged*, which is
        // the one outcome the watch exists to observe.
        let branch = landing_branch(bloom);
        if let Some(existing) = self.client.find_pull_request_for_head(&branch)? {
            return Ok(LandOutcome::Proposed { number: existing.number });
        }

        // Reverse-resolve the real mainline object (a sha1/40-hex on a real repo,
        // which the old fixed-64-hex gate rejected as `Malformed` before any swap)
        // to the base digest, then compare against the sealed base.
        let actual = self.mainline_digest(&self.mainline_head()?.sha)?;

        if actual != *expected_base {
            return Ok(LandOutcome::BaseMoved { expected: *expected_base, actual });
        }

        // Forward-resolve the new head digest to its real git object, point the
        // bloom's landing branch at it, and propose that branch onto mainline.
        self.point_landing_branch(bloom, &self.resolve_git_sha(new_head, "land new head digest")?)?;
        let (title, body) = render_landing_proposal(bloom, expected_base, new_head, &self.mainline, proposal);
        let opening = NewPullRequest { title, body, head: branch.clone(), base: self.mainline.branch().to_owned() };
        match self.client.create_pull_request(&opening) {
            Ok(opened) => Ok(LandOutcome::Proposed { number: opened.number }),
            // A 422 is the duplicate-head refusal: something opened a proposal
            // for this branch between our lookup and our create. Re-read and
            // adopt it — the same idempotent answer the lookup above gives, one
            // race later. A 422 with still no proposal to adopt is a genuine
            // refusal (an empty diff, a missing base) and propagates.
            Err(GithubError::Status { status: 422, body }) => self
                .client
                .find_pull_request_for_head(&branch)?
                .map(|raced| LandOutcome::Proposed { number: raced.number })
                .ok_or(SourceError::Github(GithubError::Status { status: 422, body })),
            Err(error) => Err(SourceError::Github(error)),
        }
    }

    fn accept_land(
        &self,
        bloom: &BloomId,
        expected_base: &Digest,
        new_head: &Digest,
        number: u64,
    ) -> Result<LandAcceptance, SourceError> {
        // The kill switch first, before any read and long before the write. An
        // acceptance that consulted the gate only on the way past would be a
        // way to land with `cas_land_enabled` off — the one thing the switch
        // exists to make impossible.
        if !self.cas_land_enabled {
            return Err(SourceError::LandingDisabled);
        }

        let Some(pull) = self.client.get_pull_request(number)? else {
            return Ok(refused_drift(format!("proposal #{number} is gone")));
        };
        // Already merged — by an operator, or by this very call on a pass whose
        // observation was lost. Idempotent rather than a refusal: re-pressing a
        // button that is already pressed is a no-op, and calling it drift would
        // reject a bloom for having landed.
        if pull.merged {
            return Ok(LandAcceptance::Accepted);
        }
        if pull.state != PullRequestState::Open {
            return Ok(refused_drift(format!("proposal #{number} is closed without having merged")));
        }
        // Only a landing this coordinator opened is accepted here, and only
        // onto its own mainline. The pair is the identity check: a human-flow
        // pull request proposes from a branch that is not this bloom's landing
        // branch, so it can never be what a number resolves to here, and a
        // landing aimed somewhere other than mainline is not the landing that
        // was proposed whatever branch it came from.
        let branch = landing_branch(bloom);
        if strip_heads(&pull.head_ref) != branch {
            return Ok(refused_drift(format!(
                "proposal #{number} proposes `{}`, not this bloom's landing branch `{branch}`",
                pull.head_ref,
            )));
        }
        if strip_heads(&pull.base) != self.mainline.branch() {
            return Ok(refused_drift(format!(
                "proposal #{number} aims at `{}`, not `{}`",
                pull.base,
                self.mainline.branch(),
            )));
        }

        // The head the bloom proved. A proposal whose head is anything else has
        // gained commits nobody verified — the one shape a merge must never
        // wave through, because every gate upstream judged the other tree.
        let proven = self.resolve_git_sha(new_head, "landing acceptance head digest")?;
        if pull.head_sha != proven {
            return Ok(refused_drift(format!(
                "proposal #{number} is at `{}`, not the proven head `{proven}`",
                pull.head_sha,
            )));
        }

        // The same compare-and-swap `land_proposal` opened under, re-asked at
        // the moment of the write. `land_proposal` deliberately does not
        // re-decide a base once a proposal is open — an open proposal's fate
        // belongs to the proposal — but a merge *is* the write that base guards,
        // and mainline can have moved in the ticks between opening and green.
        let actual = self.mainline_digest(&self.mainline_head()?.sha)?;
        if actual != *expected_base {
            return Ok(LandAcceptance::Refused(LandingRefusal::BaseMoved { expected: *expected_base, actual }));
        }

        // Green, and only green. `Absent` reads as not-yet — a gate that has not
        // reported is not a gate that passed — so a proposal nothing ever checks
        // is left for a person rather than merged on an absence.
        if self.client.checks_for_ref(&pull.head_sha)? != ChecksState::Passed {
            return Ok(LandAcceptance::Pending);
        }

        match self.client.squash_merge_pull_request(number, &pull.head_sha)? {
            PullMergeResult::Merged { .. } => Ok(LandAcceptance::Accepted),
            PullMergeResult::Refused { status, detail } => {
                Ok(LandAcceptance::Refused(LandingRefusal::Merge { status, detail }))
            }
        }
    }

    fn prune_working_refs(&self, bloom: &BloomId) -> Result<usize, SourceError> {
        let prefix = Self::working_ref_prefix(bloom);
        let mut pruned = 0;
        for git_ref in self.client.list_matching_refs(&prefix)? {
            if !Self::is_reclaimable_working_ref(&git_ref.name, &prefix) {
                continue;
            }
            self.client.delete_ref(&git_ref.name)?;
            pruned += 1;
        }
        Ok(pruned)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::slice::from_ref;

    use crate::client::ChecksState;
    use crate::correspondence::GitObjectId;

    use aether_bloomery::{
        BackendObjectId, BloomId, Checkpoint, ClaimHolder, ClaimOutcome, ClaimRefKind, ClaimRefState,
        ClaimReleaseOutcome, Correspondence as DomainCorrespondence, CorrespondenceError, Digest, IntegrateOutcome,
        LandOutcome, LandProposal, SourceBackend, WorkpieceId,
    };

    use std::sync::Arc;

    use super::{
        ADMISSION_REF, EMPTY_TREE, GitSource, LandAcceptance, LandingRefusal, LandingSource, MainlineRef, SourceError,
        candidate_ref_name, landing_branch, member_checkpoint_ref_name, parse_bloom_line, render_claim_message,
        render_tombstone_message, to_hex,
    };
    use crate::client::{GitDataApi, PullRequestApi};
    use crate::short_hex;
    use crate::testing::FakeGithub;

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    // The Git-object view of a recorded correspondence — the same forward
    // resolution plus adapter-edge conversion the port itself performs, so an
    // assertion below can name the git object a digest resolves to.
    fn resolve_git(fake: &FakeGithub, digest: &Digest) -> Option<GitObjectId> {
        fake.resolve_backend_object(digest).unwrap().map(|object| GitObjectId::try_from(object).unwrap())
    }

    // A `GitSource` over `fake` as both its git-data client and its correspondence
    // store — one in-process double serves both seams, so a test seeds git objects
    // and their correspondences into the same fake. On the default mainline, which
    // is what `seeded` and every unconfigured deployment operate on.
    fn git_source(fake: &FakeGithub, cas_land_enabled: bool) -> GitSource<FakeGithub> {
        git_source_on(fake, cas_land_enabled, MainlineRef::default())
    }

    // The same double, pointed at an explicitly configured mainline (ADR-0186).
    fn git_source_on(fake: &FakeGithub, cas_land_enabled: bool, mainline: MainlineRef) -> GitSource<FakeGithub> {
        GitSource::new(fake.clone(), Arc::new(fake.clone()), cas_land_enabled, mainline)
    }

    fn bloom() -> BloomId {
        BloomId(digest(1))
    }

    fn bloom_id(seed: u8) -> BloomId {
        BloomId(digest(seed))
    }

    fn workpiece(name: &str) -> WorkpieceId {
        WorkpieceId(name.to_owned())
    }

    // The `heads/…`-form claim ref for a workpiece, the address the tests assert
    // holder/existence against.
    fn claim_ref(workpiece: &WorkpieceId) -> String {
        GitSource::<FakeGithub>::workpiece_claim_ref(workpiece)
    }

    // A fake seeded with a commit at `base_tree`, its mainline ref pointing at
    // that commit, and the bloom's integration namespace created on it.
    fn seeded() -> (FakeGithub, BloomId, Digest) {
        let fake = FakeGithub::new();
        let base_tree = digest(10);
        // `seed_base_commit` records the base head ↔ commit and tree ↔ tree-object
        // correspondences the mainline paths resolve through, and points mainline
        // at the base commit.
        let base = fake.seed_base_commit(&base_tree);
        fake.seed_ref_at("heads/main", &base);
        let source = git_source(&fake, false);
        source.create_namespace(&bloom(), &base).unwrap();
        (fake, bloom(), base)
    }

    #[test]
    fn parse_bloom_line_round_trips_a_held_and_a_tombstoned_message_and_rejects_a_garbled_one() {
        let held = bloom_id(42);
        assert_eq!(parse_bloom_line(&render_claim_message(&held)).unwrap(), ClaimHolder::Held(held));
        assert_eq!(parse_bloom_line(&render_tombstone_message()).unwrap(), ClaimHolder::Tombstoned);

        match parse_bloom_line("no bloom-id line here") {
            Err(SourceError::Malformed(_)) => {}
            other => panic!("expected Malformed for a message with no Bloom-Id line, got {other:?}"),
        }
        match parse_bloom_line("bloomery claim\n\nBloom-Id: not-a-real-id") {
            Err(SourceError::Malformed(_)) => {}
            other => panic!("expected Malformed for a garbled Bloom-Id value, got {other:?}"),
        }
    }

    #[test]
    fn snapshot_is_stable_for_a_base() {
        let fake = FakeGithub::new();
        let tree = digest(7);
        let base = fake.seed_base_commit(&tree);
        let source = git_source(&fake, false);

        let first = source.snapshot(&base).unwrap();
        let second = source.snapshot(&base).unwrap();
        assert_eq!(first, second, "a base snapshots to a stable digest");
        assert_eq!(first.tree, tree);
        assert_eq!(first.head, base);
    }

    #[test]
    fn prune_working_refs_deletes_candidate_integration_and_checkpoint_and_spares_the_rest() {
        // Tripwire: a terminal bloom's working refs must go, and the claim
        // registry plus the landing branch must not. A prune that walked the
        // whole `heads/bloom/<short>/` prefix without filtering would delete
        // a still-open landing proposal; one that walked `bloomery/` would
        // clobber ADR-0150 claim refs another instance still holds.
        let (fake, bloom, _base) = seeded();
        let source = git_source(&fake, false);
        let candidate = candidate_ref_name(&bloom, "wp-0").trim_start_matches("refs/").to_owned();
        let member_checkpoint = member_checkpoint_ref_name(&bloom, "wp-0").trim_start_matches("refs/").to_owned();
        fake.seed_ref(&candidate, "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        fake.seed_ref(&member_checkpoint, "ffffffffffffffffffffffffffffffffffffffff");
        fake.seed_ref(
            &GitSource::<FakeGithub>::checkpoint_ref(&bloom, &digest(3)),
            "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
        );
        fake.seed_ref(&GitSource::<FakeGithub>::landing_ref(&bloom), "cccccccccccccccccccccccccccccccccccccccc");
        fake.seed_ref(ADMISSION_REF, "dddddddddddddddddddddddddddddddddddddddd");
        fake.seed_ref(&claim_ref(&workpiece("wp-live")), "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee");

        let pruned = source.prune_working_refs(&bloom).unwrap();
        assert_eq!(pruned, 4, "integration (from create_namespace) + candidate + checkpoint + member-checkpoint");
        assert!(!fake.ref_exists(&GitSource::<FakeGithub>::integration_ref(&bloom)));
        assert!(!fake.ref_exists(&candidate));
        assert!(!fake.ref_exists(&member_checkpoint));
        assert!(!fake.ref_exists(&GitSource::<FakeGithub>::checkpoint_ref(&bloom, &digest(3))));
        assert!(fake.ref_exists(&GitSource::<FakeGithub>::landing_ref(&bloom)), "the landing branch stays");
        assert!(fake.ref_exists(&GitSource::<FakeGithub>::attempt_ref(&bloom, 1)), "attempt refs stay");
        assert!(fake.ref_exists(ADMISSION_REF), "the admission claim ref stays");
        assert!(fake.ref_exists(&claim_ref(&workpiece("wp-live"))), "a workpiece claim ref stays");
    }

    #[test]
    fn is_reclaimable_working_ref_matches_the_member_checkpoint_prefix() {
        // Tripwire: a member-checkpoint ref has to leave with the rest of a
        // terminal bloom's namespace. A matcher that only knew candidate /
        // integration / checkpoint would leak these past the janitor. A live
        // bloom never calls prune, so its member checkpoints stay by that
        // policy, not by a special-case exemption here.
        let prefix = "heads/bloom/aa/";
        assert!(GitSource::<FakeGithub>::is_reclaimable_working_ref("heads/bloom/aa/member-checkpoint/wp-0", prefix));
        assert!(GitSource::<FakeGithub>::is_reclaimable_working_ref("heads/bloom/aa/candidate/wp-0", prefix));
        assert!(GitSource::<FakeGithub>::is_reclaimable_working_ref("heads/bloom/aa/checkpoint/00", prefix));
        assert!(GitSource::<FakeGithub>::is_reclaimable_working_ref("heads/bloom/aa/integration", prefix));
        assert!(
            !GitSource::<FakeGithub>::is_reclaimable_working_ref("heads/bloom/aa/landing", prefix),
            "the landing branch is not this sweep's",
        );
        assert!(
            !GitSource::<FakeGithub>::is_reclaimable_working_ref("heads/bloom/aa/attempt/1", prefix),
            "attempt refs stay",
        );
    }

    #[test]
    fn checkpoints_enumerates_only_integration_checkpoints_when_member_checkpoints_exist() {
        // Tripwire: member-checkpoint refs are a sibling of checkpoint/, never
        // nested under it. `checkpoints` strips `checkpoint/` and requires the
        // remainder to be a hex tree digest; a nested name would hard-error
        // Malformed and break successor reuse.
        let (fake, bloom, base) = seeded();
        let source = git_source(&fake, false);
        let base_tree = source.snapshot(&base).unwrap().tree;
        source.checkpoint(&bloom, &base_tree).unwrap();
        fake.seed_ref(
            member_checkpoint_ref_name(&bloom, "wp-0").trim_start_matches("refs/"),
            "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        );

        let listed = source.checkpoints(&bloom).unwrap();
        assert_eq!(listed, vec![Checkpoint { bloom, tree: base_tree }]);
    }

    #[test]
    fn member_checkpoint_ref_name_shares_the_candidate_sanitizer() {
        // The two spellings go through one sanitizer so a workpiece that is
        // ref-safe on the candidate branch is ref-safe on the checkpoint, and
        // a slash cannot open a nested path under either prefix.
        let bloom = bloom();
        let candidate = candidate_ref_name(&bloom, "wp/cand");
        let checkpoint = member_checkpoint_ref_name(&bloom, "wp/cand");
        assert!(candidate.ends_with("/candidate/wp-cand"));
        assert!(checkpoint.ends_with("/member-checkpoint/wp-cand"));
        assert!(checkpoint.contains("/member-checkpoint/"), "the prefix is a sibling of checkpoint/, not nested");
        assert!(!checkpoint.contains("/checkpoint/member"), "nesting under checkpoint/ would break enumeration");
    }

    #[test]
    fn create_namespace_writes_integration_and_attempt_refs() {
        let (fake, bloom, _base) = seeded();
        assert!(fake.ref_exists(&GitSource::<FakeGithub>::integration_ref(&bloom)), "integration branch created");
        assert!(fake.ref_exists(&GitSource::<FakeGithub>::attempt_ref(&bloom, 1)), "attempt handle created");
    }

    #[test]
    fn checkpoint_create_enumerate_and_reuse_across_a_successor() {
        let (fake, bloom, base) = seeded();
        let source = git_source(&fake, false);
        // The base tree is the integration branch's current tree.
        let base_tree = source.snapshot(&base).unwrap().tree;

        let checkpoint = source.checkpoint(&bloom, &base_tree).unwrap();
        assert_eq!(checkpoint, Checkpoint { bloom, tree: base_tree });

        // Enumerable.
        let listed = source.checkpoints(&bloom).unwrap();
        assert_eq!(listed, vec![Checkpoint { bloom, tree: base_tree }]);

        // Reusable across a successor: the successor matches the checkpoint by
        // digest without re-recording it.
        let successor = source.checkpoints(&bloom).unwrap();
        assert!(successor.iter().any(|c| c.tree == base_tree), "the successor reuses the checkpoint by digest");

        // Idempotent: re-recording the same tree does not add a second ref.
        source.checkpoint(&bloom, &base_tree).unwrap();
        assert_eq!(source.checkpoints(&bloom).unwrap().len(), 1);
    }

    // Seed a candidate branch at its own commit and return its `heads/…` ref —
    // what a member's capture pushes (ADR-0152) and what a merge fold reads.
    fn seed_candidate_ref(fake: &FakeGithub, workpiece: &str, tree: &str) -> String {
        let commit = fake.create_commit(workpiece, tree, &[]).unwrap();
        let name = format!("heads/bloom/cand/{workpiece}");
        fake.seed_ref(&name, &commit.sha);
        name
    }

    #[test]
    fn a_merge_fold_keeps_what_the_branch_already_carried_and_names_the_result() {
        // The whole reason this verb exists. `integrate` sets the branch to the
        // candidate's tree, so folding a second member — or a candidate built
        // before the branch moved — reverts the first one's work with a clean
        // commit and no error. A merge's result must be neither input.
        //
        // And the result must be *nameable*: a merged tree is new to the
        // correspondence (unlike a tree-replace, whose result is the
        // candidate's own recorded tree), so failing to record it would leave
        // the branch unreachable to the next snapshot.
        let (fake, bloom, base) = seeded();
        let source = git_source(&fake, false);
        let base_tree = source.snapshot(&base).unwrap().tree;
        let expected = source.checkpoint(&bloom, &base_tree).unwrap();

        let first = seed_candidate_ref(&fake, "wp-a", "tree-a");
        let IntegrateOutcome::Integrated { tree: after_first, head } =
            source.integrate_merge(&bloom, &first, &expected).unwrap()
        else {
            panic!("the first member folds");
        };
        assert_ne!(after_first, base_tree, "the fold advanced the branch off the base");
        assert!(
            resolve_git(&fake, &after_first).is_some(),
            "the merged tree is recorded, so the next snapshot can reverse-resolve the branch",
        );
        assert_ne!(head, after_first, "the landable head stays a distinct digest from the artifact tree");

        // The second member folds onto the branch the first one left, and the
        // result carries both. Under tree-replace this tree would equal the
        // second candidate's and the first member's work would be gone.
        let second = seed_candidate_ref(&fake, "wp-b", "tree-b");
        let checkpoint = Checkpoint { bloom, tree: after_first };
        let IntegrateOutcome::Integrated { tree: after_second, .. } =
            source.integrate_merge(&bloom, &second, &checkpoint).unwrap()
        else {
            panic!("the second member folds onto the first");
        };
        assert_ne!(after_second, after_first, "the second fold advanced the branch again");
        assert_ne!(after_second, base_tree, "and did not rewind it to the base");
    }

    #[test]
    fn a_merge_fold_separates_a_conflict_a_replay_and_a_stale_checkpoint() {
        // Three non-advancing answers a fold must tell apart: a collision is an
        // owner decision, a member already folded must let the fold move past it
        // rather than stall, and a branch that moved past the checkpoint is the
        // single-writer refusal.
        let (fake, bloom, base) = seeded();
        let source = git_source(&fake, false);
        let base_tree = source.snapshot(&base).unwrap().tree;
        let expected = source.checkpoint(&bloom, &base_tree).unwrap();

        let conflicting = seed_candidate_ref(&fake, "wp-clash", "tree-clash");
        fake.seed_merge_conflict(&format!("bloom/{}/integration", short_hex(&bloom.0)), "bloom/cand/wp-clash");
        assert!(
            matches!(
                source.integrate_merge(&bloom, &conflicting, &expected).unwrap(),
                IntegrateOutcome::Conflict { .. }
            ),
            "a collision is a clean outcome, not a transport error to retry",
        );

        // A member already on the branch: the fold re-offers it after a restart
        // and must be told where the branch stands, not that it failed.
        let folded = seed_candidate_ref(&fake, "wp-done", "tree-done");
        let IntegrateOutcome::Integrated { tree: advanced, head } =
            source.integrate_merge(&bloom, &folded, &expected).unwrap()
        else {
            panic!("the member folds the first time");
        };
        let replayed = source.integrate_merge(&bloom, &folded, &Checkpoint { bloom, tree: advanced }).unwrap();
        assert_eq!(
            replayed,
            IntegrateOutcome::Integrated { tree: advanced, head },
            "re-folding a member the branch already carries reports its position unchanged",
        );

        // The original checkpoint is now stale — the branch advanced past it.
        let IntegrateOutcome::StaleCheckpoint { actual } = source.integrate_merge(&bloom, &folded, &expected).unwrap()
        else {
            panic!("a checkpoint the branch has passed is refused");
        };
        assert_eq!(actual, advanced, "the refusal reports where the branch actually is");
    }

    #[test]
    fn integrate_resolves_the_candidate_tree_and_rejects_a_stale_checkpoint() {
        let (fake, bloom, base) = seeded();
        let source = git_source(&fake, false);
        let base_tree = source.snapshot(&base).unwrap().tree;
        let expected = source.checkpoint(&bloom, &base_tree).unwrap();

        // The candidate tree must have a recorded git-object correspondence
        // (materialized elsewhere); integrate resolves it for the commit's tree.
        let candidate = digest(50);
        fake.seed_git_object(&candidate);
        let outcome = source.integrate(&bloom, &candidate, &expected).unwrap();
        let IntegrateOutcome::Integrated { tree, head } = outcome else {
            panic!("expected Integrated, got {outcome:?}");
        };
        assert_eq!(tree, candidate, "the integrated tree is the candidate tree");
        // The head digest is distinct from the tree digest and resolves to the
        // produced commit, while the tree digest still resolves to its own
        // object — recording `head ↔ commit` does not clobber `tree ↔ tree`
        // (issue #3615).
        assert_ne!(head, tree, "the integrated head is a distinct digest from the artifact tree");
        let head_object = resolve_git(&fake, &head).expect("the integrated head resolves to the produced commit");
        let tree_object = resolve_git(&fake, &tree).expect("the artifact tree still resolves to its own git object");
        assert_ne!(head_object, tree_object, "head and tree resolve to distinct git objects — no clobber");

        // The branch has advanced; the same (now stale) checkpoint is refused,
        // reverse-resolving the advanced branch's tree back to the candidate.
        let another = digest(60);
        fake.seed_git_object(&another);
        let stale = source.integrate(&bloom, &another, &expected).unwrap();
        assert_eq!(stale, IntegrateOutcome::StaleCheckpoint { actual: candidate });
    }

    /// A correspondence whose reads work but whose `record` always faults,
    /// standing in for the durable store failing mid-integrate.
    struct RecordFaults(FakeGithub);

    impl DomainCorrespondence for RecordFaults {
        fn record(&self, _digest: &Digest, _object: &BackendObjectId) -> Result<(), CorrespondenceError> {
            Err(CorrespondenceError::new("store fault"))
        }

        fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError> {
            DomainCorrespondence::resolve_backend_object(&self.0, digest)
        }

        fn resolve_digest(&self, object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError> {
            DomainCorrespondence::resolve_digest(&self.0, object)
        }
    }

    #[test]
    fn a_correspondence_fault_leaves_the_integration_ref_re_integratable() {
        // Tripwire: the correspondence record must precede the ref advance
        // (#3667). Recording after would move the branch to the candidate tree
        // and *then* fault, so the retry below reads `current_tree == candidate`
        // and returns StaleCheckpoint forever — the head never landable and
        // never re-integratable. The ref staying put is what keeps the retry a
        // retry.
        let (fake, bloom, base) = seeded();
        let base_tree = git_source(&fake, false).snapshot(&base).unwrap().tree;
        let expected = git_source(&fake, false).checkpoint(&bloom, &base_tree).unwrap();
        let candidate = digest(50);
        fake.seed_git_object(&candidate);

        let faulting =
            GitSource::new(fake.clone(), Arc::new(RecordFaults(fake.clone())), false, MainlineRef::default());
        assert!(faulting.integrate(&bloom, &candidate, &expected).is_err(), "the store fault surfaces");

        // The retry runs against a working store and still sees its own
        // checkpoint, so it integrates rather than refusing as stale.
        let outcome = git_source(&fake, false).integrate(&bloom, &candidate, &expected).unwrap();
        let IntegrateOutcome::Integrated { head, .. } = outcome else {
            panic!("the retry must integrate, got {outcome:?}");
        };
        assert!(
            resolve_git(&fake, &head).is_some(),
            "the retried head resolves — the state the fault left is recoverable",
        );
    }

    #[test]
    fn observing_a_head_bloomery_already_named_returns_that_same_digest() {
        // Tripwire: the observation reverse-resolves before it mints (#4667).
        // `seeded` records the base head ↔ commit correspondence and points
        // mainline at that commit, so the live head is one a digest already
        // names. Minting a *second* digest for it would make every observation
        // report a head mainline is not at, so the reducer would "advance"
        // mainline onto a fresh digest for the commit it was already sitting on —
        // an endless false advance, once per boot.
        let (fake, _, base) = seeded();

        let observed = git_source(&fake, false).observe_mainline_head().unwrap();

        assert_eq!(observed, base, "the observation returns the digest already naming the head, not a fresh mint");
    }

    #[test]
    fn observing_a_foreign_head_mints_a_digest_and_records_it() {
        // Tripwire: a head merged by anyone else has no digest, so one is minted
        // *and recorded*. Without the record the observed head would be a digest
        // nothing can forward-resolve, and the next snapshot against the advanced
        // mainline would fault `UnresolvedCorrespondence` — the coordinator
        // wedged on a base it named itself.
        let (fake, _, base) = seeded();
        let tree_sha = resolve_git(&fake, &digest(10)).unwrap().to_hex();
        let foreign = fake.seed_commit_with_message("someone else's merge", &tree_sha);
        fake.seed_ref("heads/main", &foreign);

        let observed = git_source(&fake, false).observe_mainline_head().unwrap();

        assert_ne!(observed, base, "a moved head is a different digest from the one mainline sat at");
        assert_eq!(
            resolve_git(&fake, &observed).expect("the minted digest was recorded").to_hex(),
            foreign,
            "and it forward-resolves to the commit that was actually observed",
        );
    }

    #[test]
    fn a_descendant_is_a_fast_forward_and_an_ancestor_or_unrelated_head_is_not() {
        // Tripwire: the observation door classifies against git ancestry
        // (#4938). Equal and genesis are fast-forwards without a round-trip;
        // a child of the current tip is one; walking the other way, or
        // naming a commit off the line, is not.
        let (fake, _, base) = seeded();
        let source = git_source(&fake, false);
        let base_sha = resolve_git(&fake, &base).unwrap().to_hex();

        assert!(source.is_fast_forward(&base, &base).unwrap(), "equal digests are a fast-forward");
        assert!(
            source.is_fast_forward(&aether_bloomery::Snapshot::GENESIS_MAINLINE, &base).unwrap(),
            "the genesis sentinel is the boot bind, not an ancestry question",
        );

        let child_sha =
            fake.create_commit("forward", &to_hex(&digest(10)), &[base_sha]).expect("the child commit mints").sha;
        fake.seed_correspondence(&digest(11), &child_sha);

        assert!(source.is_fast_forward(&base, &digest(11)).unwrap(), "a descendant of mainline is a fast-forward");
        assert!(!source.is_fast_forward(&digest(11), &base).unwrap(), "an ancestor of mainline is not");

        let side = fake.seed_commit_with_message("sideways", &to_hex(&digest(12)));
        fake.seed_correspondence(&digest(13), &side);
        assert!(
            !source.is_fast_forward(&base, &digest(13)).unwrap(),
            "an unrelated commit is sideways, not a fast-forward"
        );
    }

    #[test]
    fn integrate_errors_cleanly_when_the_candidate_tree_is_unrecorded() {
        // A candidate whose tree was never materialized into git (no
        // correspondence) is the clean typed `UnresolvedCorrespondence` — never a
        // `Malformed` or a hex-punned sha git cannot resolve (ADR-0150 boundary).
        let (fake, bloom, base) = seeded();
        let source = git_source(&fake, false);
        let base_tree = source.snapshot(&base).unwrap().tree;
        let expected = source.checkpoint(&bloom, &base_tree).unwrap();

        match source.integrate(&bloom, &digest(77), &expected) {
            Err(SourceError::UnresolvedCorrespondence(_)) => {}
            other => panic!("expected UnresolvedCorrespondence, got {other:?}"),
        }
    }

    #[test]
    fn land_proposes_the_resolved_head_and_never_writes_mainline() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        // A real sha1 (40-hex) mainline — the object format the reported failure
        // (#3590) hit, which the old fixed-64-hex `digest_from_hex` gate rejected
        // as `Malformed` before any read. The correspondence resolves it.
        let base = digest(10);
        let mainline_sha1 = "a1".repeat(20);
        assert_eq!(mainline_sha1.len(), 40, "a sha1 object id is 40 hex");
        fake.seed_ref("heads/main", &mainline_sha1);
        fake.seed_correspondence(&base, &mainline_sha1);
        let new_head = digest(90);
        fake.seed_git_object(&new_head);

        // Gated off (the kill switch): a typed refusal, before any write.
        let gated = git_source(&fake, false);
        match gated.land(&bloom, &base, &new_head) {
            Err(SourceError::LandingDisabled) => {}
            other => panic!("expected LandingDisabled, got {other:?}"),
        }

        // Enabled: reverse-resolve the sha1 mainline to the base, point the
        // landing branch at the resolved head, and propose it.
        let enabled = git_source(&fake, true);
        let number = match enabled.land(&bloom, &base, &new_head).unwrap() {
            LandOutcome::Proposed { number } => number,
            other @ LandOutcome::BaseMoved { .. } => panic!("expected Proposed, got {other:?}"),
        };
        assert_eq!(
            fake.ref_target(&format!("heads/{}", landing_branch(&bloom))),
            Some(to_hex(&new_head)),
            "the landing branch points at the resolved head",
        );
        // Tripwire: mainline is protected, so `land` must never write it. A slip
        // back to the direct compare-and-swap would 403 against the real repo —
        // a failure no fake-backed test would otherwise catch.
        assert_eq!(fake.ref_target("heads/main"), Some(mainline_sha1), "land never writes mainline");
        assert!(fake.get_pull_request(number).unwrap().is_some(), "the proposal exists");
    }

    // Tripwire: a repoint moves every mainline read at once (ADR-0186). The
    // observation, the compare-and-swap base check, and the proposal's base are
    // three separate reads of the same ref, and a slip back to `main` on any one
    // of them is silent: the observation would name a head the day branch never
    // held, the compare would refuse a base that had not moved, and the proposal
    // would aim yesterday's landing at the branch bloomery no longer owns. So
    // `main` is seeded here too, at a different commit, and every assertion is
    // one the default ref would fail.
    #[test]
    fn a_repointed_mainline_observes_compares_and_proposes_against_the_configured_ref() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let day = MainlineRef::new("refs/heads/bloomery/daily/2026-08-13");
        let base = digest(10);
        let day_sha = "a1".repeat(20);
        fake.seed_ref(day.git_ref(), &day_sha);
        fake.seed_correspondence(&base, &day_sha);
        fake.seed_ref("heads/main", &"b2".repeat(20));
        let new_head = digest(90);
        fake.seed_git_object(&new_head);

        let source = git_source_on(&fake, true, day.clone());

        assert_eq!(source.mainline_head_sha().unwrap(), day_sha, "the observation reads the configured ref");
        assert_eq!(source.observe_mainline_head().unwrap(), base, "and names the base the day branch sits at");

        let LandOutcome::Proposed { number } = source.land(&bloom, &base, &new_head).unwrap() else {
            panic!("the sealed base is what the day branch holds, so the land proposes");
        };
        assert_eq!(
            fake.get_pull_request(number).unwrap().expect("the proposal exists").base,
            day.branch(),
            "the landing is proposed onto the day branch",
        );
        let (_, body) = fake.pull_request_proposal(number).expect("the proposal's prose is recorded");
        assert!(body.contains(&day.to_string()), "the provenance footer names the ref it lands onto: {body}");
    }

    // Tripwire: a mainline ref that is not there names *itself* in the refusal.
    // An operator who repoints at a branch they have not cut yet reads this
    // message, and a stale `heads/main` in it would send them looking at a ref
    // that is present and fine.
    #[test]
    fn an_absent_configured_mainline_names_the_ref_it_looked_for() {
        let fake = FakeGithub::new();
        let day = MainlineRef::new("refs/heads/bloomery/daily/2026-08-13");
        fake.seed_ref("heads/main", &"b2".repeat(20));

        match git_source_on(&fake, true, day.clone()).mainline_head_sha() {
            Err(SourceError::MissingRef(name)) => assert_eq!(name, day.git_ref()),
            other => panic!("expected MissingRef naming the configured ref, got {other:?}"),
        }
    }

    // Tripwire: issuing a land is idempotent. The land outbox re-drains on any
    // transport fault and replays after a crash, so a `land` that proposed again
    // instead of adopting would open a fresh pull request every poll tick.
    #[test]
    fn land_adopts_the_proposal_it_already_opened() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(&base, &"a1".repeat(20));
        let new_head = digest(90);
        fake.seed_git_object(&new_head);
        let source = git_source(&fake, true);

        let first = source.land(&bloom, &base, &new_head).unwrap();
        let second = source.land(&bloom, &base, &new_head).unwrap();
        assert_eq!(first, second, "a re-issued land adopts the same proposal");
    }

    #[test]
    fn land_reports_base_moved_when_the_mainline_correspondence_mismatches() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let actual_base = digest(10);
        let mainline_sha1 = "b2".repeat(20);
        fake.seed_ref("heads/main", &mainline_sha1);
        fake.seed_correspondence(&actual_base, &mainline_sha1);
        let enabled = git_source(&fake, true);

        // A stale expected base is the clean BaseMoved refusal, carrying the base
        // the mainline object actually resolves to — and no proposal is opened.
        let stale_expected = digest(200);
        match enabled.land(&bloom, &stale_expected, &digest(91)).unwrap() {
            LandOutcome::BaseMoved { expected, actual } => {
                assert_eq!(expected, stale_expected);
                assert_eq!(actual, actual_base);
            }
            other @ LandOutcome::Proposed { .. } => panic!("expected BaseMoved, got {other:?}"),
        }
        assert_eq!(
            fake.find_pull_request_for_head(&landing_branch(&bloom)).unwrap(),
            None,
            "a moved base proposes nothing",
        );
    }

    // Tripwire: an unrecognized mainline head is a *moved base*, not a fault.
    // Anything merged outside a bloom leaves mainline on an object with no
    // recorded correspondence, and the land reactor re-drives a fault — so
    // raising one here spun the coordinator every tick on a condition no retry
    // could resolve, since nothing ever records a correspondence for a commit
    // made outside Bloomery. Minting the head's address is what `poll_land`
    // already does for a squash commit; it is what lets the refusal be clean.
    #[test]
    fn an_unrecognized_mainline_head_is_a_moved_base_not_a_fault() {
        let fake = FakeGithub::new();
        let foreign = "c3".repeat(20);
        fake.seed_ref("heads/main", &foreign);
        let enabled = git_source(&fake, true);

        let expected_base = digest(10);
        match enabled.land(&bloom(), &expected_base, &digest(90)).expect("an unknown head refuses, it does not fault") {
            LandOutcome::BaseMoved { expected, actual } => {
                assert_eq!(expected, expected_base);
                // The head is now nameable, so the refusal reports what mainline
                // actually is rather than declining to say.
                assert_eq!(
                    enabled
                        .correspondence
                        .resolve_digest(&BackendObjectId::from(GitObjectId::from_hex(&foreign).unwrap()))
                        .unwrap(),
                    Some(actual),
                    "the minted address is recorded, so the next check resolves it",
                );
            }
            other @ LandOutcome::Proposed { .. } => panic!("expected BaseMoved, got {other:?}"),
        }
    }

    // Tripwire: the adopt runs ahead of the base check. Mainline moving is the
    // *expected* consequence of a landing proposal being merged, so re-deciding
    // the base on a re-drive abandoned the bloom at precisely the moment its
    // landing succeeded — the watch could never observe the merge it was waiting
    // for, because observing it is what moved the base.
    #[test]
    fn an_open_proposal_is_adopted_even_after_mainline_moved_off_the_sealed_base() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        let mainline_sha1 = "a1".repeat(20);
        fake.seed_ref("heads/main", &mainline_sha1);
        fake.seed_correspondence(&base, &mainline_sha1);
        let new_head = digest(90);
        fake.seed_git_object(&new_head);
        let enabled = git_source(&fake, true);

        let LandOutcome::Proposed { number } = enabled.land(&bloom, &base, &new_head).unwrap() else {
            panic!("the first land opens the proposal");
        };

        // Mainline moves on — the merge of this very proposal is one way it does.
        fake.seed_ref("heads/main", &"d4".repeat(20));

        assert_eq!(
            enabled.land(&bloom, &base, &new_head).unwrap(),
            LandOutcome::Proposed { number },
            "the same proposal is re-adopted so the watch can still reach its terminal",
        );
    }

    // Tripwire: the receipt attests the commit mainline *became*, and records a
    // correspondence for it. A squash accept produces a commit that is not the
    // proposed head and that nothing on our side created, so re-deriving the
    // head from the proposal would attest a commit that is on no branch — and
    // leaving it unrecorded would break the next bloom's base check, which
    // reverse-resolves mainline through exactly this correspondence.
    // #4689 — an open proposal is only *waiting* while its gate might still
    // pass. A concluded-red gate means it can never merge, and reporting that as
    // `Open` is what left a bloom polling something nothing would accept.
    //
    // Tripwire on the pending side above all: reading a partial gate as failed
    // would tear a bloom's line open every time a slow check had not reported
    // yet, which is worse than the bug being fixed.
    #[test]
    fn an_open_proposal_is_only_open_while_its_checks_might_still_pass() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(&base, &"a1".repeat(20));
        let new_head = digest(90);
        fake.seed_git_object(&new_head);
        let source = git_source(&fake, true);

        let LandOutcome::Proposed { number } = source.land(&bloom, &base, &new_head).unwrap() else {
            panic!("expected Proposed");
        };
        let head_sha = fake.pull_request_head_sha(number).expect("the proposal has a head");

        // No check has reported: nothing to judge, so the watch keeps waiting.
        assert_eq!(source.poll_land(&bloom, &base, number).unwrap(), LandProposal::Open);

        // A check still running is not a verdict — a later one can still fail.
        fake.seed_checks(&head_sha, ChecksState::Pending);
        assert_eq!(source.poll_land(&bloom, &base, number).unwrap(), LandProposal::Open);

        fake.seed_checks(&head_sha, ChecksState::Passed);
        assert_eq!(source.poll_land(&bloom, &base, number).unwrap(), LandProposal::Open);

        // Red: terminal for the watch, carrying the names a repair is directed by.
        fake.seed_checks(&head_sha, ChecksState::Failed { failing: alloc_vec(&["Clippy", "Rustdoc"]) });
        assert_eq!(
            source.poll_land(&bloom, &base, number).unwrap(),
            LandProposal::ChecksFailed { failing: alloc_vec(&["Clippy", "Rustdoc"]) },
        );
    }

    fn alloc_vec(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn poll_land_reports_the_squash_commit_as_the_landed_head_and_records_it() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(&base, &"a1".repeat(20));
        let new_head = digest(90);
        fake.seed_git_object(&new_head);
        let source = git_source(&fake, true);

        let LandOutcome::Proposed { number } = source.land(&bloom, &base, &new_head).unwrap() else {
            panic!("expected Proposed");
        };
        assert_eq!(source.poll_land(&bloom, &base, number).unwrap(), LandProposal::Open, "an open proposal waits");

        // The operator squash-merges: mainline becomes a commit that is neither
        // the proposed head nor anything Bloomery created.
        let squashed = "5c".repeat(20);
        fake.merge_pull_request(number, &squashed);
        let landed = source.poll_land(&bloom, &base, number).unwrap();
        let LandProposal::Landed(receipt) = landed else {
            panic!("expected Landed, got {landed:?}")
        };
        assert_eq!(receipt.previous_base, base);
        assert_ne!(receipt.new_head, new_head, "the landed head is the squash commit, not the proposed head");
        assert_eq!(
            resolve_git(&fake, &receipt.new_head).map(|object| object.to_hex()),
            Some(squashed),
            "the landed head's correspondence is recorded, so the next bloom's base check resolves",
        );
    }

    #[test]
    fn poll_land_reports_declined_for_a_closed_or_vanished_proposal() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        let base = digest(10);
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(&base, &"a1".repeat(20));
        fake.seed_git_object(&digest(90));
        let source = git_source(&fake, true);

        let LandOutcome::Proposed { number } = source.land(&bloom, &base, &digest(90)).unwrap() else {
            panic!("expected Proposed");
        };
        fake.close_pull_request(number);
        assert_eq!(source.poll_land(&bloom, &base, number).unwrap(), LandProposal::Declined, "closed unmerged");

        // A number nothing answers is terminal too — otherwise the watch spins
        // forever on a proposal that will never resolve.
        assert_eq!(source.poll_land(&bloom, &base, 9999).unwrap(), LandProposal::Declined, "vanished");
    }

    // A proposal, its head sha, and an enabled port over `fake` — the state
    // every acceptance test starts from, since accepting is only ever asked of
    // a landing the port itself opened.
    fn proposed(fake: &FakeGithub, base: &Digest, new_head: &Digest) -> (GitSource<FakeGithub>, u64, String) {
        fake.seed_ref("heads/main", &"a1".repeat(20));
        fake.seed_correspondence(base, &"a1".repeat(20));
        fake.seed_git_object(new_head);
        let source = git_source(fake, true);
        let LandOutcome::Proposed { number } = source.land(&bloom(), base, new_head).unwrap() else {
            panic!("the port opens the proposal it is then asked to accept");
        };
        let head_sha = fake.pull_request_head_sha(number).expect("the proposal has a head");
        (source, number, head_sha)
    }

    // Acceptance 1: a green landing merges with nobody pressing anything. The
    // merge commit is what mainline became — the port never reports a landing
    // under the head it proposed, because a squash produces neither branch's
    // commit.
    #[test]
    fn a_green_proposal_is_accepted_and_mainline_becomes_the_squash_commit() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, head_sha) = proposed(&fake, &base, &new_head);
        fake.seed_checks(&head_sha, ChecksState::Passed);

        assert_eq!(source.accept_land(&bloom(), &base, &new_head, number).unwrap(), LandAcceptance::Accepted);
        assert_eq!(fake.pull_request_merged(number), Some(true), "the port merged the proposal it opened");

        let LandProposal::Landed(receipt) = source.poll_land(&bloom(), &base, number).unwrap() else {
            panic!("the accepted proposal reads as landed");
        };
        assert_ne!(receipt.new_head, new_head, "the receipt attests the squash commit, not the proposed head");
    }

    // Tripwire: the kill switch bounds the *merge*, not only the proposal.
    // `cas_land_enabled` off is the one control that must make a landing
    // impossible, and an acceptance that consulted it after opening — or not at
    // all — would be a second door into the same write.
    #[test]
    fn accepting_a_landing_refuses_while_the_land_gate_is_off() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (_, number, head_sha) = proposed(&fake, &base, &new_head);
        fake.seed_checks(&head_sha, ChecksState::Passed);

        match git_source(&fake, false).accept_land(&bloom(), &base, &new_head, number) {
            Err(SourceError::LandingDisabled) => {}
            other => panic!("expected LandingDisabled, got {other:?}"),
        }
        assert_eq!(fake.pull_request_merged(number), Some(false), "a gated-off port merges nothing");
    }

    // A gate that has not concluded green is not a gate that passed. `Absent`
    // above all: a proposal nothing has checked yet reads exactly like one
    // everything has passed if the port asks only "did anything fail".
    #[test]
    fn a_proposal_whose_gate_has_not_gone_green_is_not_accepted() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, head_sha) = proposed(&fake, &base, &new_head);

        for state in [ChecksState::Absent, ChecksState::Pending] {
            fake.seed_checks(&head_sha, state.clone());
            assert_eq!(
                source.accept_land(&bloom(), &base, &new_head, number).unwrap(),
                LandAcceptance::Pending,
                "a {state:?} gate leaves the proposal for a later pass",
            );
        }
        assert_eq!(fake.pull_request_merged(number), Some(false), "nothing merged on an unconcluded gate");
    }

    // Acceptance 2: a proposal that gained a commit nobody proved is refused,
    // not merged. Every gate upstream judged the head the bloom resolved on, so
    // a merge that waved a different tree through would land work under a proof
    // that was never about it.
    #[test]
    fn a_proposal_whose_head_moved_off_the_proven_one_is_refused() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);

        let pushed = "ee".repeat(20);
        fake.push_to_pull_request(number, &pushed);
        fake.seed_checks(&pushed, ChecksState::Passed);

        match source.accept_land(&bloom(), &base, &new_head, number).unwrap() {
            LandAcceptance::Refused(LandingRefusal::Drifted { detail }) => {
                assert!(detail.contains(&pushed), "the refusal names the head it found: {detail}");
            }
            other => panic!("expected a drift refusal, got {other:?}"),
        }
        assert_eq!(fake.pull_request_merged(number), Some(false), "a drifted proposal is not merged");
    }

    // Acceptance 2, the other axis. `land` deliberately stops re-deciding the
    // base once a proposal is open, so without this check the acceptance would
    // be the one write in the landing that no base guard covers — and the
    // window is real: a proposal sits open for as many ticks as its gate takes.
    #[test]
    fn a_base_that_moved_after_the_proposal_opened_refuses_the_merge() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, head_sha) = proposed(&fake, &base, &new_head);
        fake.seed_checks(&head_sha, ChecksState::Passed);

        let moved = digest(77);
        fake.seed_ref("heads/main", &"d4".repeat(20));
        fake.seed_correspondence(&moved, &"d4".repeat(20));

        match source.accept_land(&bloom(), &base, &new_head, number).unwrap() {
            LandAcceptance::Refused(LandingRefusal::BaseMoved { expected, actual }) => {
                assert_eq!(expected, base);
                assert_eq!(actual, moved, "the refusal names where mainline actually stands");
            }
            other => panic!("expected a base-moved refusal, got {other:?}"),
        }
        assert_eq!(fake.pull_request_merged(number), Some(false), "a moved base is not landed onto");
    }

    // A proposal a person merged out from under the coordinator is not a
    // refusal — the button it would press is already pressed. Calling that
    // drift would reject a bloom for having landed.
    #[test]
    fn accepting_an_already_merged_proposal_is_the_idempotent_no_op() {
        let fake = FakeGithub::new();
        let (base, new_head) = (digest(10), digest(90));
        let (source, number, _) = proposed(&fake, &base, &new_head);
        fake.merge_pull_request(number, &"5c".repeat(20));

        assert_eq!(source.accept_land(&bloom(), &base, &new_head, number).unwrap(), LandAcceptance::Accepted);
    }

    #[test]
    fn claim_seal_acquires_every_member_and_the_admission_ref() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let claimant = bloom_id(1);
        let (w1, w2) = (workpiece("wp-1"), workpiece("wp-2"));

        let outcome = source.claim_seal(&claimant, &[w1.clone(), w2.clone()]).unwrap();
        assert_eq!(outcome, ClaimOutcome::Acquired);

        // Each member claim ref and the single admission ref resolves to the
        // claiming bloom, from the claim commit's message — never its tree,
        // which is always the well-known empty tree (a real per-claim tree is
        // what 500s against a real GitHub repo).
        for name in [claim_ref(&w1), claim_ref(&w2), ADMISSION_REF.to_owned()] {
            assert_eq!(source.claim_holder(&name).unwrap(), Some(claimant), "{name} held by the claimant");
            let sha = fake.ref_target(&name).unwrap();
            let commit = source.client().get_commit(&sha).unwrap();
            assert_eq!(commit.tree, EMPTY_TREE, "{name}'s claim commit points at the empty tree, not a real one");
        }
    }

    #[test]
    fn claim_seal_workpiece_conflict_reports_the_ref_and_holder() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (holder, contender) = (bloom_id(1), bloom_id(2));
        let w1 = workpiece("wp-1");
        source.claim_seal(&holder, from_ref(&w1)).unwrap();

        // The contender's acquire hits the held member ref first — reported as
        // the conflict, naming the workpiece and its holder.
        let outcome = source.claim_seal(&contender, from_ref(&w1)).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(w1), held_by: holder });
    }

    #[test]
    fn claim_seal_admission_conflict_reports_the_admission_ref() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (holder, contender) = (bloom_id(1), bloom_id(2));
        // The holder takes the admission ref (empty member set → admission only).
        source.claim_seal(&holder, &[]).unwrap();

        // A member-carrying contender clears its members, then hits the held
        // admission ref last — reported as the conflict on the admission ref.
        let outcome = source.claim_seal(&contender, &[workpiece("wp-1")]).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::MainlineAdmission, held_by: holder });
    }

    #[test]
    fn claim_seal_rolls_back_every_member_it_created_on_a_conflict() {
        // Tripwire: an aborted acquire leaves no partial claim — every member ref
        // it created before the conflict is deleted, not just some, so a later
        // acquire on those members is not spuriously blocked.
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (holder, contender) = (bloom_id(1), bloom_id(2));
        source.claim_seal(&holder, &[]).unwrap(); // holder owns the admission ref

        let members = [workpiece("wp-1"), workpiece("wp-2"), workpiece("wp-3")];
        let outcome = source.claim_seal(&contender, &members).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::MainlineAdmission, held_by: holder });
        for member in &members {
            assert!(!fake.ref_exists(&claim_ref(member)), "{} rolled back", member.0);
        }
    }

    #[test]
    fn transfer_seal_fast_forwards_carried_refs_and_admission_to_the_successor() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (predecessor, successor) = (bloom_id(1), bloom_id(2));
        let w1 = workpiece("wp-1");
        source.claim_seal(&predecessor, from_ref(&w1)).unwrap();

        let outcome = source.transfer_seal(&predecessor, &successor, from_ref(&w1), &[], &[]).unwrap();
        assert_eq!(outcome, ClaimOutcome::Acquired);
        // The carried member and the admission ref now name the successor.
        assert_eq!(source.claim_holder(&claim_ref(&w1)).unwrap(), Some(successor));
        assert_eq!(source.claim_holder(ADMISSION_REF).unwrap(), Some(successor));
    }

    #[test]
    fn transfer_seal_loses_cleanly_when_a_carried_ref_was_concurrently_moved() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (predecessor, successor, intruder) = (bloom_id(1), bloom_id(2), bloom_id(3));
        let w1 = workpiece("wp-1");
        source.claim_seal(&predecessor, from_ref(&w1)).unwrap();

        // A concurrent writer repoints the carried ref onto a third bloom's claim
        // commit between the predecessor's seal and the transfer.
        let w1_ref = claim_ref(&w1);
        let intruder_commit = fake.seed_commit_with_message(&render_claim_message(&intruder), EMPTY_TREE);
        fake.seed_ref(&w1_ref, &intruder_commit);

        let outcome = source.transfer_seal(&predecessor, &successor, from_ref(&w1), &[], &[]).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(w1), held_by: intruder });
        // The CAS never removed the ref — it still names the intruder.
        assert_eq!(fake.ref_target(&w1_ref), Some(intruder_commit));
    }

    #[test]
    fn transfer_seal_fresh_acquires_net_new_and_releases_dropped_members() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (predecessor, successor) = (bloom_id(1), bloom_id(2));
        let (carried, dropped, fresh) = (workpiece("wp-carried"), workpiece("wp-dropped"), workpiece("wp-fresh"));
        source.claim_seal(&predecessor, &[carried.clone(), dropped.clone()]).unwrap();

        let outcome = source
            .transfer_seal(&predecessor, &successor, from_ref(&carried), from_ref(&fresh), from_ref(&dropped))
            .unwrap();
        assert_eq!(outcome, ClaimOutcome::Acquired);
        // The net-new member is freshly created for the successor, the carried
        // one is fast-forwarded to it, and the dropped one is released.
        assert_eq!(source.claim_holder(&claim_ref(&fresh)).unwrap(), Some(successor));
        assert_eq!(source.claim_holder(&claim_ref(&carried)).unwrap(), Some(successor));
        assert!(!fake.ref_exists(&claim_ref(&dropped)), "dropped member released");
    }

    #[test]
    fn release_seal_tombstones_then_deletes_the_owned_refs() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let owner = bloom_id(1);
        let w1 = workpiece("wp-1");
        source.claim_seal(&owner, from_ref(&w1)).unwrap();

        let outcome = source.release_seal(&owner, from_ref(&w1)).unwrap();
        assert_eq!(outcome, ClaimOutcome::Acquired);
        // Both the member claim ref and the admission ref the owner held are gone.
        assert!(!fake.ref_exists(&claim_ref(&w1)), "owned member released");
        assert!(!fake.ref_exists(ADMISSION_REF), "owned admission released");
    }

    #[test]
    fn release_seal_spares_a_ref_another_bloom_holds() {
        // Tripwire: the CAS read-guard that retires the check-then-delete TOCTOU —
        // a release must never delete a claim ref it does not own.
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (owner, stranger) = (bloom_id(1), bloom_id(2));
        let w1 = workpiece("wp-1");
        source.claim_seal(&owner, from_ref(&w1)).unwrap();

        let outcome = source.release_seal(&stranger, from_ref(&w1)).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(w1.clone()), held_by: owner });
        assert!(fake.ref_exists(&claim_ref(&w1)), "the foreign hold is spared, not deleted");
    }

    // Point `name` at a tombstone commit (empty tree + `Bloom-Id: tombstone`)
    // directly — the ref state an interrupted `release_seal` leaves after its
    // CAS-to-tombstone linearized but its name-only cleanup delete never ran.
    fn seed_tombstone(fake: &FakeGithub, name: &str) {
        let commit = fake.seed_commit_with_message(&render_tombstone_message(), EMPTY_TREE);
        fake.seed_ref(name, &commit);
    }

    // Point `name` at a claim commit carrying `holder`'s id on its `Bloom-Id`
    // message line — a live hold staged directly, sidestepping `claim_seal`'s
    // admission-ref coupling.
    fn seed_hold(fake: &FakeGithub, name: &str, holder: &BloomId) {
        let commit = fake.seed_commit_with_message(&render_claim_message(holder), EMPTY_TREE);
        fake.seed_ref(name, &commit);
    }

    #[test]
    fn enumerate_claims_classifies_held_tombstoned_and_admission_refs() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let holder = bloom_id(1);
        let (w1, w2) = (workpiece("wp-1"), workpiece("wp-2"));
        // w1 held by the claimant (which also takes the admission ref); w2 left at a
        // tombstone by an interrupted release.
        source.claim_seal(&holder, from_ref(&w1)).unwrap();
        seed_tombstone(&fake, &claim_ref(&w2));

        let mut states = source.enumerate_claims().unwrap();
        states.sort_by(|a, b| format!("{:?}", a.ref_kind).cmp(&format!("{:?}", b.ref_kind)));

        assert_eq!(
            states,
            vec![
                ClaimRefState { ref_kind: ClaimRefKind::MainlineAdmission, holder: ClaimHolder::Held(holder) },
                ClaimRefState { ref_kind: ClaimRefKind::Workpiece(w1), holder: ClaimHolder::Held(holder) },
                ClaimRefState { ref_kind: ClaimRefKind::Workpiece(w2), holder: ClaimHolder::Tombstoned },
            ],
        );
    }

    #[test]
    fn complete_transfer_moves_a_predecessor_held_ref_and_no_ops_at_the_successor() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (predecessor, successor) = (bloom_id(1), bloom_id(2));
        let w1 = workpiece("wp-1");
        source.claim_seal(&predecessor, from_ref(&w1)).unwrap();
        let ref_kind = ClaimRefKind::Workpiece(w1.clone());

        // A predecessor-held ref fast-forwards to the successor.
        assert_eq!(source.complete_transfer(&predecessor, &successor, &ref_kind).unwrap(), ClaimOutcome::Acquired);
        assert_eq!(source.claim_holder(&claim_ref(&w1)).unwrap(), Some(successor));

        // Re-driving the same completion over the already-moved ref is the no-op
        // that lets a boot re-drive converge — Acquired, ref unchanged.
        assert_eq!(source.complete_transfer(&predecessor, &successor, &ref_kind).unwrap(), ClaimOutcome::Acquired);
        assert_eq!(source.claim_holder(&claim_ref(&w1)).unwrap(), Some(successor));
    }

    #[test]
    fn complete_transfer_on_a_foreign_held_ref_is_held() {
        // Tripwire: the per-ref completion never stomps a ref a third bloom holds —
        // a holder that is neither predecessor nor successor is the clean Held.
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (predecessor, successor, foreign) = (bloom_id(1), bloom_id(2), bloom_id(3));
        let w1 = workpiece("wp-1");
        source.claim_seal(&foreign, from_ref(&w1)).unwrap();
        let ref_kind = ClaimRefKind::Workpiece(w1.clone());

        let outcome = source.complete_transfer(&predecessor, &successor, &ref_kind).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind, held_by: foreign });
        assert_eq!(source.claim_holder(&claim_ref(&w1)).unwrap(), Some(foreign), "the foreign hold is untouched");
    }

    #[test]
    fn complete_release_sweeps_a_tombstone_and_spares_a_live_foreign_ref() {
        // Tripwire: the sweep (`None` holder) deletes a tombstoned ref's name but
        // must never delete a live ref it does not own.
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (w1, w2) = (workpiece("wp-1"), workpiece("wp-2"));
        seed_tombstone(&fake, &claim_ref(&w1));
        source.claim_seal(&bloom_id(9), from_ref(&w2)).unwrap();

        // The tombstoned ref is swept. A tombstone *is* the released state, so
        // the sweep reports it absent rather than freshly released.
        assert_eq!(
            source.complete_release(None, &ClaimRefKind::Workpiece(w1.clone())).unwrap(),
            ClaimReleaseOutcome::AlreadyAbsent
        );
        assert!(!fake.ref_exists(&claim_ref(&w1)), "the tombstoned ref name was reclaimed");

        // A live ref under a `None` sweep is spared, reporting the holder it
        // found, never deleted.
        let outcome = source.complete_release(None, &ClaimRefKind::Workpiece(w2.clone())).unwrap();
        assert_eq!(outcome, ClaimReleaseOutcome::Changed { observed_holder: bloom_id(9) });
        assert!(fake.ref_exists(&claim_ref(&w2)), "a live ref is not swept");
    }

    #[test]
    fn complete_release_releases_a_holder_named_ref_and_spares_a_foreign_one() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        let (owner, foreign) = (bloom_id(1), bloom_id(2));
        let (mine, theirs) = (workpiece("wp-mine"), workpiece("wp-theirs"));
        source.claim_seal(&owner, from_ref(&mine)).unwrap();
        // Stage the foreign hold directly — a second `claim_seal` would conflict on
        // the admission ref the owner already holds and roll its member back.
        seed_hold(&fake, &claim_ref(&theirs), &foreign);

        // Naming the owner releases exactly its ref (the stranded-drop release path).
        assert_eq!(
            source.complete_release(Some(&owner), &ClaimRefKind::Workpiece(mine.clone())).unwrap(),
            ClaimReleaseOutcome::Released
        );
        assert!(!fake.ref_exists(&claim_ref(&mine)), "the owner's ref was released");

        // A ref a foreign bloom holds is spared even when a holder is named — the
        // expected-holder compare that keeps the ADR-0179 operator surface from
        // destroying another instance's live claim.
        let outcome = source.complete_release(Some(&owner), &ClaimRefKind::Workpiece(theirs.clone())).unwrap();
        assert_eq!(outcome, ClaimReleaseOutcome::Changed { observed_holder: foreign });
        assert!(fake.ref_exists(&claim_ref(&theirs)), "the foreign ref is spared");

        // An absent ref is the idempotent terminal success — the crash-after-delete
        // redrive ADR-0179 relies on to finish a release whose completion was
        // never admitted.
        assert_eq!(
            source.complete_release(Some(&owner), &ClaimRefKind::Workpiece(mine)).unwrap(),
            ClaimReleaseOutcome::AlreadyAbsent
        );
    }

    #[test]
    fn close_issue_comments_then_closes() {
        // Tripwire: the land reactor's human-facing close is a comment that
        // names the landing plus the state write. Dropping either half leaves
        // GitHub disagreeing with the journal about work that has landed.
        let fake = FakeGithub::new();
        fake.seed_issue(7, "the order");
        let source = git_source(&fake, false);

        source.close_issue(7, "landed via pull request #3").unwrap();

        assert_eq!(fake.comments_on(7), ["landed via pull request #3"]);
        assert_eq!(fake.issue_is_closed(7), Some(true));
    }

    #[test]
    fn close_issue_on_a_missing_target_is_an_error() {
        let fake = FakeGithub::new();
        let source = git_source(&fake, false);
        assert!(source.close_issue(7, "landed via pull request #3").is_err());
        assert_eq!(fake.issue_is_closed(7), None, "a miss does not fabricate the object");
    }
}
