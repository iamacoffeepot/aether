//! ADR-maturity derivation for the pre-seal hard gate.
//!
//! A glob matches paths, not maturity, so admission cannot classify a
//! `docs/adr` surface as still-Proposed from shape alone. The sealed base's
//! `Status:` line (or absence of the file) is the authority. The blob is the
//! git object at that base, never the process working tree: a stale local
//! `Proposed` must not waive the Human hard gate on an established-at-base
//! ADR, and a stale local `Accepted` must not refuse a still-Proposed one.

use std::path::{Path, PathBuf};

use aether_bloomery::{Correspondence, Digest, SurfacePattern};
use aether_bloomery_git::{GitObjectId, command};

use crate::bloomery::AdrTouch;

/// Status of one ADR file at the sealed base.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SealedAdrStatus {
    /// Present and still `Proposed` — defers to the tier policy.
    Proposed,
    /// Present with any other status (including an unreadable `Status:` line).
    Established,
}

/// Look up an ADR path at the sealed base.
///
/// [`Some(SealedAdrStatus::Proposed)`] is the only confirmed still-Proposed
/// blob. [`Some(Established)`] is a confirmed non-Proposed status. [`None`]
/// is everything else: a missing path, unresolved correspondence, a missing
/// git object, or a spawn/decode failure. None of those are confirmed
/// Proposed, so the Human hard gate stays armed. [`None`] is not a confirmed
/// absence — an I/O failure and a missing file are indistinguishable here.
pub trait AdrMaturity {
    fn status(&self, path: &str) -> Option<SealedAdrStatus>;
}

/// Every path is absent at the sealed base.
#[cfg(test)]
pub struct AbsentAdrs;

#[cfg(test)]
impl AdrMaturity for AbsentAdrs {
    fn status(&self, _path: &str) -> Option<SealedAdrStatus> {
        None
    }
}

/// ADR markdown as of one git commit in `repo`.
///
/// `commit` is a real git object name from correspondence, never a Bloomery
/// digest hex-pun. [`None`] means the sealed base could not be resolved;
/// every lookup then returns [`AdrMaturity::status`]'s uncertain [`None`]
/// (Human hard gate), never Proposed, and never falls back to cwd.
pub struct TreeAdrs {
    repo: PathBuf,
    commit: Option<String>,
}

impl AdrMaturity for TreeAdrs {
    fn status(&self, path: &str) -> Option<SealedAdrStatus> {
        blob_markdown(&self.repo, self.commit.as_deref()?, path).map(|text| status_from_markdown(&text))
    }
}

impl TreeAdrs {
    /// Git blobs at `commit` in `repo`. `repo` is the configured lane
    /// repository (a local bare authority, or the boot-time cwd), not the
    /// process working tree at seal time.
    pub fn at(repo: impl AsRef<Path>, commit: Option<String>) -> Self {
        Self { repo: repo.as_ref().to_path_buf(), commit }
    }

    /// Resolve the sealed-base commit through correspondence, then read blobs
    /// from `repo`. Unresolved correspondence yields an uncertain catalog.
    pub fn resolve(repo: impl AsRef<Path>, correspondence: Option<&dyn Correspondence>, base: Digest) -> Self {
        Self::at(repo, sealed_commit_hex(correspondence, base))
    }
}

/// The git object name correspondence recorded for `base`, or [`None`] when
/// the digest has no git object (including a default-zero draft base).
///
/// A Bloomery [`Digest`] is never a git sha. Reading `{digest-hex}:{path}`
/// would miss the sealed blob on today's sha1 repositories and is the hex-pun
/// ADR-0150 forbids.
fn sealed_commit_hex(correspondence: Option<&dyn Correspondence>, base: Digest) -> Option<String> {
    let object = correspondence?.resolve_backend_object(&base).ok()??;
    GitObjectId::try_from(object).ok().map(|id| id.to_hex())
}

/// Blob text at `commit:path`, or [`None`] when the read does not confirm a
/// status line. A missing path, a missing object, and a spawn/decode fault
/// all return [`None`] — uncertain, not a confirmed Proposed blob.
fn blob_markdown(repo: &Path, commit: &str, path: &str) -> Option<String> {
    let spec = format!("{commit}:{path}");
    let output = command::run(repo, &["cat-file", "-p", &spec]).ok()?;
    output.status.success().then_some(output.stdout).and_then(|bytes| String::from_utf8(bytes).ok())
}

/// Derive [`AdrTouch`] from a declared surface and the sealed-base catalog.
pub fn adr_touch(surface: &[String], maturity: &impl AdrMaturity) -> AdrTouch {
    let mut touch = AdrTouch::None;
    for glob in surface {
        match classify(glob, maturity) {
            AdrTouch::NewOrEstablished => return AdrTouch::NewOrEstablished,
            AdrTouch::ProposedOnly => touch = AdrTouch::ProposedOnly,
            AdrTouch::None => {}
        }
    }
    touch
}

