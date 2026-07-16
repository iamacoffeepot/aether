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
//! # Digests are git object shas
//!
//! A bloomery [`Digest`] is a 32-byte sha256; a git object addressed under the
//! sha256 object format is the same 32 bytes, rendered as 64 lowercase hex. So
//! this backend treats a commit or tree object sha as the hex of a `Digest` and
//! back — the one representation choice the ADR carved out as implementation
//! level (the four operations, the CAS-land gate, and "no core module names a
//! GitHub type" are fixed; the ref layout and sha encoding are the slice's).
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

use aether_bloomery::{
    BloomId, Checkpoint, ClaimOutcome, ClaimRefKind, Digest, IntegrateOutcome, LandOutcome, LandingReceipt,
    SourceBackend, SourceSnapshot, WorkpieceId,
};

use crate::client::{GitDataApi, GithubError};

/// A source-port fault, distinct from the clean [`IntegrateOutcome`] /
/// [`LandOutcome`] refusals (which are not errors). Its own type because the
/// port needs a variant the value vocabulary does not carry — the gated-land
/// refusal — alongside the underlying transport faults.
#[derive(Debug)]
pub enum SourceError {
    /// The underlying Git Data call failed (transport or non-2xx status).
    Github(GithubError),
    /// `land` was called while compare-and-swap mainline landing is gated off
    /// (`cas_land_enabled` is false — ADR-0149 gates it to migration step 3).
    /// Not a [`LandOutcome`]: the swap was never attempted.
    LandingDisabled,
    /// An operation needed a ref that does not exist — e.g. `integrate` before
    /// the bloom's integration namespace was created, or `land` with no
    /// mainline ref.
    MissingRef(String),
    /// A ref name or object sha did not parse as the expected hex-of-`Digest`
    /// form — a malformed checkpoint ref, or a commit/tree sha that is not a
    /// 64-hex sha256.
    Malformed(String),
}

impl fmt::Display for SourceError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Github(error) => write!(f, "git source backend: {error}"),
            Self::LandingDisabled => {
                write!(f, "compare-and-swap land is disabled (cas_land_enabled is off, ADR-0149 migration step 3)")
            }
            Self::MissingRef(name) => write!(f, "git source backend: required ref `{name}` does not exist"),
            Self::Malformed(what) => write!(f, "git source backend: malformed {what}"),
        }
    }
}

impl Error for SourceError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Github(error) => Some(error),
            _ => None,
        }
    }
}

impl From<GithubError> for SourceError {
    fn from(error: GithubError) -> Self {
        Self::Github(error)
    }
}

/// The mainline ref this backend compare-and-swaps on `land`.
const MAINLINE_REF: &str = "heads/main";

/// The per-workpiece claim ref prefix (short `refs/`-less form the client
/// prepends `refs/` to). One ref per member workpiece under it carries the
/// claiming bloom id, its atomic create the distributed lock (ADR-0150).
const CLAIM_PREFIX: &str = "bloomery/claims/";

/// The single mainline-admission ref (short form): held while any bloom is
/// sealed-and-unlanded on the mainline, so a second instance's seal is refused
/// across instances (ADR-0150). Released by the landing receipt or an explicit
/// supersession.
const ADMISSION_REF: &str = "bloomery/admission/mainline";

/// The result of acquiring one shared ref: newly created, already held by the
/// same bloom (idempotent — a crash-retried acquire re-converges), or held by a
/// different bloom (the conflict that aborts the all-or-nothing acquire).
enum AcquireOne {
    Created,
    AlreadyMine,
    Held(BloomId),
}

/// Render a digest as the 64-lowercase-hex git object sha. `pub` (not
/// `pub(crate)`) because it lives in a private module — its reach is already
/// crate-internal, and `pub(crate)` here would be redundant.
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

/// Parse a 64-hex git object sha back into a digest, or `None` if it is not
/// exactly 64 hex characters. `pub` for the same private-module reason as
/// [`to_hex`].
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

/// The git source backend over a [`GitDataApi`] client. Holds the
/// `cas_land_enabled` gate — off by default, so `land` refuses until ADR-0149
/// migration step 3 explicitly enables it.
pub struct GitSource<C: GitDataApi> {
    client: C,
    cas_land_enabled: bool,
}

impl<C: GitDataApi> GitSource<C> {
    /// Build a source backend over `client`, with mainline landing gated by
    /// `cas_land_enabled` (pass `false` for every pre-migration-step-3 build).
    pub const fn new(client: C, cas_land_enabled: bool) -> Self {
        Self { client, cas_land_enabled }
    }

