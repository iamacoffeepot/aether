//! `aether.engine` — engines capability (issue 763 P4).
//!
//! A `#[bridge(singleton)]` `NativeActor` that supervises a fleet of
//! `EngineProxy` actors — the engine-management surface of the
//! forward-model architecture (issue 763). Three handlers:
//!
//! - **`on_spawn`** ([`SpawnEngine`]) picks a free localhost port,
//!   fork+execs the substrate binary with `AETHER_RPC_PORT` injected,
//!   then boots an `aether.engine.proxy:<id>` child actor that dials
//!   it. The proxy owns the forked child from there — startup-dial
//!   retry, kill-on-failed-boot, kill-on-drop. Reply:
//!   `SpawnEngineResult`.
//! - **`on_list`** ([`ListEngines`]) reports every supervised engine.
//! - **`on_terminate`** ([`TerminateEngine`]) forwards the kind to the
//!   engine's proxy (which SIGKILLs its substrate and self-shuts-down)
//!   and drops the table entry. Reply: `TerminateEngineResult`.
//!
//! ## Scope (issue 763 P4 vs P5)
//!
//! P4 is the cap itself: spawn / list / terminate. The hub RPC
//! server's `engine = Some(_)` routing — which drives `ForwardEnvelope`
//! at a proxy on behalf of an external RPC client — and the
//! `describe_kinds` / `describe_component` proxy handlers land in P5
//! alongside the `aether-mcp` extraction; they only have meaning once
//! an out-of-process RPC client drives the hub.
//!
//! Native-only: the cap fork+execs processes and threads the
//! `std::process::Child` handle into the proxy. The `#[bridge]` macro
//! emits the wasm-side marker stub so `aether-capabilities` still
//! compiles for `wasm32`.

// `#[handler]` methods take their decoded payload by value per the
// ADR-0033 dispatch ABI; the macro-generated trampoline owns the
// decoded bytes so callers can't see references.
#![allow(clippy::needless_pass_by_value)]

// Handler-signature kinds must be importable at file root — the
// `#[bridge]` macro emits `impl HandlesKind<K>` markers as siblings of
// the mod.
use aether_kinds::{ListEngines, RouteEnvelope, SpawnEngine, TerminateEngine};
#[cfg(test)]
use std::sync::{Arc, Mutex};

#[aether_actor::bridge(singleton)]
mod server_native {
    use super::{ListEngines, RouteEnvelope, SpawnEngine, TerminateEngine};
    use crate::engine::proxy::{EngineProxy, EngineProxyConfig};
    use aether_actor::{MailCtx, actor};
    use aether_data::{EngineId, Kind, MailboxId, Uuid};
    use aether_kinds::{
        CallSettled, EngineDescriptor, ForwardEnvelope, ListEnginesResult, SpawnEngineResult,
        TerminateEngineResult,
    };
    use aether_substrate::Mail;
    use aether_substrate::Subname;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::{ReplyTarget, ReplyTo};
    use std::collections::HashMap;
    use std::io;
    use std::net::TcpListener;
    use std::process::{Command, Stdio};
    use std::sync::Arc;

    /// One supervised engine in [`EngineServer`]'s table.
    struct EngineEntry {
        /// Mailbox of the `aether.engine.proxy:<id>` actor — the
        /// forward target for [`TerminateEngine`].
        proxy_mailbox: MailboxId,
        /// The localhost RPC port the cap assigned this substrate.
        rpc_port: u16,
    }

    /// Engines capability: supervises a fleet of [`EngineProxy`]
    /// actors, one per spawned substrate. Singleton at `aether.engine`.
    pub struct EngineServer {
        engines: HashMap<EngineId, EngineEntry>,
        /// Monotonic source of `EngineId`s. Engine ids only need to be
        /// unique among the engines this cap currently supervises — a
        /// process-local counter delivers that without a `uuid` rng
        /// dependency. Starts at 1 (`Uuid::from_u128(0)` is the nil
        /// uuid).
        next_engine_seq: u128,
        /// Cached so `on_route` can push a `ForwardEnvelope` at a proxy
        /// while *propagating the inbound reply-to* — `NativeCtx`'s
        /// sends stamp the cap as sender, but a routed call's reply
        /// must reach the originating `RpcServerCapability`, not here.
        mailer: Arc<Mailer>,
    }

