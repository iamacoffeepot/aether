//! Issue 7627: `ctx.now()` reads the engine's one actor clock on a guest ctx
//! and on a native ctx alike, driven through a harness whose clock the test
//! steps by hand.
//!
//! Each probe keeps one `Instant`: the reading it took at `init`, which a
//! [`ClockMark`] replaces. A [`ClockElapsed`] is answered with how long ago
//! that reading was taken. The guest probe is the bundle's `ClockProbe`;
//! [`NativeClockProbe`] is the same actor written against the native ctxs.
//!
//! No scenario reads real time: the clock moves only when the test steps it,
//! so every expected duration is exact.
//!
//! The guest scenario is skipped when the fixture wasm hasn't been built
//! (`require_wasm`), and only under `AETHER_ALLOW_WASM_SKIP=1`.

use std::fs;
use std::time::Duration;

use aether_actor::{ActorRef, HandlesKind, Instant, actor};
use aether_harness_substrate::test_helpers::require_wasm;
use aether_harness_substrate::{HarnessOp, SteppedClock, SubstrateHarness};
use aether_kinds::LoadComponent;
use aether_substrate::BootError;
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_test_fixtures_bundle::ClockProbe;
use aether_test_fixtures_kinds::{ClockElapsed, ClockElapsedReport, ClockMark};

/// How far each scenario steps the clock before its probe is built, so the
/// probe's `init` reading is not the clock's zero.
const BEFORE_INIT: Duration = Duration::from_millis(2);

/// The native probe: the bundle's `ClockProbe`, handler for handler.
struct NativeClockProbe {
    /// The reading taken at `init` and again at each [`ClockMark`].
    marked: Instant,
}

#[actor(singleton, root)]
impl NativeActor for NativeClockProbe {
    const NAMESPACE: &'static str = "test.harness_clock.probe";
    type Config = ();

    fn init((): (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { marked: ctx.now() })
    }

    #[handler::tell]
    fn on_mark(&mut self, ctx: &mut NativeCtx<'_>, _mark: ClockMark) {
        self.marked = ctx.now();
    }

    #[handler::request]
    fn on_elapsed(&mut self, ctx: &mut NativeCtx<'_>, _ask: ClockElapsed) -> ClockElapsedReport {
        ClockElapsedReport::of(ctx.now().since(self.marked))
    }
}

/// How long ago `probe` last marked, in nanoseconds, by its own reading.
fn elapsed_nanos<R>(harness: &mut SubstrateHarness, probe: ActorRef<R>) -> u64
where
    R: HandlesKind<ClockElapsed> + 'static,
{
    let report: ClockElapsedReport = harness
        .execute(vec![("elapsed", HarnessOp::send_and_await_reply(&probe, &ClockElapsed))])
        .expect("the probe answers")
        .reply("elapsed")
        .expect("decode the probe's report");

    report.elapsed_nanos
}

/// A probe built while `clock` read [`BEFORE_INIT`] reports exactly what the
/// test steps: since its `init`, then since a mark, then across a second
/// step with no mark between.
fn a_probe_measures_exactly_what_the_test_steps<R>(
    harness: &mut SubstrateHarness,
    probe: ActorRef<R>,
    clock: &SteppedClock,
) where
    R: HandlesKind<ClockMark> + HandlesKind<ClockElapsed> + 'static,
{
    clock.step(Duration::from_millis(3));
    assert_eq!(elapsed_nanos(harness, probe), 3_000_000, "since the reading `init` took");

    harness.execute(vec![("mark", HarnessOp::send_and_settle(&probe, &ClockMark))]).expect("the mark settles");
    clock.step(Duration::from_millis(7));
    assert_eq!(elapsed_nanos(harness, probe), 7_000_000, "since the mark");

    clock.step(Duration::from_millis(4));
    assert_eq!(elapsed_nanos(harness, probe), 11_000_000, "across two steps, with no mark between");
}

// Catches a `now_nanos_p32` host function or a `WasmCtx::now` wired to a
// clock other than the one the harness steps (the report would be real
// elapsed time, never an exact step), a `since` with its operands swapped
// (it saturates, so every report would be zero), and a `WasmInitCtx::now`
// that is not the engine's reading (the first report would not be 3 ms: a
// zero `init` reading reports 5 ms).
#[test]
fn a_guest_measures_exactly_what_the_test_steps() {
    let Some(path) = require_wasm("aether_test_fixtures_bundle") else {
        return;
    };
    let wasm = fs::read(path).expect("read fixture wasm");
    let clock = SteppedClock::new();
    clock.step(BEFORE_INIT);
    let mut harness = SubstrateHarness::builder()
        .size(64, 48)
        .with_component_host()
        .clock(clock.clone())
        .build()
        .expect("the component host boots");
    let load = LoadComponent { wasm, name: None, config: Vec::new(), export: None };
    let probe = harness.load::<ClockProbe>(load).unwrap_or_else(|error| panic!("the probe loads: {error}"));

    a_probe_measures_exactly_what_the_test_steps(&mut harness, probe, &clock);
}

// Catches a `NativeCtx::now` wired to a clock other than the one the harness
// steps, and a `NativeInitCtx::now` that is not the engine's reading, the
// same way the guest scenario does for the guest ctxs. With the guest
// scenario it shows the two sides read one clock.
#[test]
fn a_native_actor_measures_exactly_what_the_test_steps() {
    let clock = SteppedClock::new();
    clock.step(BEFORE_INIT);
    let mut harness = SubstrateHarness::builder()
        .with_actor::<NativeClockProbe>(())
        .clock(clock.clone())
        .build()
        .expect("the native probe boots");
    let probe = harness.actor_ref::<NativeClockProbe>();

    a_probe_measures_exactly_what_the_test_steps(&mut harness, probe, &clock);
}