    /// Borrow the underlying client (test introspection).
    #[must_use]
    pub const fn client(&self) -> &C {
        &self.client
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
        let base_sha = to_hex(base);
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

    fn read_commit_tree(&self, sha: &str) -> Result<Digest, SourceError> {
        let commit = self.client.get_commit(sha)?;
        digest_from_hex(&commit.tree)
            .ok_or_else(|| SourceError::Malformed(format!("commit tree sha `{}`", commit.tree)))
    }

    /// The claim ref for a member workpiece. The workpiece id is a stable slug;
    /// it is interpolated directly, mirroring how checkpoint refs carry the tree
    /// hex.
    fn claim_ref(workpiece: &WorkpieceId) -> String {
        format!("{CLAIM_PREFIX}{}", workpiece.0)
    }

    /// Try to take one shared ref for `bloom_sha` (the claiming bloom id as its
    /// git-object hex, the digest-as-sha convention this backend uses
    /// throughout). A create that GitHub refuses with 422 (ref exists) reads the
    /// holder: the same bloom is the idempotent no-op, a different bloom the
    /// conflict.
    fn acquire_one(&self, name: &str, bloom_sha: &str) -> Result<AcquireOne, SourceError> {
        match self.client.create_ref(name, bloom_sha) {
            Ok(_) => Ok(AcquireOne::Created),
            Err(GithubError::Status { status: 422, .. }) => {
                let existing = self.client.get_ref(name)?.ok_or_else(|| SourceError::MissingRef(name.to_owned()))?;
                if existing.sha == bloom_sha {
                    Ok(AcquireOne::AlreadyMine)
                } else {
                    let held_by = digest_from_hex(&existing.sha)
                        .map(BloomId)
                        .ok_or_else(|| SourceError::Malformed(format!("claim ref `{name}` sha `{}`", existing.sha)))?;
                    Ok(AcquireOne::Held(held_by))
                }
            }
            Err(error) => Err(SourceError::Github(error)),
        }
    }
}

impl<C: GitDataApi> SourceBackend for GitSource<C> {
    type Error = SourceError;