    #[actor]
    impl NativeActor for EngineServer {
        type Config = ();
        const NAMESPACE: &'static str = "aether.engine";

        fn init(_config: (), ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self {
                engines: HashMap::new(),
                next_engine_seq: 1,
                mailer: ctx.mailer(),
            })
        }

        /// Enumerate every engine the cap currently supervises.
        ///
        /// # Agent
        /// Send `ListEngines` (fieldless). Reply: `ListEnginesResult
        /// { engines: [{ engine_id, rpc_port }] }`.
        #[handler]
        fn on_list(&mut self, ctx: &mut NativeCtx<'_>, _mail: ListEngines) {
            let engines = self
                .engines
                .iter()
                .map(|(id, entry)| EngineDescriptor {
                    engine_id: id.0.to_string(),
                    rpc_port: entry.rpc_port,
                })
                .collect();
            ctx.reply(&ListEnginesResult { engines });
        }

        /// Fork+exec a substrate binary and connect a proxy to it.
        ///
        /// # Agent
        /// Send `SpawnEngine { binary_path, args }`. The cap assigns a
        /// free localhost port for the substrate's RPC server, injects
        /// it as `AETHER_RPC_PORT`, forks the binary, then boots an
        /// `aether.engine.proxy:<id>` actor that dials it. Reply:
        /// `SpawnEngineResult::Ok { engine_id, rpc_port }` on success,
        /// or `Err { error }` if the fork fails or the substrate never
        /// comes up.
        #[handler]
        fn on_spawn(&mut self, ctx: &mut NativeCtx<'_>, mail: SpawnEngine) {
            let rpc_port = match free_local_port() {
                Ok(port) => port,
                Err(e) => {
                    ctx.reply(&SpawnEngineResult::Err {
                        error: format!("could not allocate an RPC port: {e}"),
                    });
                    return;
                }
            };

            let child = match Command::new(&mail.binary_path)
                .args(&mail.args)
                .env("AETHER_RPC_PORT", rpc_port.to_string())
                .stdin(Stdio::null())
                .spawn()
            {
                Ok(child) => child,
                Err(e) => {
                    ctx.reply(&SpawnEngineResult::Err {
                        error: format!("failed to spawn {}: {e}", mail.binary_path),
                    });
                    return;
                }
            };

            let engine_id = EngineId(Uuid::from_u128(self.next_engine_seq));
            self.next_engine_seq += 1;
            let subname = engine_id.0.simple().to_string();
            let rpc_addr = format!("127.0.0.1:{rpc_port}");

            // `finish()` runs `EngineProxy::init` on this thread: it
            // dials the substrate (retrying while it comes up) and, on
            // failure, kills the child it was handed — so a failed
            // spawn never leaves an orphan for the cap to clean up.
            let result = ctx
                .spawn_child::<EngineProxy>(
                    Subname::Named(&subname),
                    EngineProxyConfig {
                        engine_id,
                        rpc_addr,
                        spawned: Some(child),
                    },
                )
                .finish();

            match result {
                Ok(proxy_mailbox) => {
                    self.engines.insert(
                        engine_id,
                        EngineEntry {
                            proxy_mailbox,
                            rpc_port,
                        },
                    );
                    ctx.reply(&SpawnEngineResult::Ok {
                        engine_id: engine_id.0.to_string(),
                        rpc_port,
                    });
                }
                Err(e) => {
                    ctx.reply(&SpawnEngineResult::Err {
                        error: format!("proxy failed to connect to the spawned substrate: {e:?}"),
                    });
                }
            }
        }

