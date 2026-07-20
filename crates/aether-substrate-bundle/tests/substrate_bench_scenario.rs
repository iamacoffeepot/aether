//! Phase 3 substrate-feature scenarios (issue 430). Each test boots
//! a `SubstrateBench` and exercises one substrate primitive — input
//! subscription, drop, `replace_component` (all via
//! `aether-test-fixtures`'s `probe` cdylib), or the chassis `aether.fs`
//! adapter's read/write/delete/list round trips — driving every step
//! through `SubstrateBench::execute` (issue 868). The render scenarios
//! moved to `aether-render`'s own `render_scenario` target (issue #3771).
//!
//! Skipped when:
//! - No wgpu adapter is available (driverless Linux runners without
//!   `mesa-vulkan-drivers`).
//! - The fixture's wasm hasn't been built — fixture-loading tests
//!   read `target/wasm32-unknown-unknown/{debug,release}/examples/probe.wasm`
//!   and skip with an `eprintln!` when it's absent. fs scenarios
//!   don't load the fixture, so they only need wgpu. CI builds the
//!   fixture wasm before invoking `cargo test`; setting
//!   `AETHER_REQUIRE_RUNTIME=1` (CI does) flips both skip points
//!   into hard panics so a missing pre-build is loud.
//!
//! All boot-time mechanics (wgpu probe, wasm locator, skip-or-panic
//! gate, `save://` sandbox) live in
//! `aether_substrate_bench_capture::test_helpers` (issues 460 +
//! 821). Per issue 464, the sandbox flows in via
//! `SubstrateBench::builder().full().namespace_roots(...)` rather than env-var
//! mutation.

// Integration-test skip diagnostic: emit via stderr so `cargo test`
// surfaces "skipping: ..." alongside `test ... ok` (issue 891).
#![allow(clippy::print_stderr)]
// Test reads the AETHER_REQUIRE_RUNTIME CI skip toggle — a test-harness knob,
// not cap config.
#![allow(clippy::disallowed_methods)]

use aether_substrate_bundle::FullBenchExt;
use std::path::Path;

use aether_clipboard::{GetClipboardText, GetClipboardTextResult, SetClipboardText, SetClipboardTextResult};
use aether_data::{Kind, MailboxId};
use aether_fs::{Delete, DeleteResult, FsError, List, ListResult, Read, ReadResult, Write, WriteResult};
use aether_kinds::{
    CachedFontMetrics, ClipRect, DropComponent, DropResult, ListComponents, ListComponentsResult, LoadComponent,
    LoadResult, NamedMail, Ping, QuadScale, QuadSpace, ReplaceComponent, ReplaceResult,
};
use aether_math::{Mat4, Rgba, Vec3};
use aether_render::ViewProjection;
use aether_substrate_bench::{BenchOp, SubstrateBench};
use aether_substrate_bench_capture::test_helpers::{
    has_wgpu_adapter, init_save_sandbox, require_runtime, test_namespace_roots,
};
use aether_substrate_bench_capture::visual::{
    Image, background_top_left, bounding_box, centroid, coverage, decode_png,
};
use aether_test_fixtures_kinds::{
    Bump, CountQuery, CountReport, DespawnChild, INLINE_WHO_CHILD, INLINE_WHO_PARENT, InlineEcho, InlineProbe,
    TagSpawnQuery, TagSpawnReport,
};
use aether_text::{DrawText, FontMetricsRequest, FontMetricsResult, FontRef, LoadFont, LoadFontResult};

// Pin the fixture rlib so its `inventory::submit!` `KindDescriptor`
// entries are present in this test binary. Without the reference, the
// host-target rlib's descriptor symbols can be stripped by the linker
// and `aether_kinds::descriptors::all()` won't see fixture kinds.
#[allow(unused_imports)]
use aether_test_fixtures_kinds as _;
use std::env;
use std::fs;

/// Caller-supplied component name passed to `LoadComponent`.
const PROBE_NAME: &str = "probe";
/// Full trampoline address the substrate registers under post-issue-634
/// Phase 4. Mail destined for the loaded probe goes here, not to the
/// bare `PROBE_NAME` (which isn't a registered mailbox). Built from
/// The `/`-rendered lineage a loaded component registers at (ADR-0099
/// §4): the component host `aether.component` `/`-joined to the
/// trampoline node — exactly what `LoadResult.name` reports.
fn probe_address() -> String {
    use aether_actor::Addressable;
    format!("aether.component/{}:{}", aether_component::WasmTrampoline::NAMESPACE, PROBE_NAME)
}
const TICK_OBSERVED: &str = "aether.test_fixture.tick_observed";
/// ADR-0147 boot fixture markers (`crate::aether-test-fixtures-boot`): the boot
/// actor broadcasts `BOOT_OBSERVED` from `wire` (once per instance) and
/// `BOOT_TORN_DOWN` from `unwire` (once at teardown). The boot scenario counts
/// them via `count_observed`.
const BOOT_OBSERVED: &str = "aether.test_fixture.boot_observed";
const BOOT_TORN_DOWN: &str = "aether.test_fixture.boot_torn_down";

