//! `aether.process` cap (ADR-0078 Phase 1). Wraps the hub chassis's
//! bespoke `spawn_substrate` / `terminate_substrate` plumbing in a
//! `#[bridge] mod native` cap so process supervision shares the same
//! shape, testability, and composition as every other chassis cap.
//!
//! Hub-only — desktop / headless / test-bench chassis don't load this
//! cap (they don't supervise child processes).
//!
//! In PR 1 the cap is registered but the MCP coordinator still calls
//! the bespoke async functions in `hub::spawn`. PR 2 routes the
//! `spawn_substrate` / `terminate_substrate` MCP tools through this
//! cap and retires the registry-side child storage. PR 3 tacks an
//! editorial note onto ADR-0009.

use aether_kinds::{ProcessExited, Spawn, SpawnResult, Terminate, TerminateResult};

use crate::hub::registry::EngineRegistry;
use crate::hub::spawn::PendingSpawns;

#[aether_actor::bridge]
mod native {
    use super::{
        EngineRegistry, PendingSpawns, ProcessExited, Spawn, SpawnResult, Terminate,
        TerminateResult,
    };
    use aether_actor::{MailCtx, actor};
    use aether_data::Kind;
    use aether_substrate::capability::BootError;
    use aether_substrate::native_actor::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::outbound::HubOutbound;
    use std::collections::HashMap;
    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use tokio::process::Child;
    use tokio::runtime::Handle;
    use tokio::task::JoinHandle as TokioJoinHandle;

    use crate::hub::spawn::{
        DEFAULT_HANDSHAKE_TIMEOUT, DEFAULT_TERMINATE_GRACE, SpawnOpts, spawn_substrate_no_adopt,
    };
    use crate::hub::wire::{EngineId, Uuid};

    /// Per-engine bookkeeping for a spawned child. The reaper task
    /// owns the `tokio::process::Child` outright (Child::wait needs
    /// `&mut self` and is single-shot, so dual ownership doesn't
    /// compose); the cap keeps the PID so the terminate handler can
    /// signal the process directly via `libc::kill` and the reaper's
    /// `JoinHandle` so terminate can await the reaped exit code.
    ///
    /// Reaper resolution publishes the exit code through the
    /// `JoinHandle<Option<i32>>`. On natural exit the reaper itself
    /// broadcasts `aether.process.exited`; on `Terminate` the
    /// terminate handler reads the same exit code from the join
    /// output (so the reply carries the actual status, not a stale
    /// `None`) and the reaper still broadcasts.
    struct ChildSlot {
        pid: u32,
        reaper: TokioJoinHandle<Option<i32>>,
    }

    /// Resolved configuration `ProcessCapability::init` consumes. Every
    /// piece of hub state the cap needs to drive child lifecycle:
    /// the engine registry (for record lookup on terminate), the
    /// shared pending-handshake table (so the engine handshake path
    /// resolves PIDs the cap spawned), the listener address to inject
    /// as `AETHER_HUB_URL`, and a `tokio::runtime::Handle` so the
    /// dispatcher-thread sync handlers can drive async spawn / wait /
    /// terminate work on the hub's existing tokio runtime.
    pub struct ProcessCapabilityConfig {
        pub engines: EngineRegistry,
        pub pending: PendingSpawns,
        pub hub_engine_addr: SocketAddr,
        pub runtime: Handle,
    }

    /// `aether.process` mailbox cap. Owns the spawned children
    /// directly (not via the registry side-map) so the reaper task
    /// can take ownership of `Child` and `wait()` on it without
    /// racing the bespoke `terminate_substrate` MCP path that PR 1
    /// still leaves wired. PR 2 retires the registry-side storage
    /// once the MCP coordinator routes through this cap.
    pub struct ProcessCapability {
        engines: EngineRegistry,
        pending: PendingSpawns,
        hub_engine_addr: SocketAddr,
        runtime: Handle,
        outbound: Option<Arc<HubOutbound>>,
        /// Cap-local bookkeeping for every spawned child. Each
        /// [`ChildSlot`] carries the child's PID (so the terminate
        /// handler can signal it directly via `libc::kill`) and the
        /// reaper task's `JoinHandle` (so terminate can await the
        /// reaped exit code). The reaper task itself owns the
        /// `tokio::process::Child` — it has to, because `Child::wait`
        /// is `&mut self` and single-shot.
        children: Arc<Mutex<HashMap<EngineId, ChildSlot>>>,
    }