        /// Terminate a supervised engine.
        ///
        /// # Agent
        /// Send `TerminateEngine { engine_id }` (the string from a
        /// prior `SpawnEngineResult` / `ListEnginesResult`). The cap
        /// forwards the kind to the engine's proxy — which SIGKILLs
        /// its substrate and self-shuts-down — and drops its table
        /// entry. Reply: `TerminateEngineResult::Ok`, or `Err { error }`
        /// for an `engine_id` that doesn't parse or names no
        /// supervised engine.
        #[handler]
        fn on_terminate(&mut self, ctx: &mut NativeCtx<'_>, mail: TerminateEngine) {
            let engine_id = match Uuid::parse_str(&mail.engine_id) {
                Ok(uuid) => EngineId(uuid),
                Err(e) => {
                    ctx.reply(&TerminateEngineResult::Err {
                        error: format!("engine_id {:?} is not a valid UUID: {e}", mail.engine_id),
                    });
                    return;
                }
            };

            let Some(entry) = self.engines.remove(&engine_id) else {
                ctx.reply(&TerminateEngineResult::Err {
                    error: format!("no supervised engine {}", mail.engine_id),
                });
                return;
            };

            // Forward to the proxy: it SIGKILLs its substrate and
            // self-shuts-down. Fire-and-forget — the proxy doesn't
            // reply, and the table entry is already gone, so the
            // returned MailId has nothing to subscribe against.
            let payload = mail.encode_into_bytes();
            let _ = ctx.send_envelope_traced(
                entry.proxy_mailbox,
                <TerminateEngine as Kind>::ID,
                &payload,
            );
            ctx.reply(&TerminateEngineResult::Ok);
        }

        /// Relay one mail to a specific engine's substrate.
        ///
        /// # Agent
        /// Not a user-facing tool — the hub's `RpcServerCapability`
        /// sends this when an RPC client addresses a `Call` at
        /// `engine = Some(_)`. The cap looks the engine up in its
        /// table and re-emits a `ForwardEnvelope` at the matching
        /// `aether.engine.proxy:<id>`, propagating the inbound
        /// reply-to verbatim so the substrate's reply (and the proxy's
        /// terminal `CallSettled`) stream straight back to that
        /// `RpcServerCapability`. An unknown / unparseable `engine_id`
        /// is answered with `CallSettled::Err` so the originating wire
        /// call closes instead of hanging.
        #[handler]
        fn on_route(&mut self, ctx: &mut NativeCtx<'_>, mail: RouteEnvelope) {
            let reply_to = ctx.reply_target();
            let ReplyTarget::Component(reply_target) = reply_to.target else {
                // A routed call always carries a Component reply-to
                // (the originating RpcServerCapability). Without one
                // there's nowhere to stream the reply or the
                // CallSettled — drop rather than guess.
                tracing::warn!(
                    target: "aether_substrate::engine_server",
                    engine_id = %mail.engine_id,
                    "engine route: no Component reply-to; dropping",
                );
                return;
            };
            let correlation = reply_to.correlation_id;

            let engine_id = match Uuid::parse_str(&mail.engine_id) {
                Ok(uuid) => EngineId(uuid),
                Err(e) => {
                    settle_err(
                        &self.mailer,
                        reply_target,
                        correlation,
                        format!("engine_id {:?} is not a valid UUID: {e}", mail.engine_id),
                    );
                    return;
                }
            };
            let Some(entry) = self.engines.get(&engine_id) else {
                settle_err(
                    &self.mailer,
                    reply_target,
                    correlation,
                    format!("no supervised engine {}", mail.engine_id),
                );
                return;
            };

            // Re-emit as a ForwardEnvelope at the proxy, carrying the
            // inbound reply-to verbatim so the substrate's reply — and
            // the proxy's CallSettled — route straight back to the
            // originating RpcServerCapability.
            let forward = ForwardEnvelope {
                mailbox: mail.mailbox,
                kind: mail.kind,
                payload: mail.payload,
            };
            self.mailer.push(
                Mail::new(
                    entry.proxy_mailbox,
                    <ForwardEnvelope as Kind>::ID,
                    forward.encode_into_bytes(),
                    1,
                )
                .with_reply_to(reply_to),
            );
        }
    }

    /// Push a `CallSettled::Err` back to `target` (correlation
    /// preserved) so a routed call that the cap can't satisfy — bad
    /// `engine_id`, unknown engine — closes with a wire `ReplyEnd`
    /// instead of leaving the RPC client hanging.
    fn settle_err(mailer: &Arc<Mailer>, target: MailboxId, correlation: u64, error: String) {
        mailer.push(
            Mail::new(
                target,
                <CallSettled as Kind>::ID,
                CallSettled::Err { error }.encode_into_bytes(),
                1,
            )
            .with_reply_to(ReplyTo::with_correlation(ReplyTarget::None, correlation)),
        );
    }

    /// Bind `127.0.0.1:0`, read the OS-assigned port, drop the
    /// listener. A tiny TOCTOU window exists before the substrate
    /// rebinds the port, but on localhost it's negligible — and this
    /// sidesteps both a wire change to report an ephemeral port back
    /// from the substrate and an un-recycled incrementing port pool.
    fn free_local_port() -> io::Result<u16> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let port = listener.local_addr()?.port();
        drop(listener);
        Ok(port)
    }
}

