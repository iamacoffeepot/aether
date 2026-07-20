//! Failure-only diagnostic artifacts for direct `SubstrateHarness` visual
//! assertions (issue 2914).
//!
//! A widget-heavy `SubstrateHarness` scenario scores a captured frame through
//! `visual::run_checks` and asserts on the returned `FrameVerdict`, but
//! a failing `assert!` only leaves a scalar diagnostic in the test
//! log — the captured pixels are gone by the time a developer reads
//! it. [`ArtifactGuard`] closes that gap for direct Rust tests without
//! making exact golden images the primary oracle: a passing test
//! leaves no files behind, and a panicking one preserves the exact
//! frame it scored, one mask per requested check (generated from the
//! same region/background/tolerance partition `run_checks` used), and
//! the measurements that produced the verdict.
//!
//! ## Usage
//!
//! ```ignore
//! use aether_substrate_bundle::substrate_harness::ArtifactGuard;
//!
//! let mut guard = ArtifactGuard::arm("widget_panel_layout", png, checks, verdict.results);
//! // ... assertions on `verdict` that may panic ...
//! ```
//!
//! The guard is a plain `Drop` type: a normal return leaves `guard`
//! untouched and it writes nothing. An unwinding panic through the
//! guard's scope best-effort writes under
//! `target/substrate-harness-artifacts/<sanitized id>/`. A test that detects
//! failure through a `Result` return rather than a panic calls
//! [`ArtifactGuard::persist`] explicitly before returning `Err`.

use std::path::{Path, PathBuf};
use std::{env, fs, io, thread};

use aether_kinds::{FrameCheck, FrameCheckResult};
use aether_substrate::render;
use serde::Serialize;

use crate::visual::{self, Image, ImageError, decode_png};

/// Directory (under the resolved Cargo target root) every artifact
/// guard writes under.
const ARTIFACT_ROOT_DIRNAME: &str = "substrate-harness-artifacts";

/// Deterministic failure-artifact guard for one direct-Rust `SubstrateHarness`
/// visual assertion. See the module docs for the write contract.
#[must_use = "an ArtifactGuard only writes on drop-during-panic or an explicit `persist()` \
              call — bind it to a variable that stays alive across the assertions it guards"]
pub struct ArtifactGuard {
    id: String,
    actual_png: Vec<u8>,
    checks: Vec<FrameCheck>,
    results: Vec<FrameCheckResult>,
    expectation: Option<String>,
    reference_png: Option<Vec<u8>>,
    root: PathBuf,
    persisted: bool,
}

/// Failure surfaced by the guard's internal write path. Never
/// propagated as a panic — [`ArtifactGuard::persist`] catches it and
/// reports to stderr instead, so artifact I/O can't replace the
/// assertion failure that armed the guard.
#[derive(Debug, thiserror::Error)]
enum ArtifactWriteError {
    #[error("io error: {0}")]
    Io(#[from] io::Error),
    #[error("failed to decode a capture as PNG: {0}")]
    Decode(#[from] ImageError),
    #[error("failed to serialize measurements.json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("failed to encode a diagnostic PNG: {0}")]
    Encode(String),
}

/// `measurements.json` shape: the checks the test requested paired
/// index-for-index with the results they scored, plus the id and an
/// optional human expectation string.
#[derive(Serialize)]
struct Measurements<'a> {
    id: &'a str,
    expectation: Option<&'a str>,
    checks: &'a [FrameCheck],
    results: &'a [FrameCheckResult],
}

impl ArtifactGuard {
    /// Arm a guard for `id` around `actual_png` (the exact captured
    /// frame bytes a `SubstrateHarness` capture returned) and the ordered
    /// `checks` / `results` pair a `visual::run_checks` verdict scored
    /// from it — `results[i]` must be the score of `checks[i]`. A
    /// length mismatch between the two is tolerated at persist time
    /// (only the shared prefix is written) rather than panicking here,
    /// since arming the guard must never itself be the thing that
    /// fails a test.
    pub fn arm(
        id: impl Into<String>,
        actual_png: Vec<u8>,
        checks: Vec<FrameCheck>,
        results: Vec<FrameCheckResult>,
    ) -> Self {
        Self::arm_with_root(id, actual_png, checks, results, resolve_artifact_root())
    }

