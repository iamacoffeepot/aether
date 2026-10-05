//! SPIKE (branch `spike/load-drop-heap`, issue 7414): what a load and drop
//! of a component keeps.
//!
//! One process is one cell at one count. It boots a `SubstrateHarness` with
//! the component host, runs `--warm` cycles, then `count` cycles of a real
//! `LoadComponent` mail (awaited to settlement by `load_any`) and a real
//! `DropComponent` mail, and prints one JSON object: a series of samples
//! (resident set, `/proc/self/smaps` by mapping class, glibc `mallinfo2`,
//! and, under `--dhat`, dhat's live bytes and blocks), taken after the
//! warm-up and at eight evenly spaced cycle counts. Under `--dhat <path>` the
//! dhat heap profile is written to `<path>` with the engine still up, so a
//! site's bytes at exit are what the running engine holds.
//!
//! ```text
//! aether-perf-load-drop <same|distinct> <count> --wasm <module.wasm>
//!     [--export NS] [--addressed] [--assets N] [--payload BYTES] [--live N]
//!     [--counter-keys] [--watch] [--warm N] [--samples N] [--dhat out.json]
//!     [--frames N]
//! ```
//!
//! A bundle is the module's bytes with `--assets` asset sections appended,
//! `--payload` pseudo-random bytes in all, generated from a seed.
//!
//! - `same`: one bundle (seed 0), loaded and dropped `count` times.
//! - `distinct`: `count` bundles (seed = cycle), each loaded and dropped once.
//! - `--addressed`: append the `aether.content_addressed` marker section.
//! - `--live N`: keep N instances live; each load past that drops the
//!   oldest. The default 0 drops each instance before the next load.
//! - `--counter-keys`: load with no name, so the host picks a counter key.
//!   The default names each load `churn~<cycle>`.
//! - `--rings N`: build every actor's log ring and trace ring with a
//!   capacity of N entries, in place of the defaults (1,024 and 4,096).
//! - `--watch`: every cycle waits for the dropped instance's
//!   `MonitorNotice` through a native watcher. Without it only the last
//!   cycle does (the barrier before the exit readings).

#![allow(clippy::print_stdout, clippy::print_stderr, clippy::unwrap_used, clippy::too_many_lines)]

mod sample;

use std::collections::VecDeque;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::Duration;

use aether_actor::{ActorRef, HeldReply, actor};
use aether_component::ComponentHostCapability;
use aether_data::{CONTENT_ADDRESSED_SECTION, ErasedActorPath};
use aether_harness_substrate::{HarnessOp, SubstrateHarness};
use aether_kinds::{DropComponent, DropResult, LoadComponent, MonitorNotice};
use aether_substrate::actor::native::{Held, NativeActor, NativeCtx, NativeInitCtx, Pending};
use aether_substrate::{BootError, MonitorHandle};

#[global_allocator]
static ALLOC: dhat::Alloc = dhat::Alloc;

/// The default export: the spike bundle's one instanced type, whose `wire`
/// fetches every catalogued asset through its load window.
const EXPORT: &str = "spike.bundle";

#[aether_data::kind(name = "spike.load_drop.watch", no_serde)]
struct Watch {
    target: ErasedActorPath,
}

#[aether_data::kind(name = "spike.load_drop.watching", no_serde)]
struct Watching {
    target: ErasedActorPath,
}

#[aether_data::kind(name = "spike.load_drop.await_departure", copy, no_serde)]
struct AwaitDeparture;

#[aether_data::kind(name = "spike.load_drop.departed", copy, partial_eq, no_serde)]
struct Departed {
    notified: bool,
}

impl HeldReply for Departed {
    fn unanswered() -> Self {
        Self { notified: false }
    }
}

/// Watches one component and holds an `AwaitDeparture` until its notice
/// arrives. The shape of `harness_drop_close.rs`'s watcher.
struct DepartureWatcher {
    watch: Option<MonitorHandle>,
    departed: bool,
    waiting: Option<Held<Departed>>,
}

#[actor(singleton, root)]
impl NativeActor for DepartureWatcher {
    const NAMESPACE: &'static str = "spike.load_drop.watcher";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { watch: None, departed: false, waiting: None })
    }

    #[handler::request]
    fn on_watch(&mut self, ctx: &mut NativeCtx<'_>, watch: Watch) -> Watching {
        let proven = ctx.resolve_path(&watch.target).expect("the watched component is live");
        self.watch = Some(ctx.monitor(proven).expect("the watched component is monitorable"));
        self.departed = false;
        Watching { target: watch.target }
    }

    #[handler::request]
    fn on_await_departure(&mut self, ctx: &mut NativeCtx<'_>, _await: AwaitDeparture) -> Pending<Departed> {
        let (pending, held) = ctx.hold::<Departed>();
        if self.departed {
            held.answer(ctx, &Departed { notified: true });
        } else {
            self.waiting = Some(held);
        }
        pending
    }

    #[handler::event]
    fn on_monitor_notice(&mut self, ctx: &mut NativeCtx<'_>, _notice: MonitorNotice) {
        drop(self.watch.take());
        self.departed = true;
        if let Some(held) = self.waiting.take() {
            held.answer(ctx, &Departed { notified: true });
        }
    }
}