// The sink's handler-signature kinds must be importable at file root
// — the `#[bridge]` macro emits `impl HandlesKind<K>` markers as
// siblings of the `sink` mod.
#[cfg(test)]
use aether_kinds::{ListEnginesResult, SpawnEngineResult, TerminateEngineResult};

/// Reply sink: records the latest reply of each engines-cap reply
/// kind into shared cells so a unit test can drive a handler via
/// `mailer.push` and observe what it replied. Lives at file root (not
/// nested in `mod tests`) so the `#[bridge]` macro's marker emission
/// stays addressable.
// `pub` (not `pub(crate)`) because it's the `NativeActor::Config` of
// the test `ReplySink` below, and the `#[actor]` macro's trait impl is
// fully public — `#[cfg(test)]` keeps it out of the real public API.
#[cfg(test)]
#[derive(Clone, Default)]
pub struct ReplyCells {
    pub list: Arc<Mutex<Option<ListEnginesResult>>>,
    pub spawn: Arc<Mutex<Option<SpawnEngineResult>>>,
    pub terminate: Arc<Mutex<Option<TerminateEngineResult>>>,
}

#[cfg(test)]
#[aether_actor::bridge(singleton)]
mod sink {
    use super::{ListEnginesResult, ReplyCells, SpawnEngineResult, TerminateEngineResult};
    use aether_actor::actor;
    use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
    use aether_substrate::chassis::error::BootError;

    pub struct ReplySink {
        cells: ReplyCells,
    }

    #[actor]
    impl NativeActor for ReplySink {
        type Config = ReplyCells;
        const NAMESPACE: &'static str = "aether.engine.test.reply_sink";

        fn init(cells: ReplyCells, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
            Ok(Self { cells })
        }

        #[handler]
        fn on_list_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: ListEnginesResult) {
            *self
                .cells
                .list
                .lock()
                .expect("test setup: list cell mutex poisoned") = Some(reply);
        }