    #[actor]
    impl NativeActor for ProcessCapability {
        type Config = ProcessCapabilityConfig;
        const NAMESPACE: &'static str = "aether.process";

        fn init(
            cfg: ProcessCapabilityConfig,
            ctx: &mut NativeInitCtx<'_>,
        ) -> Result<Self, BootError> {
            Ok(Self {
                engines: cfg.engines,
                pending: cfg.pending,
                hub_engine_addr: cfg.hub_engine_addr,
                runtime: cfg.runtime,
                outbound: ctx.mailer().outbound().cloned(),
                children: Arc::new(Mutex::new(HashMap::new())),
            })
        }

        /// Spawn a substrate binary as a hub child and wait for its
        /// `Hello` handshake.
        ///
        /// # Agent
        /// Reply: `SpawnResult`. On `Ok`, a per-child reaper task is
        /// spawned that emits an `aether.process.exited` broadcast
        /// when the child terminates (whether via `Terminate` mail
        /// or external exit).
        #[handler]
        fn on_spawn(&self, ctx: &mut NativeCtx<'_>, mail: Spawn) {
            let opts = SpawnOpts {
                binary_path: PathBuf::from(mail.binary_path),
                args: mail.args,
                env: mail.env.into_iter().map(|v| (v.key, v.value)).collect(),
                handshake_timeout: mail
                    .handshake_timeout_ms
                    .map(|ms| Duration::from_millis(ms as u64))
                    .unwrap_or(DEFAULT_HANDSHAKE_TIMEOUT),
            };

            let result = self.runtime.block_on(spawn_substrate_no_adopt(
                opts,
                self.hub_engine_addr,
                &self.pending,
            ));

            match result {
                Ok((engine_id, child)) => {
                    let pid = self.engines.get(&engine_id).map(|r| r.pid).unwrap_or(0);
                    self.adopt_and_reap(engine_id, child);
                    ctx.reply(&SpawnResult::Ok {
                        engine_id: engine_id.0.to_string(),
                        pid,
                    });
                }
                Err(e) => ctx.reply(&SpawnResult::Err {
                    error: format!("spawn failed: {e}"),
                }),
            }
        }

        /// Terminate a hub-spawned substrate.
        ///
        /// # Agent
        /// Reply: `TerminateResult`. SIGTERM → `grace_ms` window →
        /// SIGKILL escalation. Errors on unknown engine id, or an
        /// engine the cap didn't spawn (externally connected).
        #[handler]
        fn on_terminate(&self, ctx: &mut NativeCtx<'_>, mail: Terminate) {
            let engine_id = match parse_engine_id(&mail.engine_id) {
                Ok(id) => id,
                Err(e) => {
                    ctx.reply(&TerminateResult::Err { error: e });
                    return;
                }
            };

            let slot = self.children.lock().unwrap().remove(&engine_id);
            let Some(ChildSlot { pid, reaper }) = slot else {
                ctx.reply(&TerminateResult::Err {
                    error: format!(
                        "engine {} is not hub-spawned by ProcessCapability; \
                         no child handle in cap",
                        mail.engine_id
                    ),
                });
                return;
            };

            let grace = mail
                .grace_ms
                .map(|ms| Duration::from_millis(ms as u64))
                .unwrap_or(DEFAULT_TERMINATE_GRACE);

            let (exit_code, sigkilled) = self
                .runtime
                .block_on(signal_and_await_reaper(pid, reaper, grace));
            // Reaper itself broadcasts `aether.process.exited` once
            // `Child::wait` resolves; the terminate handler doesn't
            // double-broadcast.
            ctx.reply(&TerminateResult::Ok {
                exit_code,
                sigkilled,
            });
        }
    }