struct Args {
    distinct: bool,
    count: usize,
    addressed: bool,
    watch: bool,
    warm: usize,
    assets: usize,
    payload: usize,
    live: usize,
    counter_keys: bool,
    dhat: Option<PathBuf>,
    frames: usize,
    samples: usize,
    rings: Option<usize>,
    wasm: PathBuf,
    export: String,
}

fn parse_args() -> Args {
    let mut argv = env::args().skip(1);
    let cell = argv.next().expect("cell: same | distinct");
    let distinct = match cell.as_str() {
        "same" => false,
        "distinct" => true,
        other => panic!("unknown cell {other}"),
    };
    let count = argv.next().expect("count").parse().expect("count is a number");
    let mut args = Args {
        distinct,
        count,
        addressed: false,
        watch: false,
        warm: 32,
        assets: 0,
        payload: 0,
        live: 0,
        counter_keys: false,
        dhat: None,
        frames: 24,
        samples: 8,
        rings: None,
        wasm: PathBuf::new(),
        export: EXPORT.to_owned(),
    };
    let mut number = |argv: &mut dyn Iterator<Item = String>| -> usize {
        argv.next().expect("a number follows the flag").parse().expect("number")
    };
    while let Some(flag) = argv.next() {
        match flag.as_str() {
            "--addressed" => args.addressed = true,
            "--watch" => args.watch = true,
            "--counter-keys" => args.counter_keys = true,
            "--warm" => args.warm = number(&mut argv),
            "--assets" => args.assets = number(&mut argv),
            "--payload" => args.payload = number(&mut argv),
            "--live" => args.live = number(&mut argv),
            "--frames" => args.frames = number(&mut argv),
            "--samples" => args.samples = number(&mut argv),
            "--rings" => args.rings = Some(number(&mut argv)),
            "--dhat" => args.dhat = Some(PathBuf::from(argv.next().expect("--dhat PATH"))),
            "--export" => args.export = argv.next().expect("--export NAMESPACE"),
            "--wasm" => args.wasm = PathBuf::from(argv.next().expect("--wasm PATH")),
            other => panic!("unknown flag {other}"),
        }
    }
    assert!(!args.wasm.as_os_str().is_empty(), "name the module with --wasm");
    args
}

fn leb128(mut value: usize, out: &mut Vec<u8>) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

/// Append one custom section (id 0, the body length, the name length, the
/// name, the payload).
fn append_section(wasm: &mut Vec<u8>, name: &str, payload: &[u8]) {
    let mut head = Vec::new();
    leb128(name.len(), &mut head);
    head.extend(name.as_bytes());

    wasm.push(0);
    leb128(head.len() + payload.len(), wasm);
    wasm.extend(head);
    wasm.extend(payload);
}

