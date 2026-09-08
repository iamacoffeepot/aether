//! The lane vocabulary a base declares, read out of that base's own tree
//! (ADR-0215).
//!
//! `pipeline.toml` at the repository root states what the checked-out tree can
//! run. The coordinator does not compile that statement and it does not let an
//! operator author it: it reads the file out of the sealed base, decodes it,
//! and seals the *decoded value* into the draft's ADR-0174 registry. Only a
//! host holding both the manifest and the tree can check that the vocabulary
//! being attested is the one the checkout carries, which is why
//! `POST /configs` refuses this kind outright (see
//! [`author_config`](super::configs::author_config)).
//!
//! The bytes are the value, never the file text. Reformatting `pipeline.toml`,
//! reordering its tables, or editing a comment re-seals nothing; only a change
//! in meaning moves the digest — which matters, because this digest is about to
//! appear in every receipt.
//!
//! # What a missing file means here, and why it is not yet a refusal
//!
//! ADR-0215 refuses a base that carries no manifest, and deliberately does not
//! arm that refusal at this slice. The manifest cannot arrive by the mechanism
//! that requires it: the file lands first as ordinary content on the day's
//! base, and the refusal is armed only once every base the coordinator can seal
//! against carries one. So a base whose tree has no `pipeline.toml` — and a
//! base whose git object this host cannot resolve, which is the same
//! non-answer — derives no entry and refuses nothing.
//!
//! A file that *is* there and will not decode is the opposite case, and is
//! refused now: it can only exist because someone edited it, the editor is
//! holding the diff that broke it, and nothing about the bootstrap ordering
//! asks the coordinator to tolerate a manifest whose meaning it cannot read.

use std::path::Path;

use aether_bloomery::{
    Correspondence, Digest, PIPELINE_MANIFEST_PATH, PipelineManifest, PipelineManifestError, config_address,
};
use aether_data::Kind;
use aether_data::wire::to_vec;

use super::commission_reader::{blob_text, sealed_commit_hex};
use super::state::ApiCapabilityState;

/// A manifest the host read out of a base, in the two forms a registry entry
/// needs: the canonical wire bytes to store, and the address to seal.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(super) struct DerivedManifest {
    /// The content address the draft's registry names.
    pub(super) address: Digest,
    /// The canonical wire encoding of the decoded manifest.
    pub(super) bytes: Vec<u8>,
}

impl ApiCapabilityState {
    /// The manifest `base` declares, or [`None`] when that base declares none.
    ///
    /// Reads the blob at the base's git object in the configured lane
    /// repository, exactly as the sealed-ADR catalog does — never the process
    /// working tree, whose `pipeline.toml` says what this host was built from
    /// rather than what the base it is about to seal against can run.
    pub(super) fn derive_pipeline_manifest(
        &self,
        base: Digest,
    ) -> Result<Option<DerivedManifest>, PipelineManifestError> {
        derive_pipeline_manifest(&self.lane_repository, self.sealed_correspondence(), base)
    }

    /// The correspondence that turns a Bloomery [`Digest`] into a git object,
    /// or [`None`] on a build or a chassis that mounted no source.
    pub(super) fn sealed_correspondence(&self) -> Option<&dyn Correspondence> {
        #[cfg(feature = "github")]
        {
            self.correspondence.as_deref().map(|correspondence| correspondence as &dyn Correspondence)
        }
        #[cfg(not(feature = "github"))]
        {
            None
        }
    }
}

