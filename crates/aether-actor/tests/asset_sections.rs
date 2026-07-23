//! ADR-0163 §2 emission validation: `export_asset!` must land the asset
//! bytes in a wasm custom section named `aether.asset.<path>`, present
//! exactly once and byte-exact against the source file.
//!
//! This is the tripwire the ADR names as the first implementation
//! risk: `#[link_section]`-to-custom-section emission on the wasm
//! target. The bugs it catches are real regressions in the macro's
//! owned logic — the section name diverging from the `aether.asset.<path>`
//! scheme the ADR-0163 indexer (#3969) keys on, the embedded bytes
//! drifting from the source file (the byte-exact compare is a computed
//! tripwire, not a mirror of a declaration), or the payload resurfacing
//! in a linear-memory data segment.
//!
//! The data-segment-absence assertion is the durable half of that.
//! ADR-0163 §2 requires the asset live in the custom section and never
//! be instantiated into linear memory. On a wasm target rustc copies a
//! `#[link_section]` static's bytes into the named custom section while
//! leaving the static an ordinary linear-memory global; that dead global
//! reaches the shipped wasm only when pinned against wasm-ld's default
//! `--gc-sections`, and `export_asset!` withholds the `#[used]` pin so it
//! is collected. Walking every `DataSection` entry and asserting the
//! payload appears in none of them reds the moment a toolchain shift, a
//! `-C link-dead-code` build, or a `--no-gc-sections` link run brings the
//! copy back.
//!
//! The `export_asset!` fixture rides `aether-test-fixtures-bundle`, not
//! an aether-actor example: `cargo xtask dist` cross-builds that crate's
//! wasm (it deps `aether-actor` and exposes a cdylib, so component
//! discovery finds it), whereas aether-actor's own examples are never
//! cross-built — the crate cannot depend on itself, so they fail the
//! actor-dep gate and no CI path emits them. Reads the prebuilt wasm the
//! same way the scenario suites do; skips when it isn't built, and turns
//! the skip into a failure under `AETHER_REQUIRE_RUNTIME=1` (CI's
//! `cargo xtask dist` pre-build emits it and sets the flag, so a
//! pre-build miss is loud rather than a vacuous pass).

use std::path::PathBuf;
use std::{env, fs};

use wasmparser::{Parser, Payload};

/// Locate the built fixture-bundle wasm off aether-actor's manifest dir
/// (`crates/aether-actor` → workspace root two levels up). A top-level
/// cdylib lands directly under the profile dir (no `examples/` segment).
/// Probes `debug` then `release` so either build profile works — CI's
/// `cargo xtask dist` builds the debug profile by default, local runs may
/// use either. Mirrors `aether-harness-substrate`'s `locate_component_wasm`
/// without taking the dev-dep (which would drag the capture/render harness
/// into aether-actor's test build).
fn locate_fixture_wasm() -> Option<PathBuf> {
    let workspace = PathBuf::from(env!("CARGO_MANIFEST_DIR")).parent()?.parent()?.to_path_buf();
    for profile in ["debug", "release"] {
        let candidate = workspace
            .join("target")
            .join("wasm32-unknown-unknown")
            .join(profile)
            .join("aether_test_fixtures_bundle.wasm");
        if candidate.exists() {
            return Some(candidate);
        }
    }
    None
}

/// The source asset, read at compile time so the assertion compares
/// against the exact bytes the macro embedded. Lives in the fixture
/// crate next to the `export_asset!` that embeds it.
const ASSET: &[u8] =
    include_bytes!(concat!(env!("CARGO_MANIFEST_DIR"), "/../aether-test-fixtures-bundle/src/asset_fixture.txt"));

/// The custom section `export_asset!("asset_fixture.txt")` emits.
const SECTION_NAME: &str = "aether.asset.asset_fixture.txt";