/// Build a `NamedMail` for a `CaptureFrame` mail bundle. Uses
/// the kind's wire encoding (`encode_into_bytes`) so any K — cast
/// or structured — packs correctly.
fn envelope<K: Kind>(recipient: &str, mail: &K) -> NamedMail {
    NamedMail {
        recipient_name: recipient.to_owned(),
        kind_name: K::NAME.to_owned(),
        payload: mail.encode_into_bytes(),
        count: 1,
    }
}

fn rgba_at(img: &Image, x: u32, y: u32) -> [u8; 4] {
    let start = ((y * img.width + x) * 4) as usize;
    [img.rgba[start], img.rgba[start + 1], img.rgba[start + 2], img.rgba[start + 3]]
}

fn rgb_close(actual: [u8; 4], expected: [u8; 3], tolerance: u8) -> bool {
    actual[..3].iter().zip(expected).all(|(actual, expected)| actual.abs_diff(expected) <= tolerance)
}

fn pixel_is_lit(img: &Image, x: u32, y: u32, bg: [u8; 3], tolerance: u8) -> bool {
    !rgb_close(rgba_at(img, x, y), bg, tolerance)
}

fn lit_fraction_in_rect(img: &Image, x: u32, y: u32, width: u32, height: u32, bg: [u8; 3], tolerance: u8) -> f32 {
    let mut lit = 0u32;
    for py in y..y + height {
        for px in x..x + width {
            if pixel_is_lit(img, px, py, bg, tolerance) {
                lit += 1;
            }
        }
    }
    #[allow(clippy::cast_precision_loss)]
    {
        lit as f32 / (width * height) as f32
    }
}

/// Load the probe into the bench via `execute`, blocking on the
/// `LoadResult` reply so subsequent `advance` ops see a
/// fully-instantiated and tick-subscribed component. Returns the
/// loaded component's `MailboxId` (the trampoline address), which
/// the drop / replace scenarios target. Pre-Phase-4 of issue 603 the
/// bench's `aether.control` mailbox (renamed to `aether.component` in
/// issue 638 phase 3) served as a single FIFO point for both load and
/// advance; Phase 4 split advance onto `aether.substrate_bench`, so load is
/// no longer naturally ordered ahead of advance — `SendAndAwait`
/// blocks on `LoadResult` before returning.
fn load_probe(bench: &mut SubstrateBench, wasm_path: &Path) -> MailboxId {
    let wasm = fs::read(wasm_path).expect("read fixture wasm");
    let loaded = bench
        .execute(vec![(
            "load",
            BenchOp::send_and_await(
                "aether.component",
                &LoadComponent { wasm, name: Some(PROBE_NAME.to_owned()), config: Vec::new(), export: None },
            ),
        )])
        .expect("load sequence");
    match loaded.reply::<LoadResult>("load").expect("decode LoadResult") {
        LoadResult::Ok { mailbox_id, .. } => mailbox_id,
        LoadResult::Err { error } => panic!("load_component: {error}"),
    }
}

/// fs scenarios need wgpu (the bench unconditionally builds a
/// `Gpu` at boot) but not the fixture wasm. Skips on wgpu-less
/// runners and panics under `AETHER_REQUIRE_RUNTIME` so a
/// CI-side regression is loud.
fn require_wgpu_only() -> bool {
    if has_wgpu_adapter() {
        return true;
    }
    let strict = env::var("AETHER_REQUIRE_RUNTIME").is_ok();
    assert!(!strict, "AETHER_REQUIRE_RUNTIME set but no wgpu adapter available");
    eprintln!("skipping: no wgpu adapter available");
    false
}

#[path = "substrate_bench_scenario/boot.rs"]
mod boot;
#[path = "substrate_bench_scenario/clipboard.rs"]
mod clipboard;
#[path = "substrate_bench_scenario/component.rs"]
mod component;
#[path = "substrate_bench_scenario/fs.rs"]
mod filesystem;
#[path = "substrate_bench_scenario/inline_child.rs"]
mod inline_child;
#[path = "substrate_bench_scenario/text.rs"]
mod text;

// Pre-#775 the bench emitted `aether.observation.frame_stats` every
// 120 frames and a test verified one broadcast arrived after
// `advance(120)`. Issue 775 retired the broadcast cap, the frame_stats
// kind, and the helper that emitted it; this test went with them.
