//! Shared test helpers for in-process substrate-harness scenarios (issue
//! 460; relocated from `aether-scenario` per issue 821).
//!
//! Three concerns lifted out of every per-component scenario file:
//!
//! - probing for a wgpu adapter (the chassis won't boot without
//!   one; driverless dev boxes need to skip cleanly),
//! - locating the component's pre-built wasm under the workspace
//!   `target/wasm32-unknown-unknown/` tree,
//! - setting up a per-process `save://` sandbox the harness's
//!   `aether.fs` capability can read and write.
//!
//! These helpers don't reference any scenario vocabulary (Script /
//! Step / Check) — they live on the chassis side so any test that
//! drives a `SubstrateHarness` directly can call them, scenario crate or
//! not.
//!
//! ## Usage
//!
//! ```ignore
//! use aether_harness_substrate::{
//!     SubstrateHarness,
//!     test_helpers::{init_save_sandbox, require_wasm, test_namespace_roots},
//! };
//!
//! #[test]
//! fn smoke() {
//!     let Some(wasm_path) = require_wasm("aether_my_component") else {
//!         return;
//!     };
//!     let sandbox = init_save_sandbox("my-component");
//!     let mut harness = SubstrateHarness::builder()
//!         .size(64, 48)
//!         .namespace_roots(test_namespace_roots(sandbox))
//!         .build()
//!         .expect("boot");
//!     // … drive harness directly …
//! }
//! ```
//!
//! Visual scenarios that need the wgpu adapter probe use
//! `aether_harness_substrate_capture::test_helpers::require_runtime`
//! instead — the probe belongs with the GPU crate (issue #3765).
//!
//! Pre-build the wasm the gate looks for with `cargo xtask build-wasm`.
//! Without it `require_wasm` fails the scenario rather than skipping it
//! (issue #5724): a skip that reports `test … ok` is indistinguishable
//! from a pass, and the whole point of running the scenario is to learn
//! which of the two happened.

use aether_data::{ActorPath, Kind};
use aether_kinds::NamedMail;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use aether_fs::NamespaceRoots;
use std::env;
use std::fs;
use std::io;
use std::process;

/// Process-wide test sandbox. Single `OnceLock` so repeat calls
/// across a binary's tests resolve to the same dir — handy for
/// `write_fixture` consumers that look up the sandbox by label.
///
/// Per issue 464, the sandbox is just a directory; `SubstrateHarness`
/// receives it via `SubstrateHarness::builder().namespace_roots(...)`, not
/// via env-var mutation. The `OnceLock` no longer linearises a
/// `set_var` call — it just memoises the path.
static TEST_SAVE_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Locate `<crate_name>.wasm` under the workspace target dir, in the
/// profile directory the most recent `cargo xtask build-wasm` /
/// `cargo xtask dist` run built. Also probes
/// `<profile>/examples/<crate_name>.wasm` so a caller can name an
/// `[[example]] crate-type = ["cdylib"]` artifact, should one exist.
/// Returns `None` if no candidate path exists.
///
/// `crate_name` is the underscore-cased crate name of a top-level
/// cdylib component (e.g. `"aether_kit"`,
/// `"aether_test_fixtures_bundle"`), or an example name for an
/// `[[example]]` cdylib.
///
/// The profile is the `profile` field of `dist/manifest.json` under the
/// checkout root, which every `build-wasm` / `dist` run rewrites after
/// its build; with no manifest it is `debug`, cargo's and `build-wasm`'s
/// default. Only that one profile directory is probed: `cargo xtask
/// package` cross-builds release wasm into the same target dir without
/// touching `dist/`, and a probe that fell through to the other profile
/// would load that leftover artifact in place of the one just built.
///
/// The checkout root is resolved at run time: the nearest ancestor of
/// the current directory that holds a `Cargo.lock`, and the target dir
/// is the `CARGO_TARGET_DIR` override when set, else `target/` under
/// that root. Run time rather than `env!("CARGO_MANIFEST_DIR")`: the
/// lane hosts share one build directory across many checkouts, so a
/// compiled test binary can outlive the tree it was compiled in — and a
/// compile-time path then names a checkout whose `target/` is gone,
/// failing every wasm-loading scenario in the member at once. Cargo and
/// nextest run every test with the current directory inside its own
/// package, so the walk up from there lands on the checkout the test
/// runs in, whose `target/` (directory or symlink) holds the pre-built
/// wasm. The compile-time path remains only as the last resort for a
/// caller whose current directory is outside any checkout.
///
/// # Panics
/// Panics on the compile-time last resort if `CARGO_MANIFEST_DIR` does
/// not have two ancestor directories — fail-fast per ADR-0063: the
/// helper crate lives at `crates/<crate>`, so the workspace root is
/// always two levels up. Also panics when `dist/manifest.json` exists
/// but cannot be read, is not JSON with a `profile` string, or names a
/// profile other than `debug` / `release`: that is a broken xtask
/// output, not a missing build.
#[must_use]
pub fn locate_component_wasm(crate_name: &str) -> Option<PathBuf> {
    probe_component_wasm(crate_name).ok()
}