    fn snapshot(&self, base: &Digest) -> Result<SourceSnapshot, Self::Error> {
        let tree = self.read_commit_tree(&to_hex(base))?;
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
        let current_tree = self.read_commit_tree(&current.sha)?;

        // Single-writer CAS: if the branch has advanced past the expected
        // checkpoint, refuse rather than clobber the concurrent advance.
        if current_tree != expected.tree {
            return Ok(IntegrateOutcome::StaleCheckpoint { actual: current_tree });
        }

        let commit = self.client.create_commit("bloomery integrate", &to_hex(candidate), &[current.sha])?;
        match self.client.update_ref(&integration, &commit.sha, false) {
            Ok(_) => Ok(IntegrateOutcome::Integrated { tree: *candidate }),
            // A 422 is GitHub's non-fast-forward refusal: a concurrent writer
            // moved the branch between our read and our update. That is the
            // same stale-checkpoint condition — re-read and report it as such.
            Err(GithubError::Status { status: 422, .. }) => {
                let actual = self.read_commit_tree(
                    &self.client.get_ref(&integration)?.ok_or(SourceError::MissingRef(integration))?.sha,
                )?;
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
        let actual = digest_from_hex(&current.sha)
            .ok_or_else(|| SourceError::Malformed(format!("mainline sha `{}`", current.sha)))?;

        if actual != *expected_base {
            return Ok(LandOutcome::BaseMoved { expected: *expected_base, actual });
        }
        match self.client.update_ref(MAINLINE_REF, &to_hex(new_head), false) {
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
                let actual = digest_from_hex(&current.sha)
                    .ok_or_else(|| SourceError::Malformed(format!("mainline sha `{}`", current.sha)))?;
                Ok(LandOutcome::BaseMoved { expected: *expected_base, actual })
            }
            Err(error) => Err(SourceError::Github(error)),
        }
    }

    fn claim_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, Self::Error> {
        let bloom_sha = to_hex(&bloom.0);
        // The refs to take, in order: every per-workpiece claim ref, then the
        // single mainline-admission ref. `created` tracks only the refs *this*
        // call created, so a conflict rolls back exactly those and never a ref
        // a prior (crashed) acquire of this same bloom left behind.
        let mut targets: Vec<(String, ClaimRefKind)> = workpieces
            .iter()
            .map(|workpiece| (Self::claim_ref(workpiece), ClaimRefKind::Workpiece(workpiece.clone())))
            .collect();
        targets.push((ADMISSION_REF.to_owned(), ClaimRefKind::MainlineAdmission));

        let mut created: Vec<String> = Vec::new();
        for (name, ref_kind) in targets {
            match self.acquire_one(&name, &bloom_sha)? {
                AcquireOne::Created => created.push(name),
                AcquireOne::AlreadyMine => {}
                AcquireOne::Held(held_by) => {
                    for name in &created {
                        self.client.delete_ref(name)?;
                    }
                    return Ok(ClaimOutcome::Held { ref_kind, held_by });
                }
            }
        }
        Ok(ClaimOutcome::Acquired)
    }

    fn release_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<(), Self::Error> {
        let bloom_sha = to_hex(&bloom.0);
        let mut names: Vec<String> = workpieces.iter().map(Self::claim_ref).collect();
        names.push(ADMISSION_REF.to_owned());
        for name in names {
            // Delete only a ref this bloom holds — never another bloom's, so a
            // release can never free a peer's live claim.
            if let Some(existing) = self.client.get_ref(&name)?
                && existing.sha == bloom_sha
            {
                self.client.delete_ref(&name)?;
            }
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use aether_bloomery::{
        BloomId, Checkpoint, ClaimOutcome, ClaimRefKind, Digest, IntegrateOutcome, LandOutcome, SourceBackend,
        WorkpieceId,
    };

    use super::{ADMISSION_REF, GitSource, SourceError, digest_from_hex, to_hex};
    use crate::testing::FakeGithub;

    fn digest(seed: u8) -> Digest {
        Digest::from_bytes([seed; 32])
    }

    fn bloom() -> BloomId {
        BloomId(digest(1))
    }

    fn workpiece(name: &str) -> WorkpieceId {
        WorkpieceId(name.to_owned())
    }

    // A fake seeded with a commit at `base_tree`, its mainline ref pointing at
    // that commit, and the bloom's integration namespace created on it.
    fn seeded() -> (FakeGithub, BloomId, Digest) {
        let fake = FakeGithub::new();
        let base_tree = digest(10);
        let base_commit = fake.seed_commit(&to_hex(&base_tree));
        fake.seed_ref("heads/main", &base_commit);
        let base = digest_from_hex(&base_commit).unwrap();
        let source = GitSource::new(fake.clone(), false);
        source.create_namespace(&bloom(), &base).unwrap();
        (fake, bloom(), base)
    }

    #[test]
    fn snapshot_is_stable_for_a_base() {
        let fake = FakeGithub::new();
        let tree = digest(7);
        let commit = fake.seed_commit(&to_hex(&tree));
        let base = digest_from_hex(&commit).unwrap();
        let source = GitSource::new(fake, false);

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
        let source = GitSource::new(fake, false);
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
    fn integrate_accepts_a_matching_checkpoint_and_rejects_a_stale_one() {
        let (fake, bloom, base) = seeded();
        let source = GitSource::new(fake, false);
        let base_tree = source.snapshot(&base).unwrap().tree;
        let expected = source.checkpoint(&bloom, &base_tree).unwrap();

        // Matching checkpoint → integrated.
        let candidate = digest(50);
        let outcome = source.integrate(&bloom, &candidate, &expected).unwrap();
        assert_eq!(outcome, IntegrateOutcome::Integrated { tree: candidate });

        // The branch has advanced; the same (now stale) checkpoint is refused.
        let another = digest(60);
        let stale = source.integrate(&bloom, &another, &expected).unwrap();
        assert_eq!(stale, IntegrateOutcome::StaleCheckpoint { actual: candidate });
    }

    #[test]
    fn land_is_refused_while_gated_and_cas_correct_when_enabled() {
        let (fake, bloom, base) = seeded();

        // Gated off (the default): a typed refusal, not a swap.
        let gated = GitSource::new(fake.clone(), false);
        let new_head = digest(90);
        match gated.land(&bloom, &base, &new_head) {
            Err(SourceError::LandingDisabled) => {}
            other => panic!("expected LandingDisabled, got {other:?}"),
        }
        assert_eq!(fake.ref_target("heads/main"), Some(to_hex(&base)), "mainline untouched while gated");

        // Enabled: expected-base CAS advances mainline and issues a receipt.
        let enabled = GitSource::new(fake.clone(), true);
        match enabled.land(&bloom, &base, &new_head).unwrap() {
            LandOutcome::Landed(receipt) => {
                assert_eq!(receipt.previous_base, base);
                assert_eq!(receipt.new_head, new_head);
            }
            LandOutcome::BaseMoved { .. } => panic!("expected Landed, got BaseMoved"),
        }
        assert_eq!(fake.ref_target("heads/main"), Some(to_hex(&new_head)), "mainline advanced to the new head");

        // A stale expected base is the clean BaseMoved refusal.
        let stale_expected = digest(200);
        match enabled.land(&bloom, &stale_expected, &digest(91)).unwrap() {
            LandOutcome::BaseMoved { expected, actual } => {
                assert_eq!(expected, stale_expected);
                assert_eq!(actual, new_head);
            }
            LandOutcome::Landed(_) => panic!("expected BaseMoved, got Landed"),
        }
    }

    #[test]
    fn claim_seal_acquires_every_workpiece_ref_and_the_admission_ref() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake.clone(), false);
        let members = [workpiece("reactor-core"), workpiece("coolant-loop")];

        assert_eq!(source.claim_seal(&bloom(), &members).unwrap(), ClaimOutcome::Acquired);
        // Each per-workpiece ref and the single admission ref now carry this
        // bloom's id (its digest as the ref's git-object hex).
        let bloom_sha = to_hex(&bloom().0);
        assert_eq!(fake.ref_target("bloomery/claims/reactor-core").as_deref(), Some(bloom_sha.as_str()));
        assert_eq!(fake.ref_target("bloomery/claims/coolant-loop").as_deref(), Some(bloom_sha.as_str()));
        assert_eq!(fake.ref_target(ADMISSION_REF).as_deref(), Some(bloom_sha.as_str()));
    }

    #[test]
    fn claim_seal_reports_a_workpiece_conflict_and_its_holder() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake.clone(), false);
        let first = bloom();
        let second = BloomId(digest(2));

        source.claim_seal(&first, &[workpiece("reactor-core")]).unwrap();
        // A second bloom claiming the same workpiece is refused, naming the ref
        // kind and the bloom already holding it.
        let outcome = source.claim_seal(&second, &[workpiece("reactor-core")]).unwrap();
        assert_eq!(
            outcome,
            ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(workpiece("reactor-core")), held_by: first }
        );
    }

