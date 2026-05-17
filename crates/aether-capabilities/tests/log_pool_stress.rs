//! Issue 635 Phase 2 stress test: drive 10k `LogBatch` mails through
//! the `Scheduling::Pooled` dispatcher path. Exercises end-to-end the
//! macro-emitted `__dispatch` arm, the
//! [`aether_substrate::runtime::log_install::with_actor_dispatch`] +
//! `local::with_stamped` wrapping inside
//! [`aether_substrate::actor::native::dispatcher_slot::DispatcherSlot`],
//! and the `SlotState` requeue/idle transitions across batched cycles.
//!
//! Compared against a parallel `Scheduling::Dedicated` fixture (same
//! struct shape + handler body, only `SCHEDULING` differs) so the perf
//! check normalises against the host's clock instead of a hard wallclock
//! bound. Issue 635 Phase 2 spec: throughput within 2× of the dedicated
//! baseline.
//!
//! Issue 776 retired `EgressBackend::egress_log_batch` (`LogCapability`
//! owns its entries in a substrate-side ring now). The handler workload
//! both fixtures share is a single `AtomicU64::fetch_add` — same shape
//! the cap's `push_entry` runs minus the timestamp/origin stamp, which
//! isn't what this test measures anyway. The counter doubles as the
//! "every mail arrived" assertion that previously rode on draining the
//! recording channel.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use aether_data::{Kind, ReplyTo};
use aether_kinds::LogBatch;

use aether_substrate::{
    Actor, BootError, Builder, BuiltChassis, Chassis, Mailer, NativeActor, NativeCtx,
    NativeInitCtx, NeverDriver, PassiveChassis, Registry, handle_store::HandleStore,
    mail::registry::MailboxEntry,
};

const STRESS_BATCHES: u32 = 10_000;
/// Pooled-vs-dedicated wallclock ratio cap. Issue 635 Phase 2 spec is
/// 2×; we ship 3× as the assertion to leave CI-runner slack — the
/// printed ratio is the load-bearing metric for human review.
const RATIO_CAP: f64 = 3.0;
/// Hard wallclock cap on the counter wait per run. Either flavour
/// completing within this budget is comfortably above any pool /
/// dedicated baseline observed locally; if a regression pushes drain
/// wallclock past this, the stress test is the right place to see it.
const DRAIN_BUDGET: Duration = Duration::from_secs(30);

struct TestChassis;
impl Chassis for TestChassis {
    const PROFILE: &'static str = "test";
    type Driver = NeverDriver;
    type Env = ();
    fn build(_env: Self::Env) -> Result<BuiltChassis<Self>, BootError> {
        unreachable!("driven by Builder directly in this test")
    }
}

/// Shared handler workload for both scheduling fixtures. Holds a
/// counter the test thread polls after the push loop to confirm every
/// dispatched mail ran end-to-end.
#[derive(Clone)]
struct CounterConfig {
    counter: Arc<AtomicU64>,
}

/// `Scheduling::Pooled` fixture. Counter-incrementing handler so the
/// per-mail work is a constant — the only variable under test is
/// scheduling-class throughput.
struct PooledBenchLogCap {
    counter: Arc<AtomicU64>,
}

impl aether_actor::Singleton for PooledBenchLogCap {}

#[aether_data::actor]
impl NativeActor for PooledBenchLogCap {
    type Config = CounterConfig;
    const NAMESPACE: &'static str = "test.bench.log_pooled";
    // SCHEDULING defaults to Pooled; left explicit so the contrast
    // with DedicatedBenchLogCap below is visible at a glance.
    const SCHEDULING: Scheduling = Scheduling::Pooled;

    fn init(config: CounterConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            counter: config.counter,
        })
    }

    #[aether_data::handler]
    fn on_log_batch(&mut self, _ctx: &mut NativeCtx<'_>, _batch: LogBatch) {
        self.counter.fetch_add(1, Ordering::Relaxed);
    }
}

/// `Scheduling::Dedicated` baseline. Mirrors `PooledBenchLogCap`'s
/// handler body so per-mail work is comparable; only `SCHEDULING`
/// differs, which is the variable under test.
struct DedicatedBenchLogCap {
    counter: Arc<AtomicU64>,
}

impl aether_actor::Singleton for DedicatedBenchLogCap {}

#[aether_data::actor]
impl NativeActor for DedicatedBenchLogCap {
    type Config = CounterConfig;
    const NAMESPACE: &'static str = "test.bench.log_dedicated";
    const SCHEDULING: Scheduling = Scheduling::Dedicated;

    fn init(config: CounterConfig, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            counter: config.counter,
        })
    }

    #[aether_data::handler]
    fn on_log_batch(&mut self, _ctx: &mut NativeCtx<'_>, _batch: LogBatch) {
        self.counter.fetch_add(1, Ordering::Relaxed);
    }
}

fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
    let registry = Arc::new(Registry::new());
    let store = Arc::new(HandleStore::new(1024 * 1024));
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store));
    (registry, mailer)
}

fn push_log_batch(registry: &Registry, recipient: &str, payload: &[u8]) {
    let id = registry.lookup(recipient).expect("mailbox registered");
    let MailboxEntry::Inbox(handler) = registry.entry(id).expect("entry exists") else {
        panic!("expected mailbox entry under {recipient}");
    };
    handler.enqueue(aether_substrate::mail::registry::OwnedDispatch {
        kind: <LogBatch as Kind>::ID,
        kind_name: LogBatch::NAME.to_owned(),
        origin: None,
        sender: ReplyTo::NONE,
        payload: payload.to_vec(),
        count: 1,
        mail_id: aether_substrate::mail::MailId::NONE,
        root: aether_substrate::mail::MailId::NONE,
        parent_mail: None,
    });
}

/// Spin-wait until the counter reaches `target`, capped by
/// [`DRAIN_BUDGET`]. Returns the count actually observed; callers
/// assert it equals `target`.
fn await_counter(counter: &AtomicU64, target: u64) -> u64 {
    let deadline = Instant::now() + DRAIN_BUDGET;
    loop {
        let value = counter.load(Ordering::Relaxed);
        if value >= target || Instant::now() >= deadline {
            return value;
        }
        std::thread::yield_now();
    }
}

/// Build one `LogBatch` mail with a single entry and return its
/// postcard-encoded bytes. Every push reuses the same buffer so the
/// stress measurement reflects dispatch cost, not encode churn.
fn encoded_one_entry_batch() -> Vec<u8> {
    let batch = LogBatch {
        entries: vec![aether_kinds::LogEvent {
            level: 2,
            target: "stress.bench".into(),
            message: "log_pool_stress".into(),
        }],
    };
    LogBatch::encode_into_bytes(&batch)
}

fn run_pool_stress() -> Duration {
    let (registry, mailer) = fresh_substrate();
    let counter = Arc::new(AtomicU64::new(0));
    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<PooledBenchLogCap>(CounterConfig {
                counter: Arc::clone(&counter),
            })
            .build_passive()
            .expect("PooledBenchLogCap boots");

    let bytes = encoded_one_entry_batch();
    let start = Instant::now();
    for _ in 0..STRESS_BATCHES {
        push_log_batch(&registry, PooledBenchLogCap::NAMESPACE, &bytes);
    }
    let observed = await_counter(&counter, u64::from(STRESS_BATCHES));
    let elapsed = start.elapsed();

    drop(chassis);

    assert_eq!(
        observed,
        u64::from(STRESS_BATCHES),
        "pool-path counter reached {observed} of {STRESS_BATCHES} before drain budget elapsed",
    );

    elapsed
}

fn run_dedicated_baseline() -> Duration {
    let (registry, mailer) = fresh_substrate();
    let counter = Arc::new(AtomicU64::new(0));
    let chassis: PassiveChassis<TestChassis> =
        Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<DedicatedBenchLogCap>(CounterConfig {
                counter: Arc::clone(&counter),
            })
            .build_passive()
            .expect("DedicatedBenchLogCap boots");

    let bytes = encoded_one_entry_batch();
    let start = Instant::now();
    for _ in 0..STRESS_BATCHES {
        push_log_batch(&registry, DedicatedBenchLogCap::NAMESPACE, &bytes);
    }
    let observed = await_counter(&counter, u64::from(STRESS_BATCHES));
    let elapsed = start.elapsed();

    drop(chassis);

    assert_eq!(
        observed,
        u64::from(STRESS_BATCHES),
        "dedicated-baseline counter reached {observed} of {STRESS_BATCHES} before drain budget elapsed",
    );

    elapsed
}

/// Issue 635 Phase 2 stress gate. Runs the dedicated baseline first
/// (so JIT warm-up doesn't tilt the comparison), then the pooled path,
/// asserts no drops on either, and bounds the pooled wallclock at
/// `RATIO_CAP` × the dedicated baseline.
#[test]
fn pool_path_drains_10k_log_batches_within_dedicated_budget() {
    let dedicated = run_dedicated_baseline();
    let pooled = run_pool_stress();

    let ratio = pooled.as_secs_f64() / dedicated.as_secs_f64().max(f64::MIN_POSITIVE);
    eprintln!(
        "log_pool_stress: {STRESS_BATCHES} mails — dedicated={dedicated:?} pooled={pooled:?} ratio={ratio:.2}x"
    );

    assert!(
        ratio <= RATIO_CAP,
        "pooled drain wallclock {ratio:.2}x dedicated baseline (cap {RATIO_CAP:.2}x); pooled={pooled:?} dedicated={dedicated:?}",
    );
}
