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

/// The single mainline-admission claim ref (short `heads/…`-style form, no
/// leading `refs/` — the client prepends it), realizing Bloomery's "one sealed,
/// unlanded bloom per mainline" invariant as a shared repository ref
/// (ADR-0150 §The claim registry).
const ADMISSION_REF: &str = "bloomery/admission/mainline";

/// The tree a tombstone claim commit carries — the all-zero digest, distinct
/// from any real bloom id (a bloom id is the digest of its non-empty canonical
/// bytes), so a boot reconcile (layer (c)) can tell a tombstoned-but-undeleted
/// ref an interrupted release left from a live claim.
const TOMBSTONE_TREE: Digest = Digest::from_bytes([0u8; 32]);

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

    fn workpiece_claim_ref(workpiece: &WorkpieceId) -> String {
        format!("bloomery/claims/{}", workpiece.0)
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

    // A claim commit's tree IS the claiming bloom's id (hex-of-digest), so the
    // holder is resolvable from the commit the ref points at via `get_commit` —
    // the same "commit tree = hex of digest" convention `snapshot` uses. The
    // message carries the id too, for human legibility on the shadow repo.
    // Returns the created commit's sha (the value a ref is pointed at).
    fn create_claim_commit(&self, bloom: &BloomId, parents: &[String]) -> Result<String, SourceError> {
        let hex = to_hex(&bloom.0);
        Ok(self.client.create_commit(&format!("bloomery claim {hex}"), &hex, parents)?.sha)
    }

    // The bloom currently holding `name`, resolved from its claim commit's tree,
    // or `None` if the ref does not exist.
    fn claim_holder(&self, name: &str) -> Result<Option<BloomId>, SourceError> {
        match self.client.get_ref(name)? {
            None => Ok(None),
            Some(git_ref) => Ok(Some(BloomId(self.read_commit_tree(&git_ref.sha)?))),
        }
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

    // Release each of `targets` this `bloom` holds by a fast-forward CAS to a
    // tombstone child commit (the linearization point) then a name-only cleanup
    // delete. An absent ref is skipped (idempotent); a ref another bloom holds is
    // spared and reported as `Held` — the CAS read-guard that retires the
    // check-then-delete TOCTOU. Shared by `release_seal` (workpieces + admission)
    // and `transfer_seal`'s dropped-member cleanup (workpieces only).
    fn release_targets(
        &self,
        bloom: &BloomId,
        targets: &[(ClaimRefKind, String)],
    ) -> Result<ClaimOutcome, SourceError> {
        for (kind, name) in targets {
            let Some(current) = self.client.get_ref(name)? else {
                continue;
            };
            let holder = BloomId(self.read_commit_tree(&current.sha)?);
            if holder != *bloom {
                return Ok(ClaimOutcome::Held { ref_kind: kind.clone(), held_by: holder });
            }
            let tombstone =
                self.client.create_commit("bloomery claim tombstone", &to_hex(&TOMBSTONE_TREE), &[current.sha])?;
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
        let targets = Self::claim_targets(workpieces, true);
        let mut created: Vec<String> = Vec::new();
        for (kind, name) in &targets {
            // A fresh root claim commit per member, then an atomic create. A 422
            // is another bloom's existing hold: resolve the holder, roll back
            // every ref this acquire created, and report the first conflict.
            let commit = self.create_claim_commit(bloom, &[])?;
            match self.client.create_ref(name, &commit) {
                Ok(_) => created.push(name.clone()),
                Err(GithubError::Status { status: 422, .. }) => {
                    let held_by = self.require_holder(name)?;
                    self.rollback(&created)?;
                    return Ok(ClaimOutcome::Held { ref_kind: kind.clone(), held_by });
                }
                Err(error) => {
                    self.rollback(&created)?;
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
            let holder = BloomId(self.read_commit_tree(&current.sha)?);
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
        self.release_targets(predecessor, &Self::claim_targets(dropped, false))?;

        Ok(ClaimOutcome::Acquired)
    }

    fn release_seal(&self, bloom: &BloomId, workpieces: &[WorkpieceId]) -> Result<ClaimOutcome, Self::Error> {
        self.release_targets(bloom, &Self::claim_targets(workpieces, true))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::slice::from_ref;

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
    fn claim_seal_acquires_every_member_and_the_admission_ref() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake, false);
        let claimant = bloom_id(1);
        let (w1, w2) = (workpiece("wp-1"), workpiece("wp-2"));

        let outcome = source.claim_seal(&claimant, &[w1.clone(), w2.clone()]).unwrap();
        assert_eq!(outcome, ClaimOutcome::Acquired);

        // Each member claim ref and the single admission ref resolves to the
        // claiming bloom (the claim commit's tree carries its id).
        for name in [claim_ref(&w1), claim_ref(&w2), ADMISSION_REF.to_owned()] {
            assert_eq!(source.claim_holder(&name).unwrap(), Some(claimant), "{name} held by the claimant");
        }
    }

    #[test]
    fn claim_seal_workpiece_conflict_reports_the_ref_and_holder() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake, false);
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
        let source = GitSource::new(fake, false);
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
        let source = GitSource::new(fake.clone(), false);
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
        let source = GitSource::new(fake, false);
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
        let source = GitSource::new(fake.clone(), false);
        let (predecessor, successor, intruder) = (bloom_id(1), bloom_id(2), bloom_id(3));
        let w1 = workpiece("wp-1");
        source.claim_seal(&predecessor, from_ref(&w1)).unwrap();

        // A concurrent writer repoints the carried ref onto a third bloom's claim
        // commit between the predecessor's seal and the transfer.
        let w1_ref = claim_ref(&w1);
        let intruder_commit = fake.seed_commit(&to_hex(&intruder.0));
        fake.seed_ref(&w1_ref, &intruder_commit);

        let outcome = source.transfer_seal(&predecessor, &successor, from_ref(&w1), &[], &[]).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(w1), held_by: intruder });
        // The CAS never removed the ref — it still names the intruder.
        assert_eq!(fake.ref_target(&w1_ref), Some(intruder_commit));
    }

    #[test]
    fn transfer_seal_fresh_acquires_net_new_and_releases_dropped_members() {
        let fake = FakeGithub::new();
        let source = GitSource::new(fake.clone(), false);
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
        let source = GitSource::new(fake.clone(), false);
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
        let source = GitSource::new(fake.clone(), false);
        let (owner, stranger) = (bloom_id(1), bloom_id(2));
        let w1 = workpiece("wp-1");
        source.claim_seal(&owner, from_ref(&w1)).unwrap();

        let outcome = source.release_seal(&stranger, from_ref(&w1)).unwrap();
        assert_eq!(outcome, ClaimOutcome::Held { ref_kind: ClaimRefKind::Workpiece(w1.clone()), held_by: owner });
        assert!(fake.ref_exists(&claim_ref(&w1)), "the foreign hold is spared, not deleted");
    }
}
