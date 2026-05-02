//! Shared test helpers for per-component scenario tests (issue 460).
//!
//! Three component crates ship `tests/scenario.rs` that each:
//! - probe for a wgpu adapter (skip otherwise),
//! - locate their own pre-built wasm artifact under
//!   `target/wasm32-unknown-unknown/{release,debug}/`,
//! - sandbox `save://` to a per-process tempdir (per issue 464,
//!   passed to `TestBench::builder().namespace_roots(...)` instead
//!   of via env mutation),
//! - and close every `Script` invocation with a `Runner::run` +
//!   `assert!(report.passed, …)` postscript.
//!
//! Lifting this boilerplate into one place keeps the per-component
//! scenario files focused on the actual `Script` definitions and lets
//! every new component-test crate skip ~70 lines of mechanical setup.
//!
//! ## Why this lives in `aether-scenario` and not `aether-substrate-bundle`
//!
//! The signatures below reference both `TestBench` (from
//! `aether_substrate_bundle::test_bench`, post-ADR-0073) and
//! `Step` / `Script` (from scenario). `aether-scenario` already
//! depends on `aether-substrate-bundle`, so the helpers compose
//! freely here. Moving them the other direction would push
//! YAML-parsing and `Script` types into chassis-land, inverting the
//! layer the consolidation in ADR-0073 deliberately preserved.
//!
//! ## Usage
//!
//! ```ignore
//! use aether_scenario::test_helpers::*;
//! use aether_scenario::{Check, Script, Step};
//!
//! #[test]
//! fn smoke() {
//!     let Some(wasm_path) = require_runtime("aether_my_component") else {
//!         return;
//!     };
//!     let sandbox = init_save_sandbox("my-component");
//!     let script = Script {
//!         name: "smoke".to_owned(),
//!         steps: vec![/* … */],
//!     };
//!     let mut bench = aether_substrate_bundle::test_bench::TestBench::builder()
//!         .size(64, 48)
//!         .namespace_roots(test_namespace_roots(sandbox))
//!         .build()
//!         .expect("boot");
//!     run_or_panic(&mut bench, &script);
//! }
//! ```

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aether_substrate_bundle::capabilities::io::NamespaceRoots;
use aether_substrate_bundle::test_bench::TestBench;

use crate::{Runner, Script, Step};

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
/// `release` first, then `debug` so either build profile works.
/// Returns `None` if neither exists.
///
/// `crate_name` is the underscore-cased crate name as it appears in
/// the wasm filename (e.g. `"aether_camera_component"`,
/// `"aether_test_fixture_probe"`). The workspace target dir is
/// resolved via `CARGO_MANIFEST_DIR` of the calling integration test
/// (`crates/<crate>` → workspace root two levels up); helper's own
/// `CARGO_MANIFEST_DIR` is irrelevant because the wasm artifacts live
/// under the shared workspace target dir, which is the same for every
/// caller.
pub fn locate_component_wasm(crate_name: &str) -> Option<PathBuf> {
    let workspace = Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .expect("workspace root reachable from CARGO_MANIFEST_DIR");
    for profile in ["release", "debug"] {
        let path = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join(format!("{crate_name}.wasm"));
        if path.exists() {
            return Some(path);
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
pub fn require_runtime(crate_name: &str) -> Option<PathBuf> {
    let strict = std::env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    if !has_wgpu_adapter() {
        assert!(
            !strict,
            "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available",
        );
        eprintln!("skipping: no wgpu adapter available");
        return None;
    }
    match locate_component_wasm(crate_name) {
        Some(path) => Some(path),
        None => {
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
pub fn init_save_sandbox(label: &str) -> &'static Path {
    TEST_SAVE_DIR.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!(
            "aether-{label}-tests-{pid}",
            pid = std::process::id(),
        ));
        std::fs::create_dir_all(&dir).expect("create test save dir");
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
/// `aether.io.read` / `aether.mesh.load` / etc.
///
/// Panics if `init_save_sandbox` was never called in this process —
/// the helper resolves the dir from the same `OnceLock` that
/// `init_save_sandbox` populates.
pub fn write_fixture(name: &str, bytes: &[u8]) -> String {
    let dir = TEST_SAVE_DIR
        .get()
        .expect("init_save_sandbox must run before write_fixture");
    std::fs::write(dir.join(name), bytes).expect("write fixture");
    name.to_owned()
}

/// Build a `SendMail` step that fires a direct `aether.tick` to
/// `mailbox` so the next `Capture` frame sees fresh render-sink
/// emissions.
///
/// Background: `TestBench::capture` runs its frame with
/// `dispatch_tick=false` (capture is a state snapshot, not a tick
/// advance). The render sink's vert buffer is consumed-and-replaced
/// every frame, so a component that only emits geometry on `on_tick`
/// will paint nothing during the capture frame even though the
/// previous `Advance` ticked it. Pushing `aether.tick` to the
/// component's mailbox right before `Capture` queues a tick that
/// drains alongside the capture request, populating the buffer
/// before the offscreen render reads it.
pub fn tick_to(mailbox: &str) -> Step {
    Step::SendMail {
        recipient: mailbox.to_owned(),
        kind: "aether.tick".to_owned(),
        params: serde_yml::Value::Null,
    }
}

/// Run the script and panic with a structured failure report if any
/// step did not pass. Collapses the
/// `let report = Runner::run(...); assert!(report.passed, ...)` pair
/// every consumer file repeats per `Script`.
///
/// The panic message includes the script name (so a multi-script
/// test file identifies which script regressed) and the per-step
/// debug dump.
pub fn run_or_panic(bench: &mut TestBench, script: &Script) {
    let report = Runner::run(bench, script);
    assert!(
        report.passed,
        "{} failed:\n{:#?}",
        script.name, report.steps,
    );
}