/// Read, decode, encode, and address the manifest at `base` in `repo`.
///
/// Free of the router state so the read is testable against a bare repository
/// and a correspondence pair, which is the only interesting axis: the git
/// object a digest resolves to, and the text at that object's `pipeline.toml`.
fn derive_pipeline_manifest(
    repo: &Path,
    correspondence: Option<&dyn Correspondence>,
    base: Digest,
) -> Result<Option<DerivedManifest>, PipelineManifestError> {
    let Some(commit) = sealed_commit_hex(correspondence, base) else {
        return Ok(None);
    };
    let Some(text) = blob_text(repo, &commit, PIPELINE_MANIFEST_PATH) else {
        return Ok(None);
    };

    // The address is taken over these exact bytes rather than through
    // `ConfigKind::address`, so the row the store holds and the digest the
    // draft seals are one encoding rather than two that have to agree.
    let bytes = to_vec(&PipelineManifest::from_toml(&text)?)
        .expect("a pipeline manifest never exceeds the ADR-0118 u32 wire-length ceiling");

    Ok(Some(DerivedManifest { address: config_address(PipelineManifest::NAME, &bytes), bytes }))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use aether_bloomery::{
        BackendObjectId, Correspondence, CorrespondenceError, Digest, PIPELINE_MANIFEST_PATH, PipelineManifest,
        PipelineManifestError, config_address,
    };
    use aether_bloomery_git::GitObjectId;
    use aether_data::Kind;
    use aether_data::wire::to_vec;
    use tempfile::TempDir;

    use super::derive_pipeline_manifest;

    /// This repository's own manifest — the text a base carries once the file
    /// has landed on it.
    const CHECKED_IN: &str = include_str!("../../../../../pipeline.toml");

    /// The one recorded digest-to-object pair, the shape correspondence has at
    /// a seal door.
    struct OnePair {
        digest: Digest,
        object: BackendObjectId,
    }

    impl Correspondence for OnePair {
        fn record(&self, _: &Digest, _: &BackendObjectId) -> Result<(), CorrespondenceError> {
            Ok(())
        }

        fn resolve_backend_object(&self, digest: &Digest) -> Result<Option<BackendObjectId>, CorrespondenceError> {
            Ok((*digest == self.digest).then(|| self.object.clone()))
        }

        fn resolve_digest(&self, object: &BackendObjectId) -> Result<Option<Digest>, CorrespondenceError> {
            Ok((object == &self.object).then_some(self.digest))
        }
    }

    #[test]
    fn the_manifest_is_the_blob_at_the_base_not_the_working_tree() {
        // The whole point of reading through correspondence: the file on disk
        // right now is what this host was built from, and the file at the base
        // is what the bloom is about to dispatch against. A read that took the
        // working tree would attest a vocabulary the base does not carry, which
        // is the divergence ADR-0215 exists to close.
        let repo = repository();
        write(repo.path(), PIPELINE_MANIFEST_PATH, CHECKED_IN);
        let declared = commit(repo.path(), "the base declares its lanes");
        write(repo.path(), PIPELINE_MANIFEST_PATH, "version = 9\n");

        let base = Digest::from_bytes([7; 32]);
        let correspondence = OnePair { digest: base, object: object_at(&declared) };
        let derived = derive_pipeline_manifest(repo.path(), Some(&correspondence), base)
            .expect("the base's manifest reads")
            .expect("the base carries a manifest");

        let bytes = to_vec(&PipelineManifest::from_toml(CHECKED_IN).expect("the checked-in manifest reads"))
            .expect("a manifest encodes");
        assert_eq!(derived.bytes, bytes, "the sealed bytes are the decoded value the base declared");
        assert_eq!(derived.address, config_address(PipelineManifest::NAME, &bytes));
    }

    #[test]
    fn a_base_that_declares_no_manifest_derives_nothing_rather_than_refusing() {
        // The bootstrap ordering, as a test: every base sealed before the file
        // landed has to stay sealable, and an unresolvable base is the same
        // non-answer as an absent file. Arming either refusal here would take
        // the day down at the moment the file was introduced.
        let repo = repository();
        write(repo.path(), "README.md", "a base from before the manifest\n");
        let bare = commit(repo.path(), "no manifest yet");

        let base = Digest::from_bytes([7; 32]);
        let correspondence = OnePair { digest: base, object: object_at(&bare) };
        assert!(
            derive_pipeline_manifest(repo.path(), Some(&correspondence), base)
                .expect("a base with no manifest is not a refusal")
                .is_none()
        );
        assert!(
            derive_pipeline_manifest(repo.path(), None, base).expect("an unresolvable base is not a refusal").is_none()
        );
    }

    #[test]
    fn a_manifest_that_will_not_decode_is_refused_and_says_why() {
        // Present-and-unreadable is not the bootstrap case: the file is there
        // because somebody edited it, so the refusal reaches the person holding
        // the diff. It must never fall back to the compiled vocabulary.
        let repo = repository();
        write(repo.path(), PIPELINE_MANIFEST_PATH, "version = 2\n[lanes.future]\nshape = \"unknown\"\n");
        let future = commit(repo.path(), "a manifest from a later coordinator");

        let base = Digest::from_bytes([7; 32]);
        let correspondence = OnePair { digest: base, object: object_at(&future) };
        assert_eq!(
            derive_pipeline_manifest(repo.path(), Some(&correspondence), base),
            Err(PipelineManifestError::UnsupportedVersion { declared: 2 }),
        );
    }

    fn object_at(commit: &str) -> BackendObjectId {
        BackendObjectId::from(GitObjectId::from_hex(commit).expect("a fixture commit is a git object"))
    }

    fn repository() -> TempDir {
        let dir = tempfile::tempdir().expect("a temp dir for the fixture creates");
        git(dir.path(), &["init", "--object-format=sha1", "--quiet"]);
        git(dir.path(), &["config", "user.name", "pipeline-manifest"]);
        git(dir.path(), &["config", "user.email", "pipeline-manifest@test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.autocrlf", "false"]);
        dir
    }

    fn write(root: &Path, relative: &str, contents: &str) {
        fs::write(root.join(relative), contents).expect("the fixture file writes");
    }

    fn commit(root: &Path, message: &str) -> String {
        git(root, &["add", "-A"]);
        git(root, &["commit", "--quiet", "--message", message]);
        let output = Command::new("git").current_dir(root).args(["rev-parse", "HEAD"]).output().expect("git starts");
        assert!(output.status.success(), "git rev-parse HEAD failed");
        String::from_utf8(output.stdout).expect("HEAD is utf-8").trim().to_owned()
    }

    fn git(root: &Path, args: &[&str]) {
        let output = Command::new("git").current_dir(root).args(args).output().expect("git starts");
        assert!(output.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&output.stderr));
    }
}