/// One step of the `SplitMix64` generator.
const fn next_word(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mixed = (*state ^ (*state >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    let mixed = (mixed ^ (mixed >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);

    mixed ^ (mixed >> 31)
}

/// The bytes one cycle loads: the module, `--assets` asset sections of
/// pseudo-random bytes from `seed`, and the content-addressed marker when
/// asked. With no assets a distinct bundle still differs: it carries one
/// eight-byte asset holding the seed.
fn bundle(base: &[u8], args: &Args, seed: u64) -> Vec<u8> {
    let mut wasm = base.to_vec();
    let mut state = seed;
    let assets = args.assets.min(args.payload);
    for index in 0..assets {
        let length = args.payload / assets + usize::from(index < args.payload % assets);
        let mut bytes = vec![0u8; length];
        for chunk in bytes.chunks_mut(8) {
            let word = next_word(&mut state).to_le_bytes();
            chunk.copy_from_slice(&word[..chunk.len()]);
        }
        append_section(&mut wasm, &format!("aether.asset.asset/{index:05}"), &bytes);
    }
    if assets == 0 && args.distinct {
        append_section(&mut wasm, "aether.asset.seed", &seed.to_le_bytes());
    }
    if args.addressed {
        append_section(&mut wasm, CONTENT_ADDRESSED_SECTION, &[1]);
    }
    wasm
}

struct Driver {
    harness: SubstrateHarness,
    host: ActorRef<ComponentHostCapability>,
    watcher: ActorRef<DepartureWatcher>,
    export: String,
    live: VecDeque<ErasedActorPath>,
    keep: usize,
    loads: usize,
    counter_keys: bool,
}

impl Driver {
    /// Load `wasm`, then drop the oldest live instance past the kept count.
    fn cycle(&mut self, wasm: Vec<u8>, watch: bool) {
        self.loads += 1;
        let name = (!self.counter_keys).then(|| format!("churn~{}", self.loads));
        let load = LoadComponent { wasm, name, config: Vec::new(), export: Some(self.export.clone()) };
        let (_, path) = self.harness.load_any(&load).unwrap_or_else(|error| panic!("the load succeeds: {error}"));
        self.live.push_back(path);

        if self.live.len() > self.keep {
            let oldest = self.live.pop_front().expect("a live instance");
            self.unload(oldest, watch);
        }
    }

    fn unload(&mut self, path: ErasedActorPath, watch: bool) {
        let drop = DropComponent { target: path.clone() };
        let steps = if watch {
            vec![
                ("watch", HarnessOp::send_and_await_reply(&self.watcher, &Watch { target: path })),
                ("drop", HarnessOp::send_and_await_reply(&self.host, &drop)),
                ("departed", HarnessOp::send_and_await_reply(&self.watcher, &AwaitDeparture)),
            ]
        } else {
            vec![("drop", HarnessOp::send_and_await_reply(&self.host, &drop))]
        };
        let done = self.harness.execute(steps).expect("the drop is answered");

        if let DropResult::Err { error } = done.reply::<DropResult>("drop").expect("decode DropResult") {
            panic!("the drop succeeds: {error}");
        }
        if watch {
            assert!(done.reply::<Departed>("departed").expect("decode Departed").notified);
        }
    }
}

fn main() {
    let args = parse_args();
    let profiler = args
        .dhat
        .as_ref()
        .map(|path| dhat::Profiler::builder().file_name(path).trim_backtraces(Some(args.frames)).build());

    let base = fs::read(&args.wasm).expect("read the module");
    let harness = SubstrateHarness::builder()
        .size(64, 48)
        .settlement_cap(Some(Duration::MAX))
        .log_ring_capacity(args.rings)
        .trace_ring_capacity(args.rings)
        .with_component_host()
        .with_actor::<DepartureWatcher>(())
        .build()
        .expect("boot");
    let host = harness.actor_ref::<ComponentHostCapability>();
    let watcher = harness.actor_ref::<DepartureWatcher>();
    let mut driver = Driver {
        harness,
        host,
        watcher,
        export: args.export.clone(),
        live: VecDeque::new(),
        keep: args.live,
        loads: 0,
        counter_keys: args.counter_keys,
    };

    // Warm-up seeds sit past every measured seed, so a distinct cell's
    // warm-up bundles are not the measured ones. It also fills the live
    // window, so every measured cycle is one load and one drop.
    let same = bundle(&base, &args, 0);
    let seeded = |seed: u64| {
        if args.distinct {
            bundle(&base, &args, seed)
        } else {
            same.clone()
        }
    };
    for warm in 0..(args.warm + args.live) {
        driver.cycle(seeded(u64::MAX / 2 + warm as u64), true);
    }

    let profiled = profiler.is_some();
    let mut series = vec![sample::take(0, profiled)];
    let stride = (args.count / args.samples).max(1);
    for index in 0..args.count {
        let last = index + 1 == args.count;
        driver.cycle(seeded(index as u64 + 1), args.watch || last);

        let done = index + 1;
        if done % stride == 0 || last {
            series.push(sample::take(done, profiled));
        }
    }
    let live = driver.harness.list_components().expect("list components");
    let trimmed = sample::after_trim(args.count, profiled);

    // The profile is written here, with the engine still up.
    drop(profiler);

    let cell = if args.distinct {
        "distinct"
    } else {
        "same"
    };
    println!(
        "{{\"cell\":\"{}\",\"export\":\"{}\",\"addressed\":{},\"watch\":{},\"count\":{},\"warm\":{},\"assets\":{},\"payload\":{},\"live\":{},\"counter_keys\":{},\"bundle_bytes\":{},\"live_components\":{},\"series\":[{}],\"after_trim\":{}}}",
        cell,
        args.export,
        args.addressed,
        args.watch,
        args.count,
        args.warm,
        args.assets,
        args.payload,
        args.live,
        args.counter_keys,
        same.len(),
        live.len(),
        series.iter().map(sample::Sample::json).collect::<Vec<_>>().join(","),
        trimmed.json(),
    );
}
