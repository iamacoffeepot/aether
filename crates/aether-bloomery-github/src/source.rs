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
//! through a persisted [`Correspondence`] handle held next to the client:
//! forward ([`Correspondence::resolve_git`]) to turn a digest into the git
//! object that carries it, reverse ([`Correspondence::resolve_digest`]) to read a
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
//!
//! [#3465]: https://github.com/iamacoffeepot/aether/issues/3465

use std::error::Error;
use std::fmt;
use std::sync::Arc;

use aether_bloomery::{
    BloomId, Checkpoint, ClaimHolder, ClaimOutcome, ClaimRefKind, ClaimRefState, Digest, IntegrateOutcome, LandOutcome,
    LandingReceipt, SourceBackend, SourceSnapshot, WorkpieceId,
};

use crate::client::{GitDataApi, GithubError};
use crate::correspondence::{Correspondence, CorrespondenceError, GitObjectId};

/// The persisted correspondence the source port resolves real git shas through,
/// held behind a shared handle so a `SourceBackend`'s `&self` methods can drive
/// it. The host mounts a `SQLite`-backed impl; tests mount an in-memory one.
pub type SharedCorrespondence = Arc<dyn Correspondence + Send + Sync>;

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
    /// [`Correspondence`] store and found none recorded — the honest boundary
    /// this slice draws (ADR-0150): the object was never materialized or its
    /// correspondence never seeded, so the port refuses cleanly rather than
    /// hex-punning a digest git cannot resolve. `what` names which resolution
    /// missed (mainline head, candidate tree, …).
    UnresolvedCorrespondence(String),
    /// The [`Correspondence`] store itself faulted (a durable read/write failed),
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

/// The mainline ref this backend compare-and-swaps on `land`.
const MAINLINE_REF: &str = "heads/main";

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
/// the [`Correspondence`] store. `pub` (not `pub(crate)`) because it lives in a
/// private module — its reach is already crate-internal, and `pub(crate)` here
/// would be redundant.
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
/// sha256/32 and resolve through the [`Correspondence`] store. `pub` for the
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
/// [`Correspondence`] handle (ADR-0150). The correspondence is the seam the
/// mainline paths resolve real git shas through; the `cas_land_enabled` gate is
/// on by default since ADR-0149 migration step 3 made the CAS `land` the landing
/// of record — a `false` gate is the explicit kill switch under which `land`
/// refuses.
pub struct GitSource<C: GitDataApi> {
    client: C,
    correspondence: SharedCorrespondence,
    cas_land_enabled: bool,
}

impl<C: GitDataApi> GitSource<C> {
    /// Build a source backend over `client` and `correspondence`, with mainline
    /// landing gated by `cas_land_enabled` (`false` is the kill switch under
    /// which `land` refuses; production wires it on, per ADR-0149 migration
    /// step 3).
    pub fn new(client: C, correspondence: SharedCorrespondence, cas_land_enabled: bool) -> Self {
        Self { client, correspondence, cas_land_enabled }
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
            .resolve_git(digest)?
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
        self.correspondence.resolve_digest(&git)?.ok_or_else(|| SourceError::UnresolvedCorrespondence(what.to_owned()))
    }

    // The tree digest at real commit `sha`: read the commit for its real tree
    // object sha, then reverse-resolve that object to its tree digest. The
    // integration-branch path reads a real git tree this way; the claim path
    // carries the bloom id on a `Bloom-Id` message line instead (`parse_bloom_line`).
    fn integration_tree(&self, sha: &str) -> Result<Digest, SourceError> {
        let commit = self.client.get_commit(sha)?;
        self.resolve_object_digest(&commit.tree, "integration branch tree object")
    }

    fn integration_ref(bloom: &BloomId) -> String {
        format!("heads/bloom/{}/integration", to_hex(&bloom.0))
    }

