//! The checkout's pipeline manifest: the vocabulary the verify lane fans out
//! to, interns failure names against, and renders into the evidence mask.
//!
//! ADR-0215 collapses the independently compiled copies of that vocabulary.
//! The lane reads [`PIPELINE_MANIFEST_PATH`] from the tree it is running in —
//! the checkout is the working directory — and a checkout that does not carry
//! the file keeps the compiled vocabulary until slice 9 (#5819) refuses it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aether_bloomery::{PIPELINE_MANIFEST_PATH, PipelineManifest};

/// The vocabulary this checkout declares, or the compiled one when the tree
/// carries no manifest.
///
/// Cached for the process: one umbrella pass asks more than once (preflight,
/// fan-out, intern), and the file does not change under a lane.
pub(super) fn checkout_vocabulary() -> &'static PipelineManifest {
    static VOCABULARY: OnceLock<PipelineManifest> = OnceLock::new();
    VOCABULARY.get_or_init(load_vocabulary)
}

fn load_vocabulary() -> PipelineManifest {
    match located_manifest_path() {
        None => {
            // A checkout with no pipeline.toml keeps the compiled vocabulary in
            // the lane for now. Refusal of a manifestless base is slice 9 (#5819).
            PipelineManifest::compiled()
        }
        Some(path) => {
            let text = fs::read_to_string(&path).unwrap_or_else(|err| panic!("read {}: {err}", path.display()));
            PipelineManifest::from_toml(&text)
                .unwrap_or_else(|err| panic!("{PIPELINE_MANIFEST_PATH} is not a usable pipeline manifest: {err}"))
        }
    }
}

/// `pipeline.toml` at the checkout root, or beside this crate when tests run
/// with `xtask/` as the working directory.
fn located_manifest_path() -> Option<PathBuf> {
    let cwd = PathBuf::from(PIPELINE_MANIFEST_PATH);
    if cwd.is_file() {
        return Some(cwd);
    }
    let bundled = Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(PIPELINE_MANIFEST_PATH);
    bundled.is_file().then_some(bundled)
}