fn classify(glob: &str, maturity: &impl AdrMaturity) -> AdrTouch {
    let Some(pattern) = SurfacePattern::parse(glob) else {
        return if glob.contains("docs/adr") {
            AdrTouch::NewOrEstablished
        } else {
            AdrTouch::None
        };
    };
    match pattern {
        SurfacePattern::Exact(path) if is_adr_mirror(&path) => match maturity.status(&path) {
            Some(SealedAdrStatus::Proposed) => AdrTouch::ProposedOnly,
            Some(SealedAdrStatus::Established) | None => AdrTouch::NewOrEstablished,
        },
        SurfacePattern::Exact(_) => AdrTouch::None,
        subtree @ SurfacePattern::Subtree(_) => {
            if SurfacePattern::parse("docs/adr/**").is_some_and(|tree| subtree.intersects(&tree)) {
                AdrTouch::NewOrEstablished
            } else {
                AdrTouch::None
            }
        }
    }
}

fn is_adr_mirror(path: &str) -> bool {
    let Some(name) = path.strip_prefix("docs/adr/") else {
        return false;
    };
    if name.contains('/') {
        return false;
    }
    let Some((number, rest)) = name.split_once('-') else {
        return false;
    };
    number.len() == 4
        && number.bytes().all(|byte| byte.is_ascii_digit())
        && rest.len() > 3
        && Path::new(rest).extension().is_some_and(|ext| ext.eq_ignore_ascii_case("md"))
}

fn status_from_markdown(text: &str) -> SealedAdrStatus {
    match status_token(text) {
        Some("Proposed") => SealedAdrStatus::Proposed,
        _ => SealedAdrStatus::Established,
    }
}