    impl ProcessCapability {
        /// Hand the freshly-spawned `Child` to a tokio reaper task
        /// that owns it for life and resolves with the child's exit
        /// code via the returned `JoinHandle`. The cap stores the PID
        /// (so terminate can `libc::kill` it) and the join handle (so
        /// terminate can await the reap) in the children map.
        ///
        /// On natural exit, the reaper broadcasts
        /// `aether.process.exited` itself. On `Terminate` mail or
        /// chassis shutdown, the terminate path signals the PID, the
        /// reaper observes `Child::wait` resolution, and the broadcast
        /// fires once — same path either way.
        fn adopt_and_reap(&self, engine_id: EngineId, mut child: Child) {
            let pid = child.id().unwrap_or(0);
            let outbound = self.outbound.clone();
            let children = Arc::clone(&self.children);
            let reaper = self.runtime.spawn(async move {
                let exit_code = match child.wait().await {
                    Ok(status) => status.code(),
                    Err(_) => None,
                };
                children.lock().unwrap().remove(&engine_id);
                if let Some(out) = outbound {
                    broadcast_exited_via(&out, engine_id, exit_code, "exited".to_owned());
                }
                exit_code
            });
            self.children
                .lock()
                .unwrap()
                .insert(engine_id, ChildSlot { pid, reaper });
        }

        /// Drain every spawned child and run the SIGTERM → grace →
        /// SIGKILL escalation against each in parallel. Called by the
        /// hub chassis's shutdown coordinator so SIGINT / SIGTERM on
        /// the hub doesn't orphan children into init. Each child's
        /// reaper task observes `Child::wait` resolution and
        /// broadcasts `aether.process.exited` itself.
        pub async fn shutdown_all(&self, grace: Duration) {
            let drained: Vec<(EngineId, ChildSlot)> = {
                let mut guard = self.children.lock().unwrap();
                guard.drain().collect()
            };
            if drained.is_empty() {
                return;
            }
            tracing::info!(
                target: "aether_substrate::process",
                count = drained.len(),
                "terminating spawned child(ren) at chassis shutdown",
            );
            let handles: Vec<_> = drained
                .into_iter()
                .map(|(_, ChildSlot { pid, reaper })| {
                    tokio::spawn(signal_and_await_reaper(pid, reaper, grace))
                })
                .collect();
            for h in handles {
                if let Err(e) = h.await {
                    tracing::warn!(
                        target: "aether_substrate::process",
                        error = %e,
                        "shutdown terminate join failed",
                    );
                }
            }
        }
    }

    /// Signal a child PID with SIGTERM, give the reaper task up to
    /// `grace` to resolve, escalate to SIGKILL if the reaper hasn't
    /// resolved by then, and finally await the reaper to capture the
    /// actual exit code. Returns `(exit_code, sigkilled)` matching
    /// the wire shape's `Ok` variant.
    ///
    /// On non-unix the SIGTERM step is a no-op (tokio's `Child` has
    /// no cross-platform soft-kill primitive). The grace timer still
    /// fires; the SIGKILL escalation also no-ops, leaving the wait
    /// to resolve only when the child exits on its own. This matches
    /// the pre-cap `terminate_substrate` helper's behavior on
    /// non-unix builds.
    async fn signal_and_await_reaper(
        pid: u32,
        reaper: TokioJoinHandle<Option<i32>>,
        grace: Duration,
    ) -> (Option<i32>, bool) {
        #[cfg(unix)]
        if pid != 0 {
            // SAFETY: `libc::kill` is always sound to call; a bad pid
            // returns an error we don't need to inspect.
            unsafe {
                libc::kill(pid as libc::pid_t, libc::SIGTERM);
            }
        }

        // Pin the reaper handle so we can poll it via `&mut` against
        // a select!, drop in to a SIGKILL escalation if the grace
        // window elapses, and then `await` it again to extract the
        // exit code. A bare `tokio::time::timeout(grace, reaper)`
        // drops the handle on elapse, which loses the post-SIGKILL
        // exit-code reap.
        tokio::pin!(reaper);
        let mut sigkilled = false;
        tokio::select! {
            biased;
            joined = &mut reaper => return (joined.unwrap_or(None), sigkilled),
            _ = tokio::time::sleep(grace) => {
                sigkilled = true;
                #[cfg(unix)]
                if pid != 0 {
                    unsafe {
                        libc::kill(pid as libc::pid_t, libc::SIGKILL);
                    }
                }
            }
        }

        let exit_code = reaper.await.unwrap_or(None);
        (exit_code, sigkilled)
    }