// Test-only skip diagnostic — emitted from a `cargo test` runner so a
// skipped test is visible alongside `test ... ok` lines, exactly as
// aether-harness-substrate's `require_wasm` does. Not a runtime print path.
#[allow(clippy::print_stderr)]
#[test]
fn asset_rides_a_named_custom_section_byte_exact() {
    // Test-binary runtime probe, not cap config — same contract as the
    // scenario harness's require_wasm.
    #[allow(clippy::disallowed_methods)]
    let require = env::var_os("AETHER_REQUIRE_RUNTIME").is_some();
    let Some(wasm_path) = locate_fixture_wasm() else {
        assert!(!require, "AETHER_REQUIRE_RUNTIME=1 but aether_test_fixtures_bundle wasm not pre-built");
        eprintln!(
            "skipping: aether_test_fixtures_bundle wasm not built under target/wasm32-unknown-unknown/{{debug,release}}"
        );
        return;
    };
    let bytes = fs::read(&wasm_path).expect("read fixture-bundle wasm");

    let mut section: Option<Vec<u8>> = None;
    for payload in Parser::new(0).parse_all(&bytes) {
        if let Payload::CustomSection(reader) = payload.expect("parse aether_test_fixtures_bundle.wasm")
            && reader.name() == SECTION_NAME
        {
            assert!(
                section.replace(reader.data().to_vec()).is_none(),
                "{SECTION_NAME} appears in more than one custom section — export_asset! must \
                 emit exactly one, or the indexer's per-path lookup is ambiguous"
            );
        }
    }

    let embedded = section.unwrap_or_else(|| panic!("fixture-bundle wasm carries the {SECTION_NAME} custom section"));
    assert_eq!(embedded, ASSET, "custom-section bytes must match the source asset exactly");
}

/// Tripwire: the asset bytes live in the custom section and in no
/// linear-memory data segment. `export_asset!` withholds the `#[used]`
/// pin, so wasm-ld's default `--gc-sections` collects the dead
/// linear-memory copy rustc leaves behind — ADR-0163 §2's "never
/// instantiated into linear memory". This reds if a toolchain shift, a
/// `-C link-dead-code` build, or a `--no-gc-sections` link run brings
/// the copy back. Payload-in-subslice, not byte-equal: a data segment is
/// a concatenation of many statics, so containment is the invariant.
#[allow(clippy::print_stderr)]
#[test]
fn asset_bytes_are_absent_from_every_data_segment() {
    // Test-binary runtime probe, not cap config — same contract as the
    // sibling test above.
    #[allow(clippy::disallowed_methods)]
    let require = env::var_os("AETHER_REQUIRE_RUNTIME").is_some();
    let Some(wasm_path) = locate_fixture_wasm() else {
        assert!(!require, "AETHER_REQUIRE_RUNTIME=1 but aether_test_fixtures_bundle wasm not pre-built");
        eprintln!(
            "skipping: aether_test_fixtures_bundle wasm not built under target/wasm32-unknown-unknown/{{debug,release}}"
        );
        return;
    };
    let bytes = fs::read(&wasm_path).expect("read fixture-bundle wasm");

    let mut data_segments = 0usize;
    for payload in Parser::new(0).parse_all(&bytes) {
        if let Payload::DataSection(reader) = payload.expect("parse aether_test_fixtures_bundle.wasm") {
            for segment in reader {
                let segment = segment.expect("parse data segment");
                data_segments += 1;
                assert!(
                    !contains_subslice(segment.data, ASSET),
                    "asset payload found in a linear-memory data segment — the un-pinned .rodata copy \
                     survived garbage collection (ADR-0163 §2: never instantiated into linear memory)"
                );
            }
        }
    }

    // Guard against a vacuous pass: a fixture wasm with zero data segments
    // could not fail the containment check for the wrong reason. The bundle
    // links real code, so it always carries at least one.
    assert!(data_segments > 0, "fixture-bundle wasm has no data segments — check the assertion is meaningful");
}

/// Whether `haystack` contains `needle` as a contiguous subslice.
fn contains_subslice(haystack: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() {
        return true;
    }
    haystack.windows(needle.len()).any(|window| window == needle)
}
