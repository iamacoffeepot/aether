//! The one-step run a cargo vendor asks the workspace for.
//!
//! `Outcome::tree` is `/work` after the last step minus scratch, and a mount
//! path is never read back (ADR-0237 decisions 2 and 8), so `/work` itself is
//! the vendor directory: the run tree is empty, the source is a read-only
//! mount, and cargo vendors into `/work`.
//!
//! Every argument is fixed, so the run key (ADR-0237 decision 9) is the same
//! for every vendor run in one environment, and they share one allotment
//! estimate.

use aether_bloomery_kinds::{Ref, Refusal, Tree};
use aether_workspace::{EnvVar, Mount, Mounts, Network, Run, Scratch, Step, Steps, ToolName, TreePath};

use super::{VendorInput, refused};

/// The tool the step runs, resolved through the environment's `tools` table.
const TOOL: &str = "cargo";

/// Where the source tree is mounted, relative to the root: `/source`.
const SOURCE: &str = "source";

/// The scratch path that holds temporary files and cargo's home.
///
/// Without `--no-delete`, cargo vendor clears every entry of its destination
/// whose name is not hidden before it writes, and the destination is `/work`,
/// where each scratch path is a tmpfs mount point. The dot keeps the scratch
/// mount out of that sweep.
const TMP_SCRATCH: &str = ".tmp";

/// Cargo's home, under [`TMP_SCRATCH`]: the root is read-only, and anything
/// outside scratch would land in the vendor tree.
const CARGO_HOME: &str = "/work/.tmp/cargo-home";

/// Temporary files, at [`TMP_SCRATCH`].
const TMPDIR: &str = "/work/.tmp";

/// The variables each run gives the step, over the environment's own.
const ENV: [(&str, &str); 2] = [("CARGO_HOME", CARGO_HOME), ("TMPDIR", TMPDIR)];

/// Vendor the crate sources the source's `Cargo.lock` names, refusing a stale
/// lock, into `/work`.
const ARGS: [&str; 5] = ["vendor", "--locked", "--manifest-path", "/source/Cargo.toml", "/work"];

/// The run that vendors `input.source`'s locked crates in `input.environment`
/// with the network on.
///
/// The run tree is the empty tree, so `/work` starts empty and holds only what
/// cargo vendor writes.
///
/// # Errors
///
/// A [`Refusal::Refused`] naming the value a request constructor refused.
pub(super) fn request(input: &VendorInput) -> Result<Run, Refusal> {
    let step = Step {
        tool: ToolName::new(TOOL).map_err(|error| refused(format!("tool {TOOL:?}: {error}")))?,
        args: ARGS.iter().map(|&arg| arg.to_owned()).collect(),
        env: ENV
            .iter()
            .map(|&(key, value)| EnvVar::new(key, value).map_err(|error| refused(format!("variable {key}: {error}"))))
            .collect::<Result<Vec<EnvVar>, Refusal>>()?,
        stdin: None,
    };
    let source = Mount { at: path(SOURCE)?, tree: input.source };

    Ok(Run {
        tree: Ref::of_encoded(&Tree::empty()).map_err(|error| refused(format!("the empty tree: {error}")))?,
        environment: input.environment,
        mounts: Mounts::new(vec![source]).map_err(|error| refused(format!("the source mount: {error}")))?,
        steps: Steps::new(vec![step]).map_err(|error| refused(format!("the vendor step: {error}")))?,
        scratch: Scratch::new(vec![path(TMP_SCRATCH)?])
            .map_err(|error| refused(format!("the scratch paths: {error}")))?,
        network: Network::On,
    })
}

/// One in-tree path the request spells out itself.
fn path(value: &str) -> Result<TreePath, Refusal> {
    TreePath::new(value).map_err(|error| refused(format!("path {value:?}: {error}")))
}

#[cfg(test)]
mod tests {
    use aether_bloomery_kinds::{Digest, Ref, Tree};
    use aether_workspace::Mounts;

    use super::{ENV, VendorInput, request};

    #[test]
    fn the_output_can_hold_only_what_cargo_vendor_writes() {
        // Catches a cargo home that lands in the vendor tree, a scratch mount
        // point cargo vendor would try to delete (a failure that shows only
        // live), and a regression to source-at-/work, which would put the
        // source in the vendor tree.
        let input = VendorInput {
            source: Ref::from_digest(Digest::from_bytes([1; 32])),
            environment: Ref::from_digest(Digest::from_bytes([2; 32])),
        };
        let run = request(&input).expect("the fixed request builds");
        let [step] = run.steps.as_slice() else {
            panic!("the request holds one step");
        };

        for (key, _) in ENV {
            let value = step.env.iter().find(|var| var.key() == key).expect("the step sets the variable").value();
            let in_scratch = run.scratch.as_slice().iter().any(|scratch| {
                let root = format!("/{}/{}", Mounts::WORK, scratch.as_str());
                value == root || value.starts_with(&format!("{root}/"))
            });
            assert!(in_scratch, "{key}={value} lies outside every /work/<scratch>");
        }

        for scratch in run.scratch.as_slice() {
            let first = scratch.as_str().split('/').next().unwrap_or_default();
            assert!(first.starts_with('.'), "scratch {scratch:?} is swept by cargo vendor's destination clear");
        }

        assert_eq!(run.tree, Ref::of_encoded(&Tree::empty()).expect("the empty tree encodes"), "/work starts empty");
    }
}