    #[test]
    fn claim_seal_reports_an_admission_conflict_and_rolls_back_partial_claims() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake.clone(), false);
        let first = bloom();
        let second = BloomId(digest(2));

        // `first` holds the mainline-admission ref (disjoint workpieces).
        source.claim_seal(&first, &[workpiece("alpha")]).unwrap();
        // `second`'s workpiece is free, but the admission ref is held → the whole
        // acquire is refused against the admission ref, and the workpiece ref
        // `second` created on the way is rolled back.
        let outcome = source.claim_seal(&second, &[workpiece("beta")]).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::MainlineAdmission, held_by: first });
        assert!(!fake.ref_exists("bloomery/claims/beta"), "the partial workpiece claim was rolled back");
    }

    #[test]
    fn claim_seal_is_idempotent_for_the_same_bloom() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake.clone(), false);
        let members = [workpiece("reactor-core")];

        source.claim_seal(&bloom(), &members).unwrap();
        // A crash-retried acquire by the same bloom re-converges — its own refs
        // are not a conflict.
        assert_eq!(source.claim_seal(&bloom(), &members).unwrap(), ClaimOutcome::Acquired);
    }

    #[test]
    fn release_seal_deletes_only_this_blooms_refs() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake.clone(), false);
        let mine = bloom();
        let other = BloomId(digest(2));

        source.claim_seal(&mine, &[workpiece("reactor-core")]).unwrap();
        // A peer holds an unrelated workpiece claim (but not the admission ref,
        // released below): seed it directly so release must leave it be.
        fake.seed_ref("bloomery/claims/peer-owned", &to_hex(&other.0));

        source.release_seal(&mine, &[workpiece("reactor-core")]).unwrap();
        assert!(!fake.ref_exists("bloomery/claims/reactor-core"), "this bloom's claim released");
        assert!(!fake.ref_exists(ADMISSION_REF), "this bloom's admission ref released");
        assert!(fake.ref_exists("bloomery/claims/peer-owned"), "a peer's claim is never touched");
    }
}