/// [`locate_component_wasm`] with the miss kept: `Err` carries the
/// top-level path a build would have written, so [`require_wasm`] can
/// name what it looked for.
// Test-only: CARGO_TARGET_DIR is the standard cargo build-output override, not
// cap config — honor it so wasm built into an out-of-tree target dir is found.
#[allow(clippy::disallowed_methods)]
fn probe_component_wasm(crate_name: &str) -> Result<PathBuf, PathBuf> {
    let root = env::current_dir().ok().and_then(|current| checkout_root(&current)).unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("workspace root reachable from CARGO_MANIFEST_DIR")
            .to_path_buf()
    });
    let target_root = env::var_os("CARGO_TARGET_DIR").map_or_else(|| root.join("target"), PathBuf::from);
    locate_under(&root, &target_root, crate_name)
}

/// The nearest ancestor of `current` that is a checkout root (holds a
/// `Cargo.lock`), or `None` when no ancestor is one.
///
/// Nearest root, not nearest `target/`: a checkout that has not built
/// wasm yet must resolve to where its target *would* be, so the
/// strict-mode panic names the pre-build the caller actually needs to
/// run — never a leftover target somewhere above the checkout.
fn checkout_root(current: &Path) -> Option<PathBuf> {
    current.ancestors().find(|dir| dir.join("Cargo.lock").is_file()).map(Path::to_path_buf)
}

/// Probe `<crate_name>.wasm` in the one profile directory
/// [`built_wasm_profile`] names for `checkout_root`: the top-level
/// cdylib first, then the `examples/` cdylib. `Err` carries the
/// top-level path when neither exists.
fn locate_under(checkout_root: &Path, target_root: &Path, crate_name: &str) -> Result<PathBuf, PathBuf> {
    let base = target_root.join("wasm32-unknown-unknown").join(built_wasm_profile(&checkout_root.join("dist")));
    // Top-level cdylib crates land directly under the profile dir.
    let top_level = base.join(format!("{crate_name}.wasm"));
    if top_level.exists() {
        return Ok(top_level);
    }
    // `[[example]] crate-type = ["cdylib"]` cdylibs land under
    // `<profile>/examples/<example_name>.wasm` (ADR-0090 c1).
    let example = base.join("examples").join(format!("{crate_name}.wasm"));
    if example.exists() {
        return Ok(example);
    }
    Err(top_level)
}

/// The one field of `cargo xtask dist`'s `dist/manifest.json` the
/// locator reads.
#[derive(serde::Deserialize)]
struct DistManifestProfile {
    profile: String,
}

/// The cargo profile the most recent `cargo xtask build-wasm` /
/// `cargo xtask dist` run recorded in `<dist_dir>/manifest.json`, or
/// `debug` when there is no manifest.
///
/// # Panics
/// Panics naming the manifest when it exists but cannot be read, does
/// not parse as JSON with a `profile` string, or names a profile other
/// than `debug` / `release` — fail-fast per ADR-0063.
fn built_wasm_profile(dist_dir: &Path) -> &'static str {
    let manifest = dist_dir.join("manifest.json");
    let text = match fs::read_to_string(&manifest) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return "debug",
        Err(error) => panic!("read {}: {error}", manifest.display()),
    };
    let recorded: DistManifestProfile = serde_json::from_str(&text)
        .unwrap_or_else(|error| panic!("{} has no readable `profile`: {error}", manifest.display()));
    match recorded.profile.as_str() {
        "debug" => "debug",
        "release" => "release",
        other => panic!("{} names unknown profile {other:?}", manifest.display()),
    }
}

/// Opt back into the pre-#5724 skip: with `AETHER_ALLOW_WASM_SKIP=1` a
/// missing wasm artifact returns `None` again instead of failing the
/// scenario. Any other value — including `0` and the empty string — is
/// not the opt-in, so a stale export cannot quietly restore the silent
/// pass this knob exists to make explicit.
const ALLOW_WASM_SKIP: &str = "AETHER_ALLOW_WASM_SKIP";