    fn broadcast_exited_via(
        outbound: &HubOutbound,
        engine_id: EngineId,
        exit_code: Option<i32>,
        reason: String,
    ) {
        let payload = ProcessExited {
            engine_id: engine_id.0.to_string(),
            exit_code,
            reason,
        };
        let bytes = match postcard::to_allocvec(&payload) {
            Ok(b) => b,
            Err(e) => {
                tracing::warn!(
                    target: "aether_substrate::process",
                    error = %e,
                    "failed to encode ProcessExited",
                );
                return;
            }
        };
        outbound.egress_broadcast(<ProcessExited as Kind>::NAME, bytes, None, 0);
    }

    fn parse_engine_id(s: &str) -> Result<EngineId, String> {
        Uuid::parse_str(s)
            .map(EngineId)
            .map_err(|e| format!("engine_id is not a valid UUID: {e}"))
    }

    #[cfg(test)]
    mod tests {
        use super::super::EngineRegistry;
        use super::{Arc, PendingSpawns, ProcessCapability, ProcessCapabilityConfig};
        use aether_actor::Actor;
        use aether_data::Kind;
        use aether_kinds::{EnvVar, ProcessExited, Spawn, SpawnResult};
        use aether_substrate::capability::ChassisBuilder;
        use aether_substrate::mail::ReplyTo;
        use aether_substrate::mailer::Mailer;
        use aether_substrate::native_actor::NativeCtx;
        use aether_substrate::native_transport::NativeTransport;
        use aether_substrate::outbound::{EgressEvent, HubOutbound};
        use aether_substrate::registry::Registry;
        use std::net::SocketAddr;
        use std::sync::mpsc;
        use std::time::Duration;
        use tokio::runtime::Runtime;

        fn fresh_substrate() -> (Arc<Registry>, Arc<Mailer>) {
            let registry = Arc::new(Registry::new());
            for d in aether_kinds::descriptors::all() {
                let _ = registry.register_kind_with_descriptor(d);
            }
            (registry, Arc::new(Mailer::new()))
        }

        fn cfg(rt: &Runtime, addr: SocketAddr) -> ProcessCapabilityConfig {
            ProcessCapabilityConfig {
                engines: EngineRegistry::new(),
                pending: PendingSpawns::new(),
                hub_engine_addr: addr,
                runtime: rt.handle().clone(),
            }
        }

        fn unreachable_addr() -> SocketAddr {
            "127.0.0.1:1".parse().unwrap()
        }

        /// Boot the cap through `with_actor` and verify the mailbox is
        /// claimed under `aether.process`. No spawn happens — this is
        /// the registration smoke test that PR 2 will build on.
        #[test]
        fn capability_boots_and_registers_mailbox() {
            let rt = Runtime::new().expect("tokio runtime");
            let (registry, mailer) = fresh_substrate();
            let chassis = ChassisBuilder::new(Arc::clone(&registry), Arc::clone(&mailer))
                .with_actor::<ProcessCapability>(cfg(&rt, unreachable_addr()))
                .build()
                .expect("process capability boots");
            assert!(
                registry.lookup(ProcessCapability::NAMESPACE).is_some(),
                "aether.process mailbox registered",
            );
            chassis.shutdown();
        }

        /// Manually constructed cap + a fully-wired test mailer so we
        /// can drive `on_spawn` / `on_terminate` without going through
        /// the dispatcher (much cheaper than the chassis path and
        /// produces deterministic egress for assertions).
        struct TestFixture {
            cap: ProcessCapability,
            rx: mpsc::Receiver<EgressEvent>,
            transport: NativeTransport,
            _rt: Runtime,
        }

        impl TestFixture {
            fn new() -> Self {
                let rt = Runtime::new().expect("tokio runtime");
                let (mailer, outbound, rx) = test_mailer_and_rx();
                let transport = NativeTransport::new_for_test(mailer, aether_data::MailboxId(0));
                let cap = ProcessCapability {
                    engines: EngineRegistry::new(),
                    pending: PendingSpawns::new(),
                    hub_engine_addr: unreachable_addr(),
                    runtime: rt.handle().clone(),
                    outbound: Some(outbound),
                    children: Arc::new(super::Mutex::new(super::HashMap::new())),
                };
                Self {
                    cap,
                    rx,
                    transport,
                    _rt: rt,
                }
            }

            fn ctx(&self, sender: ReplyTo) -> NativeCtx<'_> {
                NativeCtx::new(&self.transport, sender)
            }
        }