        #[handler]
        fn on_spawn_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: SpawnEngineResult) {
            *self
                .cells
                .spawn
                .lock()
                .expect("test setup: spawn cell mutex poisoned") = Some(reply);
        }

        #[handler]
        fn on_terminate_result(&mut self, _ctx: &mut NativeCtx<'_>, reply: TerminateEngineResult) {
            *self
                .cells
                .terminate
                .lock()
                .expect("test setup: terminate cell mutex poisoned") = Some(reply);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{EngineServer, ReplyCells, ReplySink};
    use crate::test_chassis::TestChassis;
    use aether_actor::Actor;
    use aether_data::{Kind, mailbox_id_from_name};
    use aether_kinds::descriptors;
    use aether_kinds::{
        ListEngines, SpawnEngine, SpawnEngineResult, TerminateEngine, TerminateEngineResult,
    };
    use aether_substrate::chassis::builder::{Builder, PassiveChassis};
    use aether_substrate::handle_store::HandleStore;
    use aether_substrate::mail::mailer::Mailer;
    use aether_substrate::mail::outbound::HubOutbound;
    use aether_substrate::mail::registry::Registry;
    use aether_substrate::mail::{Mail, ReplyTarget, ReplyTo};
    use std::sync::Arc;
    use std::thread;
    use std::time::{Duration, Instant};

    /// Boot a passive chassis hosting `EngineServer` + the reply sink.
    /// Returns the chassis (kept alive for its dispatcher threads), the
    /// mailer to push requests through, and the sink's cells.
    fn boot() -> (PassiveChassis<TestChassis>, Arc<Mailer>, ReplyCells) {
        let registry = Arc::new(Registry::new());
        for d in descriptors::all() {
            let _ = registry.register_kind_with_descriptor(d);
        }
        let (outbound, _rx) = HubOutbound::attached_loopback();
        let store = Arc::new(HandleStore::new(1024 * 1024));
        let mailer = Arc::new(Mailer::new(Arc::clone(&registry), store).with_outbound(outbound));
        let cells = ReplyCells::default();
        let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
            .with_actor::<EngineServer>(())
            .with_actor::<ReplySink>(cells.clone())
            .build_passive()
            .expect("caps boot");
        (chassis, mailer, cells)
    }

    /// Drive one request kind at `aether.engine`, reply-to the sink,
    /// and block until `probe` sees a recorded reply (or the deadline
    /// passes).
    fn drive<K: Kind + serde::Serialize, T>(
        mailer: &Arc<Mailer>,
        request: &K,
        probe: impl Fn() -> Option<T>,
    ) -> T {
        let server = mailbox_id_from_name(<EngineServer as Actor>::NAMESPACE);
        let sink = mailbox_id_from_name(<ReplySink as Actor>::NAMESPACE);
        mailer.push(
            Mail::new(server, K::ID, request.encode_into_bytes(), 1)
                .with_reply_to(ReplyTo::with_correlation(ReplyTarget::Component(sink), 1)),
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(value) = probe() {
                return value;
            }
            assert!(Instant::now() < deadline, "no reply within deadline");
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// `on_list` on a fresh cap replies with an empty engine list.
    #[test]
    fn list_on_empty_cap_is_empty() {
        let (_chassis, mailer, cells) = boot();
        let result = drive(&mailer, &ListEngines {}, || {
            cells
                .list
                .lock()
                .expect("test setup: list cell mutex poisoned")
                .take()
        });
        assert!(result.engines.is_empty(), "fresh cap supervises no engines");
    }

    /// `on_spawn` with a binary path that doesn't exist fails fast at
    /// the fork — no proxy is spawned, no retry window is entered.
    #[test]
    fn spawn_with_missing_binary_replies_err() {
        let (_chassis, mailer, cells) = boot();
        let result = drive(
            &mailer,
            &SpawnEngine {
                binary_path: "/nonexistent/aether-substrate-does-not-exist".to_owned(),
                args: vec![],
            },
            || {
                cells
                    .spawn
                    .lock()
                    .expect("test setup: spawn cell mutex poisoned")
                    .take()
            },
        );
        match result {
            SpawnEngineResult::Err { error } => {
                assert!(
                    error.contains("failed to spawn"),
                    "unexpected error: {error}"
                );
            }
            SpawnEngineResult::Ok { .. } => panic!("a missing binary must not spawn"),
        }
    }

    /// `on_terminate` with an `engine_id` that isn't a UUID, and one
    /// that is well-formed but names no supervised engine, both reply
    /// `Err` rather than panicking.
    #[test]
    fn terminate_unknown_engine_replies_err() {
        let (_chassis, mailer, cells) = boot();

        let malformed = drive(
            &mailer,
            &TerminateEngine {
                engine_id: "not-a-uuid".to_owned(),
            },
            || {
                cells
                    .terminate
                    .lock()
                    .expect("test setup: terminate cell mutex poisoned")
                    .take()
            },
        );
        assert!(
            matches!(malformed, TerminateEngineResult::Err { .. }),
            "a malformed engine_id should be rejected",
        );

        let unknown = drive(
            &mailer,
            &TerminateEngine {
                engine_id: "00000000-0000-0000-0000-000000000000".to_owned(),
            },
            || {
                cells
                    .terminate
                    .lock()
                    .expect("test setup: terminate cell mutex poisoned")
                    .take()
            },
        );
        assert!(
            matches!(unknown, TerminateEngineResult::Err { .. }),
            "a well-formed but unknown engine_id should be rejected",
        );
    }
}