fn status_token(text: &str) -> Option<&str> {
    text.lines().find_map(|line| {
        let rest = line.trim().strip_prefix("- **Status:**")?.trim();
        rest.split([' ', '|', '(', '—']).find(|part| !part.is_empty())
    })
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;
    use std::process::Command;

    use aether_bloomery::{BackendObjectId, Correspondence, CorrespondenceError, Digest};
    use aether_bloomery_git::GitObjectId;
    use tempfile::TempDir;

    use super::{AdrMaturity, SealedAdrStatus, TreeAdrs, adr_touch, sealed_commit_hex, status_from_markdown};
    use crate::bloomery::AdrTouch;

    struct Markdown<'a>(&'a [(&'a str, &'a str)]);

    impl AdrMaturity for Markdown<'_> {
        fn status(&self, path: &str) -> Option<SealedAdrStatus> {
            self.0.iter().find(|(candidate, _)| *candidate == path).map(|(_, text)| status_from_markdown(text))
        }
    }

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
    fn an_established_status_line_is_new_or_established() {
        let path = "docs/adr/0001-record.md";
        assert_eq!(
            adr_touch(&[path.to_owned()], &Markdown(&[(path, "- **Status:** Accepted\n")])),
            AdrTouch::NewOrEstablished,
        );
    }

    #[test]
    fn a_bare_adr_subtree_is_new_or_established() {
        // A glob that cannot name a concrete file could add or amend anything
        // under docs/adr; the hard gate must not be talked around.
        assert_eq!(adr_touch(&["docs/adr/**".to_owned()], &Markdown(&[])), AdrTouch::NewOrEstablished);
    }

    #[test]
    fn status_from_markdown_reads_the_first_status_token() {
        assert_eq!(status_from_markdown("- **Status:** Proposed\n"), SealedAdrStatus::Proposed);
        assert_eq!(status_from_markdown("- **Status:** Proposed (parked)\n"), SealedAdrStatus::Proposed);
        assert_eq!(status_from_markdown("- **Status:** Accepted (shipped)\n"), SealedAdrStatus::Established);
        assert_eq!(status_from_markdown("- **Status:** Superseded by ADR-0038\n"), SealedAdrStatus::Established);
        assert_eq!(status_from_markdown("# no status line\n"), SealedAdrStatus::Established);
    }

    #[test]
    fn maturity_is_the_blob_at_the_commit_not_the_working_tree() {
        // Pre-fix, TreeAdrs joined cwd, so a stale local Proposed classified
        // an Accepted-at-base ADR as ProposedOnly and skipped the Human hard
        // gate. The inverse (local Accepted, base Proposed) refused valid work.
        let path = "docs/adr/0999-sealed-base.md";
        let repo = adr_repo();
        assert_ne!(
            repo.path().canonicalize().expect("fixture repo canonicalizes"),
            std::env::current_dir().expect("process cwd").canonicalize().expect("cwd canonicalizes"),
            "the configured repository must not be the process cwd",
        );
        write_adr(repo.path(), path, "Accepted");
        let accepted = commit(repo.path(), "accepted at base");
        write_adr(repo.path(), path, "Proposed");
        assert_eq!(
            adr_touch(&[path.to_owned()], &TreeAdrs::at(repo.path(), Some(accepted.clone()))),
            AdrTouch::NewOrEstablished,
            "cwd Proposed must not reclassify an Accepted blob at the sealed commit",
        );

        let proposed = commit(repo.path(), "later proposed rewrite");
        write_adr(repo.path(), path, "Accepted");
        assert_eq!(
            adr_touch(&[path.to_owned()], &TreeAdrs::at(repo.path(), Some(proposed.clone()))),
            AdrTouch::ProposedOnly,
            "cwd Accepted must not reclassify a still-Proposed blob at the named commit",
        );
        assert_eq!(
            adr_touch(&[path.to_owned()], &TreeAdrs::at(repo.path(), None)),
            AdrTouch::NewOrEstablished,
            "unresolved correspondence is uncertain and must not become Proposed",
        );
        assert_eq!(
            adr_touch(
                &[path.to_owned()],
                &TreeAdrs::at(repo.path(), Some("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa".to_owned())),
            ),
            AdrTouch::NewOrEstablished,
            "a missing git object is uncertain and must not become Proposed",
        );

        let base = Digest::from_bytes([7; 32]);
        let correspondence = OnePair {
            digest: base,
            object: BackendObjectId::from(GitObjectId::from_hex(&proposed).expect("the sealed commit is a git object")),
        };
        assert_eq!(
            adr_touch(&[path.to_owned()], &TreeAdrs::resolve(repo.path(), Some(&correspondence), base)),
            AdrTouch::ProposedOnly,
            "resolve must read the configured repo's Proposed blob, not cwd Accepted",
        );
        assert_eq!(
            adr_touch(&[path.to_owned()], &TreeAdrs::resolve(repo.path(), None, base)),
            AdrTouch::NewOrEstablished,
            "resolve without correspondence is uncertain and must not become Proposed",
        );
    }

    #[test]
    fn a_bare_authority_repo_is_the_object_database_not_cwd() {
        // Local authority names a bare repo. cat-file still answers; the
        // process working tree must not stand in.
        let path = "docs/adr/0999-sealed-base.md";
        let working = adr_repo();
        write_adr(working.path(), path, "Proposed");
        let proposed = commit(working.path(), "proposed at base");
        let bare = tempfile::tempdir().expect("a bare clone dir creates");
        let dest = bare.path().join("authority.git");
        git(working.path(), &["clone", "--bare", "--quiet", ".", dest.to_str().expect("utf-8 dest")]);
        write_adr(working.path(), path, "Accepted");

        assert_eq!(
            adr_touch(&[path.to_owned()], &TreeAdrs::at(&dest, Some(proposed.clone()))),
            AdrTouch::ProposedOnly,
            "the bare authority still has the Proposed blob after cwd was rewritten",
        );
        let other = adr_repo();
        assert_eq!(
            adr_touch(&[path.to_owned()], &TreeAdrs::at(other.path(), Some(proposed))),
            AdrTouch::NewOrEstablished,
            "a repository that does not hold the object is uncertain and must not become Proposed",
        );
    }

    #[test]
    fn sealed_commit_hex_is_the_git_object_not_the_digest() {
        // Tripwire: a Digest is sha256 over aether-wire bytes and is never a
        // git sha. Hex-punning it as `{digest}:{path}` misses the blob on a
        // sha1 repository and is the correspondence split ADR-0150 records.
        let sha = "3a3f8c0b9e1d2a4f6b8c0e2d4a6f8b0c1e3d5a7f";
        let base = Digest::from_bytes([7; 32]);
        let correspondence = OnePair {
            digest: base,
            object: BackendObjectId::from(GitObjectId::from_hex(sha).expect("40-hex sha1")),
        };

        let resolved = sealed_commit_hex(Some(&correspondence), base).expect("the recorded pair resolves");
        assert_eq!(resolved, sha);
        assert_ne!(resolved, base.to_hex(), "the git object name is not the digest hex");
        assert_eq!(sealed_commit_hex(Some(&correspondence), Digest::from_bytes([1; 32])), None);
        assert_eq!(sealed_commit_hex(None, base), None, "no correspondence is unresolved, not a digest hex-pun");
    }

    fn adr_repo() -> TempDir {
        let dir = tempfile::tempdir().expect("a temp dir for the fixture creates");
        git(dir.path(), &["init", "--object-format=sha1", "--quiet"]);
        git(dir.path(), &["config", "user.name", "adr-touch"]);
        git(dir.path(), &["config", "user.email", "adr-touch@test"]);
        git(dir.path(), &["config", "commit.gpgsign", "false"]);
        git(dir.path(), &["config", "core.autocrlf", "false"]);
        dir
    }

    fn write_adr(root: &Path, relative: &str, status: &str) {
        let path = root.join(relative);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("the ADR parent dir creates");
        }
        fs::write(&path, format!("- **Status:** {status}\n")).expect("the ADR file writes");
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
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr),
        );
    }
}