/// The pre-#5724 CI strict toggle. Strict is now the default, so this
/// stays accepted rather than required: `cargo xtask transform
/// verify.test` still exports it, and while it is exported it also wins
/// over [`ALLOW_WASM_SKIP`] — a CI run cannot be talked into skipping by
/// an ambient opt-in in the environment it inherited.
const REQUIRE_RUNTIME: &str = "AETHER_REQUIRE_RUNTIME";

/// Whether a missing wasm artifact is allowed to skip rather than fail.
#[allow(clippy::disallowed_methods)] // aether-suppression-request: test-harness skip/strict knob, not cap config
fn wasm_skip_allowed() -> bool {
    env::var(REQUIRE_RUNTIME).is_err() && env::var(ALLOW_WASM_SKIP).is_ok_and(|value| value == "1")
}

/// Gate over the wasm artifact alone: locates the wasm with no GPU
/// involvement, for scenarios whose harness composition needs no render
/// cap (issue #3765). Returns the wasm path on success. Visual scenarios
/// use the capture crate's `require_runtime`, which adds the wgpu
/// adapter probe in front of this.
///
/// A missing artifact **fails** the scenario (issue #5724). The skip it
/// used to take returned `None` through an `eprintln!` the test harness
/// captures and only prints on failure, so a scenario that ran nothing
/// reported `test … ok` — and an agent who had not pre-built the wasm
/// read a whole suite of those as proof its change worked. Set
/// `AETHER_ALLOW_WASM_SKIP=1` to take the skip anyway, which is what a
/// consumer who genuinely cannot cross-build wasm wants and what nobody
/// reaches for by accident.
///
/// On a hit it prints `loading <path>` to stderr, so a failing
/// scenario's captured output names the wasm it ran; a miss names the
/// path it looked for. [`locate_component_wasm`] states which profile
/// directory that is.
///
/// # Panics
/// Panics when the named crate's wasm artifact is not pre-built and the
/// skip is not explicitly allowed — fail-fast per ADR-0063 — and when
/// `dist/manifest.json` is present but broken (see
/// [`locate_component_wasm`]).
#[must_use]
// Test-only load / skip diagnostic — emitted from `cargo test` runners so an
// allowed skip is visible alongside `test ... ok` lines. Not routed
// through `tracing` because the test harness already captures stderr
// and surfaces it on failure (issue 891).
#[allow(clippy::print_stderr)]
pub fn require_wasm(crate_name: &str) -> Option<PathBuf> {
    // Both arms name the file, so a failing scenario's captured stderr
    // says which wasm it ran and a miss says where it looked.
    match probe_component_wasm(crate_name) {
        Ok(path) => {
            eprintln!("loading {}", path.display());
            Some(path)
        }
        Err(looked_for) => {
            assert!(
                wasm_skip_allowed(),
                "SKIPPED (no wasm for {crate_name}): run `cargo xtask build-wasm` \
                 — set AETHER_ALLOW_WASM_SKIP=1 to ignore (looked for {})",
                looked_for.display(),
            );
            eprintln!(
                "skipping: {crate_name}.wasm not built at {}; run `cargo xtask build-wasm`",
                looked_for.display()
            );
            None
        }
    }
}

/// Process-wide `save://` sandbox dir. Idempotent; the dir is created
/// on the first call and the same path is returned on every
/// subsequent call. Per issue 464, this helper no longer mutates
/// process env — callers pass the returned path to
/// `SubstrateHarness::builder().namespace_roots(test_namespace_roots(path))`.
///
/// `label` is baked into the dirname so the tempdir is self-describing
/// (`/tmp/aether-<label>-tests-<pid>`); pass a stable per-crate label
/// like `"kit-mesh"` or `"substrate-harness-io"`. Each integration test
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
/// `SubstrateHarness::builder().namespace_roots(...)`.
///
/// Per issue 464, this is the no-env replacement for the old
/// `init_save_sandbox`-sets-`AETHER_SAVE_DIR` pattern.
#[must_use]
pub fn test_namespace_roots(save_dir: &Path) -> NamespaceRoots {
    NamespaceRoots { save: save_dir.to_path_buf(), assets: save_dir.to_path_buf(), config: save_dir.to_path_buf() }
}