    fn attempt_ref(bloom: &BloomId, attempt: u32) -> String {
        format!("heads/bloom/{}/attempt/{attempt}", to_hex(&bloom.0))
    }

    fn checkpoint_prefix(bloom: &BloomId) -> String {
        format!("heads/bloom/{}/checkpoint/", to_hex(&bloom.0))
    }

    fn checkpoint_ref(bloom: &BloomId, tree: &Digest) -> String {
        format!("{}{}", Self::checkpoint_prefix(bloom), to_hex(tree))
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

    fn ensure_ref(&self, name: &str, sha: &str) -> Result<(), SourceError> {
        if self.client.get_ref(name)?.is_none() {
            self.client.create_ref(name, sha)?;
        }
        Ok(())
    }

    fn workpiece_claim_ref(workpiece: &WorkpieceId) -> String {
        format!("bloomery/claims/{}", workpiece.0)
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
            let Some(current) = self.client.get_ref(name)? else {
                continue;
            };
            let claim_state = self.classify_holder(&current.sha)?;
            // An already-tombstoned ref is released regardless of `owner` — the
            // tombstone *is* the released state, so finishing the interrupted
            // cleanup delete is pure name reclamation, safe for any instance. But
            // guard the cleanup with the same CAS the live-holder branch uses: a
            // blind name-only delete would race a fresh claim that reused the name
            // in the window since the read, so re-assert the observed tombstone via
            // a no-op fast-forward first. If the ref moved to an unrelated fresh
            // claim commit that reassertion 422s (the new commit is not a
            // fast-forward descendant of the stale tombstone), so the name has
            // already been reclaimed by that holder — skip the delete and move on
            // rather than clobbering a claim we never owned.
            if matches!(claim_state, ClaimHolder::Tombstoned) {
                match self.client.update_ref(name, &current.sha, false) {
                    Ok(_) => self.client.delete_ref(name)?,
                    Err(GithubError::Status { status: 422, .. }) => {}
                    Err(error) => return Err(SourceError::Github(error)),
                }
                continue;
            }
            let ClaimHolder::Held(holder) = claim_state else {
                unreachable!("Tombstoned handled above")
            };
            // A live ref is released only when `owner` names its holder; a `None`
            // sweep, or a mismatch, spares it and reports the holder.
            if owner != Some(&holder) {
                return Ok(ClaimOutcome::Held { ref_kind: kind.clone(), held_by: holder });
            }
            let tombstone = self.client.create_commit(&render_tombstone_message(), EMPTY_TREE, &[current.sha])?;
            match self.client.update_ref(name, &tombstone.sha, false) {
                Ok(_) => {}
                // Lost the fast-forward CAS to a concurrent mutation — re-read the
                // holder and report it rather than deleting a ref we no longer own.
                Err(GithubError::Status { status: 422, .. }) => {
                    return Ok(ClaimOutcome::Held { ref_kind: kind.clone(), held_by: self.require_holder(name)? });
                }
                Err(error) => return Err(SourceError::Github(error)),
            }
            self.client.delete_ref(name)?;
        }
        Ok(ClaimOutcome::Acquired)
    }
}

impl<C: GitDataApi> SourceBackend for GitSource<C> {
    type Error = SourceError;

