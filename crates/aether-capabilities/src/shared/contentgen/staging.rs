//! `gen/` output staging below the content-gen caps' configured root.
//!
//! Generated binary artifacts (PNG from Nano Banana, WAV from Lyria)
//! never ride the mail wire — the cap stages the bytes to disk and the
//! reply carries the resolved `gen/<uuid>.<ext>` path instead
//! (`feedback_no_bytes_in_llm_json`). This module owns the staging
//! convention so both providers (issue 1014's text cap doesn't need it;
//! issue 1015's media cap does) write through one path.
//!
//! Root injection: the staging root is resolved once at chassis boot
//! (from [`ContentGenConfig`](super::config::ContentGenConfig), which
//! overrides via `AETHER_GEN_DIR`, else tracks the `save`-namespace root
//! the `aether.fs` cap resolves) and threaded into the cap, which passes
//! it to [`stage_gen_output_under`] at stage time. The staged file lands
//! under `gen/` within that root and writes atomically via the existing
//! `LocalFileAdapter` (tmp + rename), so a crash mid-write leaves no torn
//! file.

use std::path::Path;

use crate::fs::{Access, FsError};
use uuid::Uuid;

use crate::fs::{FileAdapter, LocalFileAdapter};

/// Subdirectory under the staging root every generated artifact lands
/// in. Mail and replies refer to staged files as `gen/<uuid>.<ext>`,
/// so a component can read one back via `aether.fs.read { namespace:
/// "save", path: "gen/<uuid>.<ext>" }`.
pub const GEN_PREFIX: &str = "gen";

/// Stage `bytes` as a fresh `gen/<uuid>.<ext>` file under `root` and
/// return the relative path the reply carries. Writes atomically via the
/// `save`-namespace `LocalFileAdapter` (the same tmp + rename the
/// `aether.fs` cap uses). `ext` is the extension without the dot
/// (`"png"`, `"wav"`).
///
/// `root` is the staging root resolved once at chassis boot and threaded
/// into the cap. The returned path is namespace-relative
/// (`gen/<uuid>.<ext>`), not absolute — a component reads it back with
/// `aether.fs.read { namespace: "save", path }`.
pub fn stage_gen_output_under(root: &Path, bytes: &[u8], ext: &str) -> Result<String, FsError> {
    let adapter = LocalFileAdapter::new(root.to_path_buf(), Access::ReadWrite)
        .map_err(|e| FsError::AdapterError(e.to_string()))?;
    let path = format!("{GEN_PREFIX}/{}.{ext}", Uuid::new_v4());
    adapter.write(&path, bytes)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::{GEN_PREFIX, stage_gen_output_under};
    use crate::fs::{Access, FileAdapter, LocalFileAdapter};
    use aether_substrate::testing::{cleanup, scratch_dir};
    use std::path::PathBuf;

    fn scratch_root(tag: &str) -> PathBuf {
        scratch_dir("aether-gen-stage", tag)
    }

    #[test]
    fn staged_path_round_trips_through_save_adapter() {
        let root = scratch_root("roundtrip");
        let bytes = b"\x89PNG fake artifact bytes";
        let path = stage_gen_output_under(&root, bytes, "png").expect("test setup: staging writes the artifact");
        assert!(path.starts_with(&format!("{GEN_PREFIX}/")));
        assert_eq!(path.rsplit('.').next(), Some("png"));
        // Read it back through a fresh adapter on the same root — the
        // file exists and the bytes round-trip verbatim.
        let adapter = LocalFileAdapter::new(root.clone(), Access::ReadWrite)
            .expect("test setup: adapter constructs on scratch root");
        assert_eq!(adapter.read(&path).expect("test setup: staged file reads back"), bytes);
        cleanup(&root);
    }

    #[test]
    fn distinct_calls_yield_distinct_paths() {
        let root = scratch_root("distinct");
        let a = stage_gen_output_under(&root, b"a", "wav").expect("test setup: first stage writes");
        let b = stage_gen_output_under(&root, b"b", "wav").expect("test setup: second stage writes");
        assert_ne!(a, b, "uuid name generation must not collide");
        cleanup(&root);
    }
}