    /// Same as [`Self::arm`] but against an injected root instead of
    /// the resolved Cargo target dir — the unit tests below use this to
    /// point at a temporary directory.
    fn arm_with_root(
        id: impl Into<String>,
        actual_png: Vec<u8>,
        checks: Vec<FrameCheck>,
        results: Vec<FrameCheckResult>,
        root: PathBuf,
    ) -> Self {
        Self {
            id: id.into(),
            actual_png,
            checks,
            results,
            expectation: None,
            reference_png: None,
            root,
            persisted: false,
        }
    }

    /// Attach a one-line description of what the assertion expected —
    /// carried into `measurements.json` verbatim, no parsing.
    pub fn with_expectation(mut self, expectation: impl Into<String>) -> Self {
        self.expectation = Some(expectation.into());
        self
    }

    /// Attach an already-loaded reference PNG to diff against the
    /// actual capture. Only realized as `reference.png` +
    /// `difference.png` when its decoded dimensions match the actual
    /// capture's; a dimension mismatch is dropped silently (the
    /// actual/mask/measurements set still persists) rather than
    /// diffing pixels that don't correspond.
    pub fn with_reference_png(mut self, reference_png: Vec<u8>) -> Self {
        self.reference_png = Some(reference_png);
        self
    }

    /// Best-effort persist path. Idempotent — a second call (including
    /// the one `Drop` may make) is a no-op. Call this explicitly from a
    /// `Result`-returning test that detects failure without panicking;
    /// `Drop` calls it automatically when the guard unwinds through a
    /// panic. Never panics itself: a write failure is reported to
    /// stderr, naming either the written directory or the error.
    // Test-diagnostic surface, not a host code path — `actor_logs` /
    // `tracing` has no channel back to a `cargo test` runner. Reporting
    // deliberately ignores stderr write errors because this method also
    // runs during panic unwinding and must never trigger a second panic.
    pub fn persist(&mut self) {
        if self.persisted {
            return;
        }
        self.persisted = true;
        let result = self.write();
        let mut stderr = io::stderr().lock();
        report_persist(&mut stderr, &self.id, &result);
    }