        fn test_mailer_and_rx() -> (Arc<Mailer>, Arc<HubOutbound>, mpsc::Receiver<EgressEvent>) {
            let (outbound, rx) = HubOutbound::attached_loopback();
            let mailer = Arc::new(Mailer::new());
            mailer.wire(Arc::new(Registry::new()));
            mailer.wire_outbound(Arc::clone(&outbound));
            (mailer, outbound, rx)
        }

        fn session_sender() -> ReplyTo {
            ReplyTo::to(aether_substrate::mail::ReplyTarget::Session(
                aether_data::SessionToken(aether_data::Uuid::nil()),
            ))
        }

        fn decode_reply<K: Kind + serde::de::DeserializeOwned>(
            rx: &mpsc::Receiver<EgressEvent>,
        ) -> K {
            let event = rx.recv_timeout(Duration::from_secs(2)).expect("egress");
            let EgressEvent::ToSession {
                kind_name, payload, ..
            } = event
            else {
                panic!("expected ToSession egress, got {event:?}");
            };
            assert_eq!(kind_name, K::NAME);
            postcard::from_bytes(&payload).unwrap()
        }

        fn drain_for<K: Kind>(
            rx: &mpsc::Receiver<EgressEvent>,
            timeout: Duration,
        ) -> Option<EgressEvent> {
            let deadline = std::time::Instant::now() + timeout;
            loop {
                let remaining = deadline.checked_duration_since(std::time::Instant::now())?;
                let event = rx.recv_timeout(remaining).ok()?;
                let kind = match &event {
                    EgressEvent::ToSession { kind_name, .. } => kind_name.as_str(),
                    EgressEvent::Broadcast { kind_name, .. } => kind_name.as_str(),
                    _ => continue,
                };
                if kind == K::NAME {
                    return Some(event);
                }
            }
        }

        /// `Spawn` against a child that never handshakes resolves as
        /// `SpawnResult::Err` (handshake timeout) within the configured
        /// window. Validates the sync handler can drive an async spawn
        /// + handshake-timeout via the runtime handle.
        #[test]
        #[cfg(unix)]
        fn spawn_handshake_timeout_replies_err() {
            let fix = TestFixture::new();
            let mut ctx = fix.ctx(session_sender());
            fix.cap.on_spawn(
                &mut ctx,
                Spawn {
                    binary_path: "/bin/sh".to_owned(),
                    args: vec!["-c".to_owned(), "sleep 60".to_owned()],
                    env: Vec::<EnvVar>::new(),
                    handshake_timeout_ms: Some(150),
                },
            );
            match decode_reply::<SpawnResult>(&fix.rx) {
                SpawnResult::Err { error } => {
                    assert!(
                        error.contains("handshake") || error.contains("Handshake"),
                        "expected handshake-timeout reason, got {error}",
                    );
                }
                SpawnResult::Ok { .. } => panic!("expected Err, got Ok"),
            }
        }

