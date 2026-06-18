//! Shared test helpers for in-process test-bench scenarios (issue
//! 460; relocated from `aether-scenario` per issue 821).
//!
//! Three concerns lifted out of every per-component scenario file:
//!
//! - probing for a wgpu adapter (the chassis won't boot without
//!   one; driverless dev boxes need to skip cleanly),
//! - locating the component's pre-built wasm under the workspace
//!   `target/wasm32-unknown-unknown/` tree,
//! - setting up a per-process `save://` sandbox the bench's
//!   `aether.fs` capability can read and write.
//!
//! These helpers don't reference any scenario vocabulary (Script /
//! Step / Check) — they live on the chassis side so any test that
//! drives a `TestBench` directly can call them, scenario crate or
//! not.
//!
//! ## Usage
//!
//! ```ignore
//! use aether_substrate_bundle::test_bench::{
//!     TestBench,
//!     test_helpers::{init_save_sandbox, require_runtime, test_namespace_roots},
//! };
//!
//! #[test]
//! fn smoke() {
//!     let Some(wasm_path) = require_runtime("aether_my_component") else {
//!         return;
//!     };
//!     let sandbox = init_save_sandbox("my-component");
//!     let mut bench = TestBench::builder()
//!         .size(64, 48)
//!         .namespace_roots(test_namespace_roots(sandbox))
//!         .build()
//!         .expect("boot");
//!     // … drive bench directly …
//! }
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use crate::capabilities::fs::NamespaceRoots;
use std::env;
use std::fs;
use std::process;

/// Process-wide test sandbox. Single `OnceLock` so repeat calls
/// across a binary's tests resolve to the same dir — handy for
/// `write_fixture` consumers that look up the sandbox by label.
///
/// Per issue 464, the sandbox is just a directory; `TestBench`
/// receives it via `TestBench::builder().namespace_roots(...)`, not
/// via env-var mutation. The `OnceLock` no longer linearises a
/// `set_var` call — it just memoises the path.
static TEST_SAVE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Probe for any usable wgpu adapter. Used by `require_runtime` and
/// by tests that need wgpu but not a wasm component (e.g. IO sink
/// scenarios in `aether-substrate-bundle`'s own test-bench tests).
#[must_use]
pub fn has_wgpu_adapter() -> bool {
    let instance =
        wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
        power_preference: wgpu::PowerPreference::default(),
        compatible_surface: None,
        force_fallback_adapter: false,
    }))
    .is_ok()
}

/// Locate `<crate_name>.wasm` under the workspace target dir. Tries
/// `release` first, then `debug` so either build profile works. Also
/// probes `examples/<crate_name>.wasm` so callers can name an
/// `[[example]] crate-type = ["cdylib"]` artifact (ADR-0090 c1 moved
/// the test-fixture probes to per-example cdylibs under
/// `aether-test-fixtures`). Returns `None` if no candidate path
/// exists.
///
/// `crate_name` is the underscore-cased crate name (for top-level
/// cdylib crates, e.g. `"aether_camera"`) or the example name (for
/// `[[example]]` cdylibs, e.g. `"probe"` / `"inline_child"`). The
/// workspace target dir is resolved via
/// `CARGO_MANIFEST_DIR` of the calling integration test
/// (`crates/<crate>` → workspace root two levels up); helper's own
/// `CARGO_MANIFEST_DIR` is irrelevant because the wasm artifacts live
/// under the shared workspace target dir, which is the same for every
/// caller.
///
/// # Panics
/// Panics if `CARGO_MANIFEST_DIR` does not have two ancestor
/// directories — fail-fast per ADR-0063: this helper only runs from
/// integration-test binaries (`crates/<crate>/tests/...`), so the
/// workspace root is always two levels up.
#[must_use]
pub fn locate_component_wasm(crate_name: &str) -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let base = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile);
        // Top-level cdylib crates land directly under the profile dir.
        let top_level = base.join(format!("{crate_name}.wasm"));
        if top_level.exists() {
            return Some(top_level);
        }
        // `[[example]] crate-type = ["cdylib"]` cdylibs land under
        // `<profile>/examples/<example_name>.wasm` (ADR-0090 c1).
        let example = base.join("examples").join(format!("{crate_name}.wasm"));
        if example.exists() {
            return Some(example);
        }
    }
    None
}

