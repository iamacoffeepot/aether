//! `aether.fs` reads and loads on worker queues (issue 7377), driven over a
//! real chassis with `aether.fs` composed on the pool.
//!
//! A worker is held mid-read by pointing it at a FIFO: `fs::read` blocks in
//! `open` until a writer opens the other end, so a test decides exactly when
//! each read can finish. Writers open their end on helper threads, so a bug
//! that never starts a read fails the test at the settlement cap rather than
//! hanging it. Every wait is a settlement wait or a pump on mail wakes, never
//! a sleep.
#![cfg(unix)]
// Deliberate embedders: the scenarios build a bare `TestChassis` through
// `Builder::new`, and spawn the FIFO writer threads that stand in for a slow
// disk.
#![allow(clippy::disallowed_methods)]

use std::ffi::CString;
use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::thread;

use aether_actor::actor;
use aether_data::{MailId, SessionToken, Uuid};
use aether_fs::{
    FsCapability, Load, LoadResult, Loaded, MAX_LOADS_IN_FLIGHT, NamespaceAddr, NamespaceRoots, Read, ReadResult,
};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::builder::{Builder, PassiveChassis, ReplyTarget};
use aether_substrate::chassis::error::BootError;
use aether_substrate::mail::outbound::EgressEvent;
use aether_substrate::testing::{
    PumpedDriver, TestChassis, await_settled, cleanup, decode_session_reply, fresh_substrate_and_rx, scratch_dir,
};

/// Ask the requester to load `addr` under `tag`.
#[aether_data::kind(name = "test.fs_offload.start_load")]
struct StartLoad {
    addr: NamespaceAddr,
    tag: u64,
}

/// A requester that loads through `aether.fs` and records what comes back:
/// each `LoadResult`, each `Loaded`, and the chain root of the turn that
/// sent each load and of the turn that received each delivery.
struct LoadRequester;

struct LoadRequesterState {
    sent_roots: Vec<Option<MailId>>,
    accepted: Vec<LoadResult>,
    loaded: Vec<(Loaded, Option<MailId>)>,
}

impl LoadRequesterState {
    fn loaded_count(&self) -> usize {
        self.loaded.len()
    }
}

#[actor(singleton, root, depends(FsCapability))]
impl NativeActor for LoadRequester {
    type State = LoadRequesterState;
    type Config = ();
    const NAMESPACE: &'static str = "test.fs_offload.requester";

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<LoadRequesterState, BootError> {
        Ok(LoadRequesterState { sent_roots: Vec::new(), accepted: Vec::new(), loaded: Vec::new() })
    }

    #[handler::tell]
    fn on_start_load(state: &mut Self::State, ctx: &mut NativeCtx<'_>, start: StartLoad) {
        state.sent_roots.push(ctx.in_flight_root());
        ctx.send::<FsCapability>(&Load { addr: start.addr, tag: start.tag });
    }

    #[handler::response]
    fn on_load_result(state: &mut Self::State, _ctx: &mut NativeCtx<'_>, result: LoadResult) {
        state.accepted.push(result);
    }