/// Write `bytes` into the sandbox at filename `name`, returning the
/// bare filename — the substrate resolves it relative to the
/// namespace root, so callers pass this as the `path` field of
/// `aether.fs.read` / `aether.kit.mesh.load` / etc.
///
/// # Panics
/// Panics if [`init_save_sandbox`] was never called in this process,
/// or if the file write fails — fail-fast per ADR-0063: the helper
/// resolves the dir from the same `OnceLock` that
/// `init_save_sandbox` populates, and a failed fixture write means
/// the test can't proceed.
pub fn write_fixture(name: &str, bytes: &[u8]) -> String {
    let dir = TEST_SAVE_DIR.get().expect("init_save_sandbox must run before write_fixture");
    fs::write(dir.join(name), bytes).expect("write fixture");
    name.to_owned()
}

/// Build a [`NamedMail`] for a mail bundle — the `pre` / `after` lists a
/// `CaptureFrame` carries, or any other named-mail batch a scenario sends.
///
/// Uses the kind's wire encoding (`encode_into_bytes`), so any `K` — cast or
/// structured — packs correctly.
///
/// Every scenario crate that drives a `SubstrateHarness` needs this and had
/// been carrying its own byte-identical copy (issue 4131); the dependency edge
/// that lets them share it already existed in all ten.
///
/// # Panics
///
/// Panics when `recipient` is not a well-formed actor path: a scenario's
/// recipient is a fixture.
pub fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient: ActorPath::new(recipient).expect("envelope recipient is a well-formed actor path"),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

#[cfg(test)]
mod tests {
    use super::{checkout_root, locate_under};
    use std::{env, fs, process};

    #[test]
    fn target_resolves_under_the_checkout_the_caller_runs_in() {
        // The lane hosts share one build directory across many checkouts, so
        // a test binary can run in a tree it was not compiled in — dispatch
        // 3181 failed 163 scenarios at once on a compile-time target path
        // into a checkout whose target was gone. Resolution walks up from
        // the caller's directory instead: the nearest checkout root wins,
        // and a directory outside any checkout resolves to nothing rather
        // than to a guess.
        let scratch = env::temp_dir().join(format!("aether-target-resolve-{}", process::id()));
        let checkout = scratch.join("outer").join("checkout");
        let nested = checkout.join("crates").join("some-crate");
        fs::create_dir_all(&nested).expect("scratch tree");
        fs::write(checkout.join("Cargo.lock"), "").expect("checkout marker");

        assert_eq!(checkout_root(&nested), Some(checkout));
        assert_eq!(checkout_root(&scratch), None, "no ancestor checkout, no resolution");

        fs::remove_dir_all(&scratch).expect("scratch removed");
    }

    #[test]
    fn wasm_profile_follows_the_dist_manifest() {
        // `cargo xtask build-wasm` builds debug by default, and `cargo xtask
        // package` cross-builds release into the same target dir without
        // rewriting `dist/`. A probe across both profiles in a fixed order
        // then loads whichever leftover it checks first: release-first
        // shadows a fresh debug build, debug-first shadows a fresh
        // `build-wasm --profile release`. Only the profile the manifest
        // records is read, and no manifest means debug.
        let scratch = env::temp_dir().join(format!("aether-wasm-profile-{}", process::id()));
        let target = scratch.join("target");
        let dist = scratch.join("dist");
        let debug = target.join("wasm32-unknown-unknown").join("debug").join("probe.wasm");
        let release = target.join("wasm32-unknown-unknown").join("release").join("probe.wasm");
        for artifact in [&debug, &release] {
            fs::create_dir_all(artifact.parent().expect("profile dir")).expect("scratch profile dir");
            fs::write(artifact, b"").expect("scratch artifact");
        }
        fs::create_dir_all(&dist).expect("scratch dist");
        let record = |profile: &str| {
            fs::write(dist.join("manifest.json"), format!(r#"{{"profile":"{profile}"}}"#)).expect("scratch manifest");
        };

        record("debug");
        assert_eq!(locate_under(&scratch, &target, "probe"), Ok(debug.clone()));
        record("release");
        assert_eq!(locate_under(&scratch, &target, "probe"), Ok(release));
        fs::remove_file(dist.join("manifest.json")).expect("manifest removed");
        assert_eq!(locate_under(&scratch, &target, "probe"), Ok(debug.clone()), "no manifest reads debug");

        record("debug");
        fs::remove_file(&debug).expect("debug artifact removed");
        assert_eq!(
            locate_under(&scratch, &target, "probe"),
            Err(debug),
            "a miss names the recorded profile's path and never falls through to release",
        );

        fs::remove_dir_all(&scratch).expect("scratch removed");
    }
}