        /// Reaper integration test: spawn a `/bin/sh` child that exits
        /// shortly via the no-adopt helper directly, hand it to the
        /// cap's adopt-and-reap path, and assert
        /// `aether.process.exited` fires on broadcast within a
        /// reasonable window. Bypasses the spawn handshake — what
        /// we're exercising here is the reaper task plumbing itself.
        #[test]
        #[cfg(unix)]
        fn reaper_emits_process_exited_when_child_exits() {
            let fix = TestFixture::new();
            let pid = std::process::id();
            let engine_id = super::EngineId(aether_data::Uuid::from_u128(0xC0FFEE_u128));

            // Hand-spawn a short-lived child (no handshake — cap's
            // reaper machinery doesn't depend on the engine record).
            let child = fix
                ._rt
                .block_on(async {
                    tokio::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg("exit 7")
                        .stdin(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn()
                })
                .expect("spawn /bin/sh");
            let _ = pid;

            fix.cap.adopt_and_reap(engine_id, child);

            let event = drain_for::<ProcessExited>(&fix.rx, Duration::from_secs(3))
                .expect("ProcessExited within 3s");
            let payload = match event {
                EgressEvent::Broadcast { payload, .. } => payload,
                EgressEvent::ToSession { payload, .. } => payload,
                _ => panic!("unexpected event"),
            };
            let exited: ProcessExited = postcard::from_bytes(&payload).unwrap();
            assert_eq!(exited.engine_id, engine_id.0.to_string());
            assert_eq!(exited.exit_code, Some(7));
            assert_eq!(exited.reason, "exited");
        }

        /// Regression for the smoke-found bug where `on_terminate`
        /// returned `Ok { None, false }` without actually killing the
        /// child: the reaper had taken the `Child` immediately, the
        /// terminate path's slot was empty, and the child kept
        /// running. Post-fix, the terminate path signals the PID via
        /// `libc::kill` and awaits the reaper's exit code — so a
        /// `/bin/sh` child that responds to SIGTERM exits cleanly
        /// inside the grace window.
        #[test]
        #[cfg(unix)]
        fn signal_and_await_reaper_kills_responsive_child_within_grace() {
            let fix = TestFixture::new();
            let engine_id = super::EngineId(aether_data::Uuid::from_u128(0xBEEF_u128));
            let child = fix
                ._rt
                .block_on(async {
                    tokio::process::Command::new("/bin/sh")
                        .arg("-c")
                        .arg("sleep 60")
                        .stdin(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn()
                })
                .expect("spawn /bin/sh");
            let pid = child.id().expect("pid available");

            fix.cap.adopt_and_reap(engine_id, child);

            // Pull the slot out the same way the terminate handler
            // does and await the signal+reap dance.
            let super::ChildSlot {
                pid: slot_pid,
                reaper,
            } = fix
                .cap
                .children
                .lock()
                .unwrap()
                .remove(&engine_id)
                .expect("slot");
            assert_eq!(slot_pid, pid);

            let (exit_code, sigkilled) = fix._rt.block_on(super::signal_and_await_reaper(
                pid,
                reaper,
                Duration::from_secs(2),
            ));
            assert!(!sigkilled, "sh should exit on SIGTERM within grace");
            // sh-on-SIGTERM yields no normal `exit_code` (signal-
            // terminated process); accept either None or any value
            // since the contract is "wait resolved".
            let _ = exit_code;

            // Verify the child is actually reaped — `libc::kill(pid, 0)`
            // returns ESRCH when the process is gone.
            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            let errno = std::io::Error::last_os_error().raw_os_error();
            assert!(
                rc != 0 && errno == Some(libc::ESRCH),
                "pid {pid} still signalable after terminate (rc={rc}, errno={errno:?})",
            );
        }

        /// Regression for the SIGKILL escalation path: a child that
        /// ignores SIGTERM doesn't exit within the grace window, so
        /// the cap escalates to SIGKILL and the reply carries
        /// `sigkilled = true`.
        #[test]
        #[cfg(unix)]
        fn signal_and_await_reaper_escalates_to_sigkill_when_grace_expires() {
            let fix = TestFixture::new();
            let engine_id = super::EngineId(aether_data::Uuid::from_u128(0xCAFE_u128));
            let child = fix
                ._rt
                .block_on(async {
                    tokio::process::Command::new("/bin/sh")
                        .arg("-c")
                        // Trap SIGTERM so only SIGKILL takes it down.
                        .arg("trap '' TERM; while :; do :; done")
                        .stdin(std::process::Stdio::null())
                        .kill_on_drop(true)
                        .spawn()
                })
                .expect("spawn /bin/sh");
            let pid = child.id().expect("pid available");
            // Give sh a beat to install the trap before we signal —
            // signaling before the trap is installed yields a clean
            // SIGTERM exit and the test premise collapses.
            std::thread::sleep(Duration::from_millis(100));

            fix.cap.adopt_and_reap(engine_id, child);
            let super::ChildSlot { reaper, .. } = fix
                .cap
                .children
                .lock()
                .unwrap()
                .remove(&engine_id)
                .expect("slot");

            let (_exit_code, sigkilled) = fix._rt.block_on(super::signal_and_await_reaper(
                pid,
                reaper,
                Duration::from_millis(200),
            ));
            assert!(sigkilled, "expected SIGKILL escalation");

            let rc = unsafe { libc::kill(pid as libc::pid_t, 0) };
            let errno = std::io::Error::last_os_error().raw_os_error();
            assert!(
                rc != 0 && errno == Some(libc::ESRCH),
                "pid {pid} still signalable after SIGKILL (rc={rc}, errno={errno:?})",
            );
        }
    }
}

pub use native::ProcessCapabilityConfig;