    #[handler::event]
    fn on_loaded(state: &mut Self::State, ctx: &mut NativeCtx<'_>, loaded: Loaded) {
        state.loaded.push((loaded, ctx.in_flight_root()));
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

/// Write `bytes` into `fifo` from a helper thread: the open blocks until the
/// worker reading the FIFO opens its end, then the write and close let that
/// read finish.
fn feed(fifo: &Path, bytes: &'static [u8]) {
    let fifo = fifo.to_path_buf();
    thread::spawn(move || {
        let mut writer = OpenOptions::new().write(true).open(&fifo).expect("open the FIFO's write end");
        writer.write_all(bytes).expect("write the FIFO");
    });
}

/// Whether a reader has the FIFO open right now: a non-blocking write-open
/// fails with `ENXIO` when none has.
fn has_reader(fifo: &Path) -> bool {
    OpenOptions::new().write(true).custom_flags(libc::O_NONBLOCK).open(fifo).is_ok()
}

fn chassis_with_fs(roots: &NamespaceRoots) -> (PassiveChassis<TestChassis>, Receiver<EgressEvent>) {
    let (registry, mailer, rx) = fresh_substrate_and_rx();
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor_configured::<FsCapability>((), roots.clone())
        .build_passive()
        .expect("aether.fs boots");

    (chassis, rx)
}

fn session(correlation: u64) -> ReplyTarget {
    ReplyTarget::Session { session: SessionToken(Uuid::nil()), correlation }
}

fn requester(sandbox: &Sandbox) -> PumpedDriver<LoadRequester> {
    let (chassis, _rx) = chassis_with_fs(&sandbox.roots);
    PumpedDriver::boot(chassis, (), ())
}

fn save(path: &str) -> NamespaceAddr {
    NamespaceAddr::new("save", path)
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
    let (chassis, rx) = chassis_with_fs(&sandbox.roots);
    let fs_ref = chassis.actor_ref::<FsCapability>();

    let (_, slow_settled) = chassis.send_tracked(fs_ref, &Read { addr: save("slow.bin") }, Some(session(1)));
    let (_, quick_settled) = chassis.send_tracked(fs_ref, &Read { addr: save("quick.bin") }, Some(session(2)));

    await_settled(&quick_settled, "the quick read beside a blocked one");
    match decode_session_reply::<ReadResult>(&rx) {
        ReadResult::Ok { addr, bytes } => {
            assert_eq!(addr.path, "quick.bin");
            assert_eq!(bytes.contiguous(), Some(&b"quick"[..]));
        }
        ReadResult::Err { error, .. } => panic!("quick read failed: {error:?}"),
    }
    assert!(slow_settled.try_recv().is_err(), "the blocked read's chain settled before its reply");

    feed(&slow, b"slow");
    await_settled(&slow_settled, "the blocked read once fed");
    match decode_session_reply::<ReadResult>(&rx) {
        ReadResult::Ok { addr, bytes } => {
            assert_eq!(addr.path, "slow.bin");
            assert_eq!(bytes.contiguous(), Some(&b"slow"[..]));
        }
        ReadResult::Err { error, .. } => panic!("slow read failed: {error:?}"),
    }
}

/// Bug caught: a load's read holding the caller's chain (the caller's frame
/// would wait for the disk), or `Loaded` sent on the caller's chain rather
/// than a fresh one.
#[test]
fn load_is_accepted_and_its_chain_settles_before_the_bytes_arrive() {
    let sandbox = Sandbox::new("load-settles");
    let fifo = sandbox.fifo("asset.bin");
    let mut driver = requester(&sandbox);
    let requester_ref = driver.chassis().actor_ref::<LoadRequester>();

    driver.send_and_settle(requester_ref, &StartLoad { addr: save("asset.bin"), tag: 42 }, None);

    let (accepted, loaded) =
        driver.read_state(|state| (state.accepted.clone(), state.loaded_count())).expect("the requester is live");
    assert!(
        matches!(accepted.as_slice(), [LoadResult::Accepted { tag: 42, .. }]),
        "expected one acceptance, got {accepted:?}",
    );
    assert_eq!(loaded, 0, "the bytes arrived before the FIFO was fed");

    feed(&fifo, b"asset bytes");
    driver.pump_until("the load's delivery", |state| state.loaded_count() == 1);

    driver
        .read_state(|state| {
            let [(Loaded::Ok { addr, tag, bytes }, delivered_root)] = state.loaded.as_slice() else {
                panic!("expected one Loaded::Ok, got {:?}", state.loaded);
            };
            assert_eq!(addr.path, "asset.bin");
            assert_eq!(*tag, 42);
            assert_eq!(bytes.contiguous(), Some(&b"asset bytes"[..]));

            let sent_root = state.sent_roots[0];
            assert!(delivered_root.is_some(), "Loaded arrived on no chain");
            assert_ne!(*delivered_root, sent_root, "Loaded rode the chain that sent the load");
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
    let fifos: Vec<PathBuf> = (0..total).map(|tag| sandbox.fifo(&format!("asset-{tag}.bin"))).collect();
    let mut driver = requester(&sandbox);
    let requester_ref = driver.chassis().actor_ref::<LoadRequester>();

    let roots: Vec<MailId> = (0..total)
        .map(|tag| {
            let start = StartLoad { addr: save(&format!("asset-{tag}.bin")), tag: tag as u64 };
            driver.send_tracked(requester_ref, &start, None)
        })
        .collect();
    driver.settle(&roots);

    let last = &fifos[MAX_LOADS_IN_FLIGHT];
    assert!(!has_reader(last), "the load past the bound started before a slot freed");

    feed(&fifos[0], b"first");
    driver.pump_until("the first load", |state| state.loaded_count() == 1);
    for fifo in &fifos[1..] {
        feed(fifo, b"rest");
    }
    driver.pump_until("every load", |state| state.loaded_count() == total);

    driver
        .read_state(|state| {
            let mut tags: Vec<u64> = state
                .loaded
                .iter()
                .map(|(loaded, _)| match loaded {
                    Loaded::Ok { tag, .. } => *tag,
                    Loaded::Err { tag, error, .. } => panic!("load {tag} failed: {error:?}"),
                })
                .collect();
            tags.sort_unstable();
            let expected: Vec<u64> = (0..total as u64).collect();
            assert_eq!(tags, expected, "every load is delivered exactly once");
        })
        .expect("the requester is live");
}
