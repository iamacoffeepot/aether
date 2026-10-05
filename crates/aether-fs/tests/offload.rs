//! `aether.fs` reads and loads on worker queues (issue 7377), driven over a
//! real chassis with `aether.fs` composed on the pool and a pumped requester
//! sending to it.
//!
//! A worker is held mid-read by pointing it at a FIFO: `fs::read` blocks in
//! `open` until a writer opens the other end, so a test decides exactly when
//! each read can finish. A FIFO is fed only through a non-blocking write-open,
//! retried on each pump of the requester, which fails with `ENXIO` until the
//! worker has opened its end; so a queue that never starts a read fails the
//! test at the pump cap instead of hanging it. Every wait is a settlement wait
//! or a pump, never a sleep.
#![cfg(unix)]

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use aether_actor::actor;
use aether_data::MailId;
use aether_fs::{FsCapability, Load, Loaded, MAX_LOADS_IN_FLIGHT, NamespaceAddr, NamespaceRoots, Read, ReadResult};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::testing::{PumpedDriver, boot_test_chassis_with, cleanup, fresh_substrate, scratch_dir};

/// Ask the requester to read `addr`, holding the chain until it is answered.
#[aether_data::kind(name = "test.fs_offload.start_read")]
struct StartRead {
    addr: NamespaceAddr,
}

/// Ask the requester to load `addr`, binding `slot` as the request's context.
#[aether_data::kind(name = "test.fs_offload.start_load")]
struct StartLoad {
    addr: NamespaceAddr,
    slot: Slot,
}

/// The context a load binds and takes back from its late reply.
#[aether_data::kind(name = "test.fs_offload.slot", copy, eq)]
struct Slot {
    index: u64,
}

/// A requester that reads and loads through `aether.fs` and records what
/// comes back, with the chain root of each load's sending turn and of each
/// turn that received a load's reply.
struct Requester;

struct RequesterState {
    sent_roots: Vec<Option<MailId>>,
    reads: Vec<ReadResult>,
    loaded: Vec<(Loaded, Slot, Option<MailId>)>,
}

#[actor(singleton, root, depends(FsCapability))]
impl NativeActor for Requester {
    type State = RequesterState;
    type Config = ();
    const NAMESPACE: &'static str = "test.fs_offload.requester";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<RequesterState, BootError> {
        Ok(RequesterState { sent_roots: Vec::new(), reads: Vec::new(), loaded: Vec::new() })
    }

    #[handler::tell]
    fn on_start_read(_state: &mut Self::State, ctx: &mut NativeCtx<'_>, start: StartRead) {
        ctx.send::<FsCapability>(&Read { addr: start.addr });
    }

    #[handler::tell]
    fn on_start_load(state: &mut Self::State, ctx: &mut NativeCtx<'_>, start: StartLoad) {
        state.sent_roots.push(ctx.in_flight_root());
        let _request = ctx.send_with_context::<FsCapability>(&Load { addr: start.addr }, start.slot);
    }

    #[handler::response]
    fn on_read_result(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, result: ReadResult) {
        state.reads.push(result);
    }

    #[handler::response]
    fn on_loaded(state: &mut Self::State, ctx: &mut NativeCtx<'_>, loaded: Loaded, slot: Slot) {
        state.loaded.push((loaded, slot, ctx.in_flight_root()));
    }
}

/// Scratch namespace roots, removed when dropped.
struct Sandbox {
    root: PathBuf,
    roots: NamespaceRoots,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let root = scratch_dir("aether-fs-offload", tag);
        let roots =
            NamespaceRoots { save: root.join("save"), assets: root.join("assets"), config: root.join("config") };
        for dir in [&roots.save, &roots.assets, &roots.config] {
            fs::create_dir_all(dir).expect("test setup: namespace root creates");
        }

        Self { root, roots }
    }

    /// A FIFO at `name` under the save root.
    fn fifo(&self, name: &str) -> PathBuf {
        let path = self.roots.save.join(name);
        let c_path = CString::new(path.as_os_str().as_bytes()).expect("test setup: path has no NUL");
        // SAFETY: `c_path` is a valid NUL-terminated path that outlives the call, and `mkfifo` only
        // reads it.
        let status = unsafe { libc::mkfifo(c_path.as_ptr(), 0o600) };
        assert_eq!(status, 0, "test setup: mkfifo {}: {}", path.display(), io::Error::last_os_error());
        path
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        cleanup(&self.root);
    }
}

/// Write `bytes` into `fifo` and close it, if a reader has its other end
/// open: a non-blocking write-open fails with `ENXIO` until one has. Called
/// from a pump predicate, so it is retried on each pump until it succeeds.
fn try_feed(fifo: &Path, bytes: &[u8]) -> bool {
    let Ok(mut writer) = OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(fifo) else {
        return false;
    };
    writer.write_all(bytes).expect("write the FIFO");
    true
}

/// Whether a reader has the FIFO open right now.
fn has_reader(fifo: &Path) -> bool {
    OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(fifo).is_ok()
}

/// A pumped [`Requester`] on a chassis composing `aether.fs` on the pool.
fn requester(sandbox: &Sandbox) -> PumpedDriver<Requester> {
    let (registry, mailer) = fresh_substrate();
    let chassis = boot_test_chassis_with::<FsCapability>(&registry, &mailer, sandbox.roots.clone(), ());
    PumpedDriver::boot(chassis, (), ())
}

fn save(path: &str) -> NamespaceAddr {
    NamespaceAddr::new("save", path)
}