/// Skip-or-panic gate: probes wgpu + locates the wasm. Returns the
/// wasm path on success; `None` when the test should skip.
///
/// `AETHER_REQUIRE_RUNTIME=1` flips both skip points into a panic so
/// CI catches a forgotten pre-build entry instead of passing a 30 ms
/// vacuous test. CI sets this; local devs leave it unset and keep the
/// existing skip behavior.
///
/// # Panics
/// Panics in strict (`AETHER_REQUIRE_RUNTIME=1`) mode if either no
/// wgpu adapter is available or the named crate's wasm artifact is
/// not pre-built — fail-fast per ADR-0063: CI relies on the strict
/// mode to catch missing pre-build entries.
#[must_use]
// Test-only skip diagnostic — emitted from `cargo test` runners so a
// skipped test is visible alongside `test ... ok` lines. Not routed
// through `tracing` because the test harness already captures stderr
// and surfaces it on failure (issue 891).
#[allow(clippy::print_stderr)]
// Test-only: AETHER_REQUIRE_RUNTIME is the CI strict-mode toggle that turns a
// missing wgpu adapter / wasm pre-build from a skip into a hard failure — a test
// harness knob, not cap config.
#[allow(clippy::disallowed_methods)]
pub fn require_runtime(crate_name: &str) -> Option<PathBuf> {
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    if !has_wgpu_adapter() {
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
        );
        eprintln!("skipping: no wgpu adapter available");
        return None;
    }
    // The else arm runs side effects (assert + eprintln); `map_or_else`
    // would bury that under closures with no clarity win.
    #[allow(clippy::option_if_let_else)]
    if let Some(path) = locate_component_wasm(crate_name) {
        Some(path)
    } else {
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but {crate_name}.wasm not pre-built; \
             CI's `Pre-build component wasm for scenario tests` step is missing this crate",
        );
        eprintln!(
            "skipping: {crate_name}.wasm not built; \
             run `cargo build --target wasm32-unknown-unknown -p <crate>`",
        );
        None
    }
}

/// Process-wide `save://` sandbox dir. Idempotent; the dir is created
/// on the first call and the same path is returned on every
/// subsequent call. Per issue 464, this helper no longer mutates
/// process env — callers pass the returned path to
/// `TestBench::builder().namespace_roots(test_namespace_roots(path))`.
///
/// `label` is baked into the dirname so the tempdir is self-describing
/// (`/tmp/aether-<label>-tests-<pid>`); pass a stable per-crate label
/// like `"mesh-viewer"` or `"test-bench-io"`. Each integration test
/// binary is its own process, so the label is only ever set once per
/// process and collisions across binaries don't arise.
///
/// # Panics
/// Panics if the tempdir can't be created — fail-fast per ADR-0063:
/// a test that can't reserve its sandbox can't proceed.
pub fn init_save_sandbox(label: &str) -> &'static Path {
    TEST_SAVE_DIR.get_or_init(|| {
        let dir = env::temp_dir().join(format!("aether-{label}-tests-{pid}", pid = process::id()));
        fs::create_dir_all(&dir).expect("create test save dir");
        dir
    })
}

/// Build a [`NamespaceRoots`] suitable for a per-process test
/// sandbox. The supplied `save_dir` (typically the path returned by
/// [`init_save_sandbox`]) backs the `save://` namespace; `assets://`
/// and `config://` reuse the same dir so writes that target either
/// don't escape the sandbox. Pass the result to
/// `TestBench::builder().namespace_roots(...)`.
///
/// Per issue 464, this is the no-env replacement for the old
/// `init_save_sandbox`-sets-`AETHER_SAVE_DIR` pattern.
#[must_use]
pub fn test_namespace_roots(save_dir: &Path) -> NamespaceRoots {
    NamespaceRoots {
        save: save_dir.to_path_buf(),
        assets: save_dir.to_path_buf(),
        config: save_dir.to_path_buf(),
    }
}

/// Write `bytes` into the sandbox at filename `name`, returning the
/// bare filename — the substrate resolves it relative to the
/// namespace root, so callers pass this as the `path` field of
/// `aether.fs.read` / `aether.mesh.load` / etc.
///
/// # Panics
/// Panics if [`init_save_sandbox`] was never called in this process,
/// or if the file write fails — fail-fast per ADR-0063: the helper
/// resolves the dir from the same `OnceLock` that
/// `init_save_sandbox` populates, and a failed fixture write means
/// the test can't proceed.
pub fn write_fixture(name: &str, bytes: &[u8]) -> String {
    let dir = TEST_SAVE_DIR
        .get()
        .expect("init_save_sandbox must run before write_fixture");
    fs::write(dir.join(name), bytes).expect("write fixture");
    name.to_owned()
}