    fn snapshot(&self, base: &Digest) -> Result<SourceSnapshot, Self::Error> {
        // Forward-resolve the base head digest to its real commit, read that
        // commit, and reverse-resolve its real tree sha back to the tree digest.
        let base_sha = self.resolve_git_sha(base, "snapshot base head digest")?;
        let commit = self.client.get_commit(&base_sha)?;
        let tree = self.resolve_object_digest(&commit.tree, "snapshot base tree object")?;
        Ok(SourceSnapshot { head: *base, tree })
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
        // the core's `land` resolves `new_head` (the resolved candidate *tree*,
        // reduce.rs) through, and what `StaleCheckpoint` reverse-resolves the
        // advanced branch's tree back to. Recording a *distinct* landable
        // head-commit correspondence would both clobber that tree mapping and go
        // unread; materializing a real landable head is the candidate-tree
        // materialization follow-on this slice defers (§Side findings, #3603).
        let candidate_tree_sha = self.resolve_git_sha(candidate, "candidate tree digest")?;
        let commit = self.client.create_commit("bloomery integrate", &candidate_tree_sha, &[current.sha])?;
        match self.client.update_ref(&integration, &commit.sha, false) {
            Ok(_) => Ok(IntegrateOutcome::Integrated { tree: *candidate }),
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

    fn land(&self, bloom: &BloomId, expected_base: &Digest, new_head: &Digest) -> Result<LandOutcome, Self::Error> {
        if !self.cas_land_enabled {
            return Err(SourceError::LandingDisabled);
        }
        let current =
            self.client.get_ref(MAINLINE_REF)?.ok_or_else(|| SourceError::MissingRef(MAINLINE_REF.to_owned()))?;
        // Reverse-resolve the real mainline object (a sha1/40-hex on a real repo,
        // which the old fixed-64-hex gate rejected as `Malformed` before any swap)
        // to the base digest, then compare against the sealed base.
        let actual = self.resolve_object_digest(&current.sha, "mainline head object")?;

        if actual != *expected_base {
            return Ok(LandOutcome::BaseMoved { expected: *expected_base, actual });
        }
        // Forward-resolve the new head digest to its real git object for the CAS.
        let new_head_sha = self.resolve_git_sha(new_head, "land new head digest")?;
        match self.client.update_ref(MAINLINE_REF, &new_head_sha, false) {
            Ok(_) => Ok(LandOutcome::Landed(LandingReceipt {
                bloom: *bloom,
                previous_base: *expected_base,
                new_head: *new_head,
            })),
            // The base moved between our read and our fast-forward-only update.
            Err(GithubError::Status { status: 422, .. }) => {
                let current = self
                    .client
                    .get_ref(MAINLINE_REF)?
                    .ok_or_else(|| SourceError::MissingRef(MAINLINE_REF.to_owned()))?;
                let actual = self.resolve_object_digest(&current.sha, "mainline head object")?;
                Ok(LandOutcome::BaseMoved { expected: *expected_base, actual })
            }
            Err(error) => Err(SourceError::Github(error)),
        }
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
    ) -> Result<ClaimOutcome, Self::Error> {
        self.release_targets(expected_holder, &[(ref_kind.clone(), Self::ref_name(ref_kind))])
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::slice::from_ref;

    use aether_bloomery::{
        BloomId, Checkpoint, ClaimHolder, ClaimOutcome, ClaimRefKind, ClaimRefState, Digest, IntegrateOutcome,
        LandOutcome, SourceBackend, WorkpieceId,
    };

    use std::sync::Arc;

    use super::{
        ADMISSION_REF, EMPTY_TREE, GitSource, SourceError, parse_bloom_line, render_claim_message,
        render_tombstone_message, to_hex,
    };
    use crate::client::GitDataApi;
    use crate::testing::FakeGithub;

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    // A `GitSource` over `fake` as both its git-data client and its correspondence
    // store — one in-process double serves both seams, so a test seeds git objects
    // and their correspondences into the same fake.
    fn git_source(fake: &FakeGithub, cas_land_enabled: bool) -> GitSource<FakeGithub> {
        GitSource::new(fake.clone(), Arc::new(fake.clone()), cas_land_enabled)
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
        assert_eq!(outcome, IntegrateOutcome::Integrated { tree: candidate });

        // The branch has advanced; the same (now stale) checkpoint is refused,
        // reverse-resolving the advanced branch's tree back to the candidate.
        let another = digest(60);
        fake.seed_git_object(&another);
        let stale = source.integrate(&bloom, &another, &expected).unwrap();
        assert_eq!(stale, IntegrateOutcome::StaleCheckpoint { actual: candidate });
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
    fn land_reads_a_sha1_mainline_and_cas_advances_when_enabled() {
        let fake = FakeGithub::new();
        let bloom = bloom();
        // A real sha1 (40-hex) mainline — the object format the reported failure
        // (#3590) hit, which the old fixed-64-hex `digest_from_hex` gate rejected
        // as `Malformed` before any swap. The correspondence resolves it.
        let base = digest(10);
        let mainline_sha1 = "a1".repeat(20);
        assert_eq!(mainline_sha1.len(), 40, "a sha1 object id is 40 hex");
        fake.seed_ref("heads/main", &mainline_sha1);
        fake.seed_correspondence(&base, &mainline_sha1);
        let new_head = digest(90);
        fake.seed_git_object(&new_head);

        // Gated off (the default): a typed refusal, not a swap.
        let gated = git_source(&fake, false);
        match gated.land(&bloom, &base, &new_head) {
            Err(SourceError::LandingDisabled) => {}
            other => panic!("expected LandingDisabled, got {other:?}"),
        }
        assert_eq!(fake.ref_target("heads/main"), Some(mainline_sha1), "mainline untouched while gated");

        // Enabled: reverse-resolve the sha1 mainline to the base, CAS to the new
        // head's resolved object, and issue a receipt.
        let enabled = git_source(&fake, true);
        match enabled.land(&bloom, &base, &new_head).unwrap() {
            LandOutcome::Landed(receipt) => {
                assert_eq!(receipt.previous_base, base);
                assert_eq!(receipt.new_head, new_head);
            }
            LandOutcome::BaseMoved { .. } => panic!("expected Landed, got BaseMoved"),
        }
        assert_eq!(
            fake.ref_target("heads/main"),
            Some(to_hex(&new_head)),
            "mainline advanced to the resolved new head"
        );
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
        // the mainline object actually resolves to.
        let stale_expected = digest(200);
        match enabled.land(&bloom, &stale_expected, &digest(91)).unwrap() {
            LandOutcome::BaseMoved { expected, actual } => {
                assert_eq!(expected, stale_expected);
                assert_eq!(actual, actual_base);
            }
            LandOutcome::Landed(_) => panic!("expected BaseMoved, got Landed"),
        }
    }

    #[test]
    fn land_errors_cleanly_when_the_mainline_correspondence_is_unrecorded() {
        // A real sha1 mainline with no recorded correspondence is the clean
        // `UnresolvedCorrespondence`, never the old `Malformed`-before-swap.
        let fake = FakeGithub::new();
        fake.seed_ref("heads/main", &"c3".repeat(20));
        let enabled = git_source(&fake, true);
        match enabled.land(&bloom(), &digest(10), &digest(90)) {
            Err(SourceError::UnresolvedCorrespondence(_)) => {}
            other => panic!("expected UnresolvedCorrespondence, got {other:?}"),
        }
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

        // The tombstoned ref is swept.
        assert_eq!(
            source.complete_release(None, &ClaimRefKind::Workpiece(w1.clone())).unwrap(),
            ClaimOutcome::Acquired
        );
        assert!(!fake.ref_exists(&claim_ref(&w1)), "the tombstoned ref name was reclaimed");

        // A live ref under a `None` sweep is spared (reported Held), never deleted.
        let outcome = source.complete_release(None, &ClaimRefKind::Workpiece(w2.clone())).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(w2.clone()), held_by: bloom_id(9) });
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
            ClaimOutcome::Acquired
        );
        assert!(!fake.ref_exists(&claim_ref(&mine)), "the owner's ref was released");

        // A ref a foreign bloom holds is spared even when a holder is named.
        let outcome = source.complete_release(Some(&owner), &ClaimRefKind::Workpiece(theirs.clone())).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(theirs.clone()), held_by: foreign });
        assert!(fake.ref_exists(&claim_ref(&theirs)), "the foreign ref is spared");
    }
}