fn read_bytes(result: &ReadResult) -> (&str, Option<&[u8]>) {
    match result {
        ReadResult::Ok { addr, bytes } => (addr.path.as_str(), bytes.contiguous()),
        ReadResult::Err { addr, error } => panic!("read of {} failed: {error:?}", addr.path),
    }
}

/// Bug caught: reads still running on the `aether.fs` turn, so a read
/// blocked on a slow file holds up the read behind it, or a read whose chain
/// settles before its reply goes out (a `Tick` frame would advance without
/// its bytes).
#[test]
fn two_reads_run_at_once_and_each_holds_its_own_chain() {
    let sandbox = Sandbox::new("reads-overlap");
    let slow = sandbox.fifo("slow.bin");
    fs::write(sandbox.roots.save.join("quick.bin"), b"quick").expect("test setup: seed quick.bin");
    let mut driver = requester(&sandbox);
    let requester_ref = driver.chassis().actor_ref::<Requester>();

    let (slow_root, slow_settled) =
        driver.chassis().send_tracked(requester_ref, &StartRead { addr: save("slow.bin") }, None);
    let quick_root = driver.send_tracked(requester_ref, &StartRead { addr: save("quick.bin") }, None);
    driver.settle(&[quick_root]);

    let reads = driver.read_state(|state| state.reads.clone()).expect("the requester is live");
    let [quick] = reads.as_slice() else {
        panic!("expected only the quick read answered, got {reads:?}");
    };
    assert_eq!(read_bytes(quick), ("quick.bin", Some(&b"quick"[..])));
    assert!(slow_settled.try_recv().is_err(), "the blocked read's chain settled before its reply");

    driver.pump_until("the slow read's worker opens its FIFO", |_| try_feed(&slow, b"slow"));
    driver.settle(&[slow_root]);

    let reads = driver.read_state(|state| state.reads.clone()).expect("the requester is live");
    assert_eq!(reads.len(), 2, "both reads answered: {reads:?}");
    assert_eq!(read_bytes(&reads[1]), ("slow.bin", Some(&b"slow"[..])));
}

/// Bug caught: a load holding the caller's chain (the caller's frame would
/// wait for the disk), a late reply that rides a chain, or one that loses
/// the context the caller bound.
#[test]
fn load_settles_its_chain_before_the_bytes_and_replies_with_the_bound_context() {
    let sandbox = Sandbox::new("load-settles");
    let fifo = sandbox.fifo("asset.bin");
    let mut driver = requester(&sandbox);
    let requester_ref = driver.chassis().actor_ref::<Requester>();

    let start = StartLoad { addr: save("asset.bin"), slot: Slot { index: 42 } };
    driver.send_and_settle(requester_ref, &start, None);

    let loaded = driver.read_state(|state| state.loaded.len()).expect("the requester is live");
    assert_eq!(loaded, 0, "the reply arrived before the FIFO was fed");

    driver.pump_until("the load's worker opens its FIFO", |_| try_feed(&fifo, b"asset bytes"));
    driver.pump_until("the load's late reply", |state| state.loaded.len() == 1);

    driver
        .read_state(|state| {
            let [(Loaded::Ok { addr, bytes }, slot, reply_root)] = state.loaded.as_slice() else {
                panic!("expected one Loaded::Ok, got {:?}", state.loaded);
            };
            assert_eq!(addr.path, "asset.bin");
            assert_eq!(bytes.contiguous(), Some(&b"asset bytes"[..]));
            assert_eq!(*slot, Slot { index: 42 }, "the late reply lost the bound context");
            assert!(state.sent_roots[0].is_some(), "the load was sent on a chain");
            assert!(reply_root.is_none(), "the late reply rode a chain: {reply_root:?}");
        })
        .expect("the requester is live");
}

/// Bug caught: a load queue that starts loads past its bound, drops the
/// loads waiting behind a full queue, or never starts the next one when a
/// slot frees.
#[test]
fn loads_past_the_bound_wait_in_order_and_none_is_dropped() {
    let sandbox = Sandbox::new("load-bound");
    let total = MAX_LOADS_IN_FLIGHT + 1;
    let fifos: Vec<PathBuf> = (0..total).map(|index| sandbox.fifo(&format!("asset-{index}.bin"))).collect();
    let mut driver = requester(&sandbox);
    let requester_ref = driver.chassis().actor_ref::<Requester>();

    let roots: Vec<MailId> = (0..total)
        .map(|index| {
            let start = StartLoad { addr: save(&format!("asset-{index}.bin")), slot: Slot { index: index as u64 } };
            driver.send_tracked(requester_ref, &start, None)
        })
        .collect();
    driver.settle(&roots);

    assert!(!has_reader(&fifos[MAX_LOADS_IN_FLIGHT]), "the load past the bound started before a slot freed");

    for fifo in &fifos {
        driver.pump_until("a load's worker opens its FIFO", |_| try_feed(fifo, b"bytes"));
    }
    driver.pump_until("every load's reply", |state| state.loaded.len() == total);

    driver
        .read_state(|state| {
            let mut indices: Vec<u64> = state
                .loaded
                .iter()
                .map(|(loaded, slot, _)| match loaded {
                    Loaded::Ok { .. } => slot.index,
                    Loaded::Err { error, .. } => panic!("load {} failed: {error:?}", slot.index),
                })
                .collect();
            indices.sort_unstable();
            let expected: Vec<u64> = (0..total as u64).collect();
            assert_eq!(indices, expected, "every load is answered exactly once");
        })
        .expect("the requester is live");
}