    /// The actual write path: replaces only this guard's own
    /// deterministic leaf directory (`remove_dir_all` then
    /// `create_dir_all`) so a repeated failing run can't leave a stale
    /// `reference.png` / `difference.png` from an earlier arm that did
    /// carry a reference.
    fn write(&self) -> Result<PathBuf, ArtifactWriteError> {
        let dir = self.root.join(sanitize_id(&self.id));
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;

        fs::write(dir.join("actual.png"), &self.actual_png)?;

        let paired = self.checks.len().min(self.results.len());
        let measurements = Measurements {
            id: &self.id,
            expectation: self.expectation.as_deref(),
            checks: &self.checks[..paired],
            results: &self.results[..paired],
        };
        fs::write(dir.join("measurements.json"), serde_json::to_string_pretty(&measurements)?)?;

        let actual_image = decode_png(&self.actual_png)?;
        for (index, check) in self.checks.iter().take(paired).enumerate() {
            let mask = visual::diagnostic_mask(&actual_image, check);
            let mask_png = encode_diagnostic_png(&mask, actual_image.width, actual_image.height)?;
            fs::write(dir.join(format!("mask_{index}.png")), mask_png)?;
        }

        if let Some(reference_png) = &self.reference_png {
            let reference_image = decode_png(reference_png)?;
            if reference_image.width == actual_image.width && reference_image.height == actual_image.height {
                fs::write(dir.join("reference.png"), reference_png)?;
                let difference = absolute_rgb_difference(&actual_image, &reference_image);
                let difference_png = encode_diagnostic_png(&difference, actual_image.width, actual_image.height)?;
                fs::write(dir.join("difference.png"), difference_png)?;
            }
        }

        Ok(dir)
    }
}

impl Drop for ArtifactGuard {
    fn drop(&mut self) {
        if !self.persisted && thread::panicking() {
            self.persist();
        }
    }
}

fn report_persist<W: io::Write + ?Sized>(writer: &mut W, id: &str, result: &Result<PathBuf, ArtifactWriteError>) {
    match result {
        Ok(dir) => {
            let _ = writeln!(
                writer,
                "substrate-harness artifact guard '{id}': wrote failure artifacts to {}",
                dir.display()
            );
        }
        Err(error) => {
            let _ =
                writeln!(writer, "substrate-harness artifact guard '{id}': failed to write failure artifacts: {error}");
        }
    }
}

fn encode_diagnostic_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, ArtifactWriteError> {
    render::encode_png(rgba, width, height).map_err(ArtifactWriteError::Encode)
}

/// Pixel-wise absolute RGB difference between two same-size images,
/// opaque alpha — the per-pixel image a developer visually scans for
/// where the two frames disagree. Not `visual::mean_absolute_error`'s
/// scalar score; that stays the pass/fail similarity oracle, this is
/// purely a diagnostic rendering of the same disagreement.
fn absolute_rgb_difference(actual: &Image, reference: &Image) -> Vec<u8> {
    let mut out = vec![0u8; actual.rgba.len()];
    for ((out_pixel, actual_pixel), reference_pixel) in
        out.chunks_exact_mut(4).zip(actual.rgba.chunks_exact(4)).zip(reference.rgba.chunks_exact(4))
    {
        out_pixel[0] = actual_pixel[0].abs_diff(reference_pixel[0]);
        out_pixel[1] = actual_pixel[1].abs_diff(reference_pixel[1]);
        out_pixel[2] = actual_pixel[2].abs_diff(reference_pixel[2]);
        out_pixel[3] = 255;
    }
    out
}

/// Resolve the Cargo target root the same way
/// `test_helpers::locate_component_wasm` does: `CARGO_MANIFEST_DIR` two
/// levels up to the workspace root, then `CARGO_TARGET_DIR` if set,
/// else `<workspace>/target`.
///
/// # Panics
/// Panics if `CARGO_MANIFEST_DIR` does not have two ancestor
/// directories — fail-fast per ADR-0063: this crate's own manifest dir
/// is always `crates/aether-substrate-bundle`, two levels under the
/// workspace root.
#[allow(clippy::disallowed_methods)] // test-only: CARGO_TARGET_DIR is the standard cargo
// build-output override, not cap config — honor it the same way
// `locate_component_wasm` does.
fn resolve_artifact_root() -> PathBuf {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    let target_root = env::var_os("CARGO_TARGET_DIR").map_or_else(|| workspace.join("target"), PathBuf::from);
    target_root.join(ARTIFACT_ROOT_DIRNAME)
}

/// Collapse `id` into a single filesystem-safe leaf name: every
/// character outside `[A-Za-z0-9_-]` — including `/`, `\`, and `.` —
/// becomes `_`, so a path-separator or `..` component in a
/// caller-supplied id can never escape `root` (there is no separator
/// left in the sanitized name to escape with). An id that sanitizes to
/// nothing but underscores falls back to a fixed name rather than
/// writing into `root` itself.
fn sanitize_id(id: &str) -> String {
    let sanitized: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.trim_matches('_').is_empty() {
        "unnamed".to_owned()
    } else {
        sanitized
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;
    use std::panic::{self, AssertUnwindSafe};
    use std::path::Component;
    use std::time::{SystemTime, UNIX_EPOCH};
    use std::{env, process};

    use aether_kinds::FrameReduction;

    use super::*;

    /// A tiny valid 2×2 opaque-white PNG, encoded once and reused by
    /// every test below that just needs *a* decodable capture — the
    /// guard's own write path is what's under test, not PNG content.
    fn tiny_png() -> Vec<u8> {
        render::encode_png(&[255u8; 2 * 2 * 4], 2, 2).expect("encode tiny png")
    }

    fn checks_and_results() -> (Vec<FrameCheck>, Vec<FrameCheckResult>) {
        let check = FrameCheck { reduction: FrameReduction::NotAllBlack, tolerance: 0, background: None, region: None };
        let result = FrameCheckResult::NotAllBlack { passed: true, detail: None };
        (vec![check], vec![result])
    }

    fn temp_root(label: &str) -> PathBuf {
        env::temp_dir().join(format!(
            "aether-artifact-guard-tests-{label}-{pid}-{nanos}",
            pid = process::id(),
            nanos = SystemTime::now().duration_since(UNIX_EPOCH).expect("system clock after epoch").as_nanos(),
        ))
    }

    struct FailingWriter;

    impl io::Write for FailingWriter {
        fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
            Err(io::Error::other("simulated stderr failure"))
        }

        fn flush(&mut self) -> io::Result<()> {
            Err(io::Error::other("simulated stderr failure"))
        }
    }

    #[test]
    fn normal_drop_writes_nothing() {
        let root = temp_root("normal-drop");
        let (checks, results) = checks_and_results();
        {
            let _guard = ArtifactGuard::arm_with_root("passing", tiny_png(), checks, results, root.clone());
            // Guard drops here without panicking.
        }
        assert!(!root.exists(), "a guard dropped without panicking must not create its artifact root at all");
    }

    #[test]
    fn panicking_drop_writes_the_expected_file_set() {
        let root = temp_root("panic-drop");
        let (checks, results) = checks_and_results();
        let root_for_guard = root.clone();
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            let _guard = ArtifactGuard::arm_with_root("failing", tiny_png(), checks, results, root_for_guard);
            panic!("simulated assertion failure");
        }));
        assert!(outcome.is_err(), "test setup: the closure must panic");

        let dir = root.join("failing");
        assert!(dir.join("actual.png").is_file(), "actual.png must exist");
        assert!(dir.join("measurements.json").is_file(), "measurements.json must exist");
        assert!(dir.join("mask_0.png").is_file(), "mask_0.png must exist for the single armed check");
        assert!(!dir.join("reference.png").exists(), "no reference.png without an attached reference");
        assert!(!dir.join("difference.png").exists(), "no difference.png without an attached reference");

        let entries: BTreeSet<String> = fs::read_dir(&dir)
            .expect("read artifact dir")
            .map(|entry| entry.expect("dir entry").file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            entries,
            ["actual.png", "measurements.json", "mask_0.png"].into_iter().map(str::to_owned).collect(),
            "the panic-triggered write should produce exactly the no-reference file set",
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn matching_reference_produces_difference_image() {
        let root = temp_root("reference-diff");
        let (checks, results) = checks_and_results();
        let mut guard = ArtifactGuard::arm_with_root("with_reference", tiny_png(), checks, results, root.clone())
            .with_reference_png(tiny_png());
        guard.persist();

        let dir = root.join("with_reference");
        assert!(dir.join("reference.png").is_file());
        assert!(dir.join("difference.png").is_file());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn expectation_is_written_to_measurements() {
        let root = temp_root("expectation");
        let (checks, results) = checks_and_results();
        let mut guard = ArtifactGuard::arm_with_root("expected", tiny_png(), checks, results, root.clone())
            .with_expectation("the widget remains centered");
        guard.persist();

        let measurements: serde_json::Value =
            serde_json::from_slice(&fs::read(root.join("expected/measurements.json")).expect("read measurements.json"))
                .expect("decode measurements.json");
        assert_eq!(measurements["expectation"].as_str(), Some("the widget remains centered"),);

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mismatched_dimension_reference_is_dropped_silently() {
        let root = temp_root("reference-mismatch");
        let (checks, results) = checks_and_results();
        let mismatched = render::encode_png(&[0u8; 4 * 4 * 4], 4, 4).expect("encode 4x4 png");
        let mut guard = ArtifactGuard::arm_with_root("mismatched", tiny_png(), checks, results, root.clone())
            .with_reference_png(mismatched);
        guard.persist();

        let dir = root.join("mismatched");
        assert!(dir.join("actual.png").is_file(), "the actual/mask/measurements set still persists");
        assert!(!dir.join("reference.png").exists(), "a dimension-mismatched reference must not be written");
        assert!(!dir.join("difference.png").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn repeated_persist_cannot_leave_stale_optional_files() {
        let root = temp_root("repeated-persist");
        let (checks, results) = checks_and_results();

        // First arm carries a reference and persists reference.png +
        // difference.png.
        let mut with_reference =
            ArtifactGuard::arm_with_root("flaky", tiny_png(), checks.clone(), results.clone(), root.clone())
                .with_reference_png(tiny_png());
        with_reference.persist();
        let dir = root.join("flaky");
        assert!(dir.join("reference.png").is_file(), "test setup: first persist wrote a reference");

        // A second arm at the same id, no reference this time, must
        // wipe the first run's optional files rather than leaving them
        // stranded alongside the new (referenceless) set.
        let mut without_reference = ArtifactGuard::arm_with_root("flaky", tiny_png(), checks, results, root.clone());
        without_reference.persist();
        assert!(dir.join("actual.png").is_file());
        assert!(
            !dir.join("reference.png").exists(),
            "a later referenceless persist must not leave the earlier reference.png behind",
        );
        assert!(!dir.join("difference.png").exists());

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn persist_is_idempotent_after_the_first_write() {
        let root = temp_root("idempotent");
        let (checks, results) = checks_and_results();
        let mut guard = ArtifactGuard::arm_with_root("once", tiny_png(), checks, results, root.clone());
        guard.persist();
        let dir = root.join("once");
        // Mutate the on-disk file so a second write would be observable,
        // then call persist again — it must be a no-op.
        fs::write(dir.join("actual.png"), b"mutated").expect("mutate actual.png");
        guard.persist();
        let contents = fs::read(dir.join("actual.png")).expect("read actual.png");
        assert_eq!(contents, b"mutated", "a second persist() call must be a no-op and not overwrite the mutated file");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn sanitize_id_collapses_hostile_paths_to_safe_leaf_names() {
        for (unsafe_id, expected) in [
            ("../../etc/passwd", "______etc_passwd"),
            ("..", "unnamed"),
            ("/etc/passwd", "_etc_passwd"),
            ("a/b/../../c", "a_b_______c"),
        ] {
            let sanitized = sanitize_id(unsafe_id);
            assert_eq!(sanitized, expected, "unexpected mapping for {unsafe_id:?}");
            let mut components = Path::new(&sanitized).components();
            assert!(
                matches!(components.next(), Some(Component::Normal(_))),
                "sanitized id {sanitized:?} must be one normal path component",
            );
            assert!(components.next().is_none(), "sanitized id {sanitized:?} must not contain a second path component");
        }
    }

    #[test]
    fn sanitized_id_persists_inside_the_injected_root() {
        let root = temp_root("safe-path-smoke");
        let safe_id = sanitize_id("../../etc/passwd");
        let (checks, results) = checks_and_results();
        let mut guard = ArtifactGuard::arm_with_root(safe_id.clone(), tiny_png(), checks, results, root.clone());
        guard.persist();
        assert!(
            root.join(safe_id).join("actual.png").is_file(),
            "the already-sanitized leaf should persist beneath the injected root",
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn mismatched_checks_and_results_persist_the_shared_prefix_without_panicking() {
        let (checks, results) = checks_and_results();
        let check = checks[0].clone();
        let result = results[0].clone();
        for (label, checks, results) in [
            ("checks-longer", vec![check.clone(), check.clone()], vec![result.clone()]),
            ("results-longer", vec![check], vec![result.clone(), result]),
        ] {
            let root = temp_root(label);
            let root_for_guard = root.clone();
            let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
                let mut guard = ArtifactGuard::arm_with_root(label, tiny_png(), checks, results, root_for_guard);
                guard.persist();
            }));
            assert!(outcome.is_ok(), "a {label} mismatch must not panic: {outcome:?}");

            let dir = root.join(label);
            let measurements: serde_json::Value =
                serde_json::from_slice(&fs::read(dir.join("measurements.json")).expect("read measurements.json"))
                    .expect("decode measurements.json");
            assert_eq!(measurements["checks"].as_array().map(Vec::len), Some(1));
            assert_eq!(measurements["results"].as_array().map(Vec::len), Some(1));
            assert!(dir.join("mask_0.png").is_file());
            assert!(!dir.join("mask_1.png").exists());

            let _ = fs::remove_dir_all(&root);
        }
    }

    #[test]
    fn report_persist_ignores_writer_errors() {
        let outcome = panic::catch_unwind(|| {
            let mut writer = FailingWriter;
            let success = Ok(PathBuf::from("artifacts"));
            report_persist(&mut writer, "success", &success);
            let failure = Err(ArtifactWriteError::Io(io::Error::other("simulated artifact failure")));
            report_persist(&mut writer, "failure", &failure);
        });
        assert!(outcome.is_ok(), "a reporter write error must not panic: {outcome:?}");
    }

    #[test]
    fn write_failure_is_reported_without_double_panicking() {
        // Point the root at a path that can't be a directory (a file
        // sits where the guard needs to create one), forcing
        // `create_dir_all` to fail. `persist()` must swallow the error
        // rather than propagating a second panic.
        let base = temp_root("write-failure");
        fs::create_dir_all(&base).expect("create base dir");
        let blocking_file = base.join("blocked");
        fs::write(&blocking_file, b"not a directory").expect("write blocking file");

        let (checks, results) = checks_and_results();
        let outcome = panic::catch_unwind(AssertUnwindSafe(|| {
            // `blocking_file` itself is the "root": the guard tries to
            // create `<blocking_file>/<sanitized id>` under a path
            // component that is a plain file, which must fail.
            let mut guard = ArtifactGuard::arm_with_root("id", tiny_png(), checks, results, blocking_file.clone());
            guard.persist();
        }));
        assert!(outcome.is_ok(), "a write failure inside persist() must not panic: {outcome:?}");

        let _ = fs::remove_dir_all(&base);
    }
}
