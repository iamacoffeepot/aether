//! The per-run scratch directory the model lanes hand their child.
//!
//! A model lane's child reasons — correctly — that reusing the lane's shared
//! `CARGO_TARGET_DIR` across divergent source is unsound, so it builds
//! throwaway target directories of its own. Left to choose where, it picks the
//! system temp dir, and tens of gigabytes per run accumulate on the root
//! filesystem until nothing on the host can compile: every later lane then fails
//! before it produces a byte, and empty evidence burns a member's whole retry
//! budget. So the lane names the location instead of leaving it to the child —
//! one directory per run, exported as `AETHER_LANE_SCRATCH`, reaped when the run
//! settles.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::{env, fs, io, process};

use anyhow::{Context, Result};

/// The variable a model lane's child reads to find its scratch directory — the
/// name `construct_instructions.md` tells the model to build under, and the name
/// the host exports to point the lane at a volume with room.
const LANE_SCRATCH: &str = "AETHER_LANE_SCRATCH";

/// One run's scratch directory: created before the child forks, exported to it,
/// and reaped when the run settles.
pub(super) struct Scratch {
    path: PathBuf,
}

impl Scratch {
    /// Prepare the directory this run owns and hold it until the run ends.
    pub(super) fn prepare(out: &Path, nonce: Option<&str>) -> Result<Self> {
        let path = run_dir(host_root().as_deref(), out, nonce);
        fs::create_dir_all(&path).with_context(|| format!("create {}", path.display()))?;
        // Absolute, because the child runs cargo from wherever it likes and a
        // relative `CARGO_TARGET_DIR` would land somewhere the reap never looks.
        Ok(Self { path: fs::canonicalize(&path).unwrap_or(path) })
    }

    /// Point `command`'s child at this run's directory.
    pub(super) fn export(&self, command: &mut Command) {
        command.env(LANE_SCRATCH, &self.path);
    }
}

impl Drop for Scratch {
    /// Reap this run's own directory and nothing else — a lane running beside it
    /// holds a differently-keyed one, so a concurrent build tree is never in the
    /// blast radius. Best-effort: a directory that will not delete is worth a
    /// word on stderr, not a failed dispatch whose evidence is already written.
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path)
            && error.kind() != io::ErrorKind::NotFound
        {
            eprintln!("lane scratch: could not reap {}: {error}", self.path.display());
        }
    }
}

/// The scratch root the host exported, when it named a non-empty one.
#[allow(clippy::disallowed_methods)] // the host's scratch volume is an external var, not cap config.
fn host_root() -> Option<PathBuf> {
    env::var_os(LANE_SCRATCH).map(PathBuf::from).filter(|root| !root.as_os_str().is_empty())
}

/// The directory this run owns, under the host's scratch `root` when it named
/// one and under the run's own evidence `out` tree when it did not — never an
/// empty path, which the child would resolve against its cwd.
///
/// The fallback keeps the build tree per-run and on the volume the checkout
/// already lives on, which is the property the system temp dir lacks; a host
/// with a roomier volume points `AETHER_LANE_SCRATCH` at it.
fn run_dir(root: Option<&Path>, out: &Path, nonce: Option<&str>) -> PathBuf {
    root.unwrap_or(out).join(format!("scratch-{}", run_key(nonce)))
}

/// The run's directory-name key: the broker nonce it was dispatched under, or
/// this process's id when the dispatch carried none. Two runs sharing a host
/// root must not share a directory, since reaping one would delete the other's
/// build out from under it.
///
/// A nonce is coordinator-supplied text, so anything outside the path-safe set
/// becomes a dash: a separator in it would place the directory somewhere else
/// entirely, where the reap would never find it.
fn run_key(nonce: Option<&str>) -> String {
    nonce.filter(|nonce| !nonce.is_empty()).map_or_else(
        || process::id().to_string(),
        |nonce| {
            nonce
                .chars()
                .map(|character| {
                    if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                        character
                    } else {
                        '-'
                    }
                })
                .collect()
        },
    )
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};

    use super::{run_dir, run_key};

    // Two lanes on one host must never share a scratch directory: the reap
    // removes the whole tree, so a shared path would delete a live run's build
    // output out from under it. The per-run key is what keeps them apart.
    #[test]
    fn each_run_owns_a_distinct_directory_under_the_hosts_root() {
        let root = PathBuf::from("/mnt/scratch");
        let out = Path::new(".bloomery/local-worktrees/n-evidence");
        let mine = run_dir(Some(&root), out, Some("nonce-7"));

        assert!(mine.starts_with(&root), "the host's volume is where the build tree goes");
        assert_ne!(mine, run_dir(Some(&root), out, Some("nonce-8")), "a concurrent lane's tree is never reaped here");
    }

    // A host that named no root still hands the child a real path — under the
    // run's own evidence tree, per-run and on the checkout's volume — rather than
    // an empty one it would resolve against its cwd.
    #[test]
    fn an_unset_root_falls_back_to_the_runs_own_output_tree() {
        let out = Path::new(".bloomery/local-worktrees/n-evidence");
        assert!(run_dir(None, out, Some("nonce-7")).starts_with(out));
        assert!(run_dir(None, out, None).starts_with(out), "a nonce-less dispatch still lands under the out tree");
    }

    // A nonce carrying a separator would steer the directory out of the root, and
    // the reap would then leave whatever it built behind.
    #[test]
    fn a_nonce_cannot_steer_the_directory_out_of_the_root() {
        assert_eq!(run_key(Some("../../etc")), "------etc");
        assert!(!run_key(Some("a/b")).contains('/'));
        assert!(!run_key(Some("")).is_empty(), "an empty nonce still names a directory");
    }
}
