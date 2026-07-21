#![allow(clippy::wildcard_imports)]

use super::*;
pub(super) use crate::args::*;
pub(super) use aether_data::{mailbox_id_from_name, mailbox_id_from_path, with_tag};
pub(super) use aether_engine::{EngineConfig, EngineServer};
pub(super) use aether_kinds::descriptors;
pub(super) use aether_rpc::{PeerKind, RpcServerCapability, RpcServerConfig, RpcServerHandle, RpcServerParams};
pub(super) use aether_substrate::chassis::builder::{Builder, PassiveChassis};
pub(super) use aether_substrate::mail::mailer::Mailer;
pub(super) use aether_substrate::mail::outbound::HubOutbound;
pub(super) use aether_substrate::mail::registry::Registry;
pub(super) use aether_substrate::testing::TestChassis;
pub(super) use aether_trace::TraceDispatchCapability;
pub(super) use std::path::PathBuf;
pub(super) use std::process;
pub(super) use std::sync::atomic::{AtomicUsize, Ordering};
pub(super) use std::time::{SystemTime, UNIX_EPOCH};
pub(super) use std::{env as std_env, fs as std_fs};

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

// Imports for the `#[cfg(test)]` `RouteInventorySink` loopback fixture
// (issue 2672). Brought into scope (rather than named by absolute path
// inline) to satisfy the `clippy::absolute_paths` restriction.
use aether_actor::actor;
use aether_rpc::{CallSettled, RouteEnvelope};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
/// The canned live vocabulary a [`RouteInventorySink`] replies with, plus
/// a counter of how many refresh RPCs it has fielded (issue 2672). Shared
/// by value into the fixture so a test both controls the widened schema
/// the refresh observes and asserts the refresh fired exactly once.
#[derive(Clone)]
pub(super) struct RouteLoopbackParams {
    pub(super) reply: ListKindsResult,
    pub(super) calls: Arc<AtomicUsize>,
}

/// `#[cfg(test)]` loopback engines-cap double (issue 2672). Registers at
/// the `aether.engine` mailbox — the id the `RpcServerCapability` routes
/// every `engine = Some` `Call` to via a `RouteEnvelope` — and answers the
/// harness's `aether.inventory.kinds` refresh RPC locally with a canned
/// [`ListKindsResult`], so the
/// field-mismatch refresh-and-retry path in [`Mcp::resolve_and_encode`] is
/// exercised end-to-end without forking a real substrate + proxy.
///
/// Lives at file root (not nested in `mod tests`) so the `#[actor]`
/// macro's marker emission stays addressable, mirroring the engines-cap's
/// own `ReplySink`. It stands in for the real `EngineServer` (never
/// co-installed with it, so the shared `aether.engine` mailbox id is
/// unambiguous): on a `RouteEnvelope` it pushes the reply and the
/// `CallSettled` terminal straight back to the originating server,
/// correlation preserved, so the forwarded wire call closes the way a
/// proxy's `CallSettled` would.
pub(super) struct RouteInventorySink {
    reply: ListKindsResult,
    calls: Arc<AtomicUsize>,
    mailer: Arc<Mailer>,
}

/// One dynamically-typed reply event emitted by [`TerrainRouteSink`].
#[derive(Clone)]
pub(super) struct TerrainReplyEvent {
    pub(super) kind: KindId,
    pub(super) payload: Vec<u8>,
}

/// Scripted outcome for one non-inventory terrain request.
#[derive(Clone)]
pub(super) struct TerrainRouteReply {
    pub(super) events: Vec<TerrainReplyEvent>,
    pub(super) settle: bool,
}

/// Dynamic route fixture for task-level terrain relay tests. The live
/// descriptors come only from `inventory`; request envelopes and reply bytes
/// remain opaque so the test never copies the kit's Rust wire vocabulary.
#[derive(Clone)]
pub(super) struct TerrainRouteLoopbackParams {
    inventory: ListKindsResult,
    calls: Arc<Mutex<Vec<RouteEnvelope>>>,
    replies: Arc<Mutex<VecDeque<TerrainRouteReply>>>,
}

pub(super) struct TerrainRouteSink {
    inventory: ListKindsResult,
    calls: Arc<Mutex<Vec<RouteEnvelope>>>,
    replies: Arc<Mutex<VecDeque<TerrainRouteReply>>>,
    mailer: Arc<Mailer>,
}

#[actor(singleton)]
impl NativeActor for TerrainRouteSink {
    // ADR-0156 §3: the canned replies + shared capture cells are construction
    // wiring, not operator config, so they ride the `Params` channel.
    type Config = ();
    type Params = TerrainRouteLoopbackParams;
    const NAMESPACE: &'static str = "aether.engine";

    fn init((): (), config: TerrainRouteLoopbackParams, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { inventory: config.inventory, calls: config.calls, replies: config.replies, mailer: ctx.mailer() })
    }

    #[handler::single]
    #[allow(clippy::needless_pass_by_value)] // Native actor handlers receive owned decoded kinds.
    fn on_route(&mut self, ctx: &mut NativeCtx<'_>, mail: RouteEnvelope) {
        use aether_substrate::mail::{Mail, Source, SourceAddr};

        self.calls.lock().expect("terrain calls mutex is never poisoned").push(mail.clone());
        let reply = if mail.kind == ListKinds::ID {
            TerrainRouteReply {
                events: vec![TerrainReplyEvent {
                    kind: ListKindsResult::ID,
                    payload: self.inventory.encode_into_bytes(),
                }],
                settle: true,
            }
        } else {
            self.replies
                .lock()
                .expect("terrain replies mutex is never poisoned")
                .pop_front()
                .unwrap_or(TerrainRouteReply { events: Vec::new(), settle: true })
        };
        let SourceAddr::Component(target) = ctx.reply_target().addr else {
            return;
        };
        let correlation = ctx.reply_target().correlation_id;
        for event in reply.events {
            self.mailer.push(
                Mail::new(target, event.kind, event.payload, 1)
                    .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
            );
        }
        if reply.settle {
            self.mailer.push(
                Mail::new(target, CallSettled::ID, CallSettled::Ok.encode_into_bytes(), 1)
                    .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
            );
        }
    }
}

#[actor(singleton)]
impl NativeActor for RouteInventorySink {
    // ADR-0156 §3: the canned reply + shared call counter are construction
    // wiring, not operator config, so they ride the `Params` channel.
    type Config = ();
    type Params = RouteLoopbackParams;
    const NAMESPACE: &'static str = "aether.engine";

    fn init((): (), config: RouteLoopbackParams, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            reply: config.reply,
            calls: config.calls,
            // Cached like the real engines cap does (its `on_route`
            // propagates the inbound reply-to, which `NativeCtx` sends
            // would overwrite with this cap as sender).
            mailer: ctx.mailer(),
        })
    }

    #[handler::single]
    fn on_route(&mut self, ctx: &mut NativeCtx<'_>, _mail: RouteEnvelope) {
        use aether_substrate::mail::{Mail, Source, SourceAddr};

        self.calls.fetch_add(1, Ordering::Relaxed);
        let reply_to = ctx.reply_target();
        // A routed call always carries a Component reply-to (the
        // originating server); without one there's nowhere to stream to.
        let SourceAddr::Component(target) = reply_to.addr else {
            return;
        };
        let correlation = reply_to.correlation_id;

        // ReplyEvent: the canned live vocabulary. The server matches it to
        // the in-flight wire call by the preserved correlation.
        self.mailer.push(
            Mail::new(target, <ListKindsResult as Kind>::ID, self.reply.encode_into_bytes(), 1)
                .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
        );
        // ReplyEnd: a forwarded call has no local chain to settle, so the
        // server's `engine = Some` path waits on this explicit terminal
        // (in production the proxy lifts the substrate's `ReplyEnd` into
        // it). Pushed after the reply so the server writes the ReplyEvent
        // frame first, then closes on the CallSettled.
        self.mailer.push(
            Mail::new(target, <CallSettled as Kind>::ID, CallSettled::Ok.encode_into_bytes(), 1)
                .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
        );
    }
}

/// Write `bytes` to a unique temp file for the `$file` embed tests.
/// The `std_env` / `std_fs` aliases avoid shadowing the module's
/// `tokio::fs`.
pub(super) fn stage_blob_file(tag: &str, bytes: &[u8]) -> PathBuf {
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let path = std_env::temp_dir().join(format!("aether-mcp-blob-{tag}-{}-{nanos}.bin", process::id()));
    std_fs::write(&path, bytes).expect("stage blob temp file");
    path
}

/// Boot a hub-shaped passive chassis: a forwarding
/// `RpcServerCapability` + the engines cap + `TraceObserver` (so
/// the `RpcServer`'s local Calls settle and close). Returns the
/// chassis (kept alive for its dispatcher threads) and the RPC
/// port an `RpcSession` dials.
//noinspection DuplicatedCode -- the inventory variant deliberately extends this typed builder chain.
pub(super) fn boot_hub() -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>((), ())
        .with_actor::<EngineServer>(EngineConfig::default(), ())
        .with_actor::<RpcServerCapability>(
            RpcServerConfig { bind_addr: Some("127.0.0.1:0".into()) },
            RpcServerParams {
            peer_kind: PeerKind::Substrate {
                engine_name: "test-hub".into(),
                engine_version: "0.1.0".into(),
                kinds: vec![],
            },
            #[allow(clippy::disallowed_methods)] // hub-shaped fixture forwards engine-addressed calls to the well-known engines-cap mailbox
            route_target: Some(mailbox_id_from_name("aether.engine")),
        },
        )
        .build_passive()
        .expect("hub caps boot");
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    (chassis, port)
}

/// Connect an `RpcSession` + wrap it in an `Mcp` against a booted
/// hub chassis, with fresh component, reverse-name, and kind-encode
/// caches.
pub(super) fn connect_mcp(port: u16) -> Mcp {
    let session = RpcSession::connect(&format!("127.0.0.1:{port}")).expect("session connects");
    Mcp::new(
        Arc::new(session),
        Arc::new(ComponentCache::default()),
        Arc::new(ReverseNameCache::default()),
        Arc::new(KindsCache::default()),
    )
}

/// Hub-shape chassis with `InventoryCapability` installed and a
/// caller-supplied descriptor registered against the harness's
/// `Registry` — emulating the post-`load_component` state where
/// a component's own kind is in the substrate's vocab but not in
/// `descriptors::all()`. Used by ADR-0091's end-to-end check that
/// the MCP encode path picks the registered kind up via
/// `aether.inventory.kinds`.
//noinspection DuplicatedCode -- this variant extends the base hub with inventory and extra descriptors.
pub(super) fn boot_hub_with_inventory(extras: &[KindDescriptor]) -> (PassiveChassis<TestChassis>, u16) {
    use aether_inventory::InventoryCapability;

    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    for d in extras {
        // Component-defined kinds enter the substrate's `Registry`
        // via `ComponentHostCapability::handle_load` →
        // `register_or_match_all`; here we shortcut that with a
        // direct register so the test doesn't need a real wasm
        // load lifecycle (the ADR-0091 surface under test is the
        // *projection*, not the loader).
        let _ = registry.register_kind_with_descriptor(d.clone());
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>((), ())
        .with_actor::<EngineServer>(EngineConfig::default(), ())
        // The inventory cap pulls `Arc::clone(ctx.mailer().registry())`
        // in `init`, so it sees the same `Registry` we just wrote
        // the extra kinds into.
        .with_actor::<InventoryCapability>((), ())
        .with_actor::<RpcServerCapability>(
            RpcServerConfig { bind_addr: Some("127.0.0.1:0".into()) },
            RpcServerParams {
            peer_kind: PeerKind::Substrate {
                engine_name: "test-hub".into(),
                engine_version: "0.1.0".into(),
                kinds: vec![],
            },
            #[allow(clippy::disallowed_methods)] // hub-shaped fixture forwards engine-addressed calls to the well-known engines-cap mailbox
            route_target: Some(mailbox_id_from_name("aether.engine")),
        },
        )
        .build_passive()
        .expect("hub caps boot");
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    (chassis, port)
}

/// Hub-shape chassis whose `aether.engine` mailbox is a
/// [`RouteInventorySink`] loopback (issue 2672) rather than the real
/// `EngineServer`, so the harness's `engine = Some`
/// `aether.inventory.kinds` refresh RPC lands locally and returns
/// `reply`. `calls` counts the refreshes the sink fielded, so a test
/// can assert the refresh-and-retry fired exactly once (no loop).
/// Unlike `boot_hub_with_inventory` this installs no `EngineServer`
/// (which would warn/settle-err an `engine = Some` for an unregistered
/// engine) and no `InventoryCapability` (the sink answers `ListKinds`
/// from the canned reply directly).
pub(super) fn boot_hub_with_route_loopback(
    reply: ListKindsResult,
    calls: Arc<AtomicUsize>,
) -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>((), ())
        .with_actor::<RouteInventorySink>((), RouteLoopbackParams { reply, calls })
        .with_actor::<RpcServerCapability>(
            RpcServerConfig { bind_addr: Some("127.0.0.1:0".into()) },
            RpcServerParams {
            peer_kind: PeerKind::Substrate {
                engine_name: "test-hub".into(),
                engine_version: "0.1.0".into(),
                kinds: vec![],
            },
            #[allow(clippy::disallowed_methods)] // hub-shaped fixture forwards engine-addressed calls to the well-known engines-cap mailbox
            route_target: Some(mailbox_id_from_name("aether.engine")),
        },
        )
        .build_passive()
        .expect("hub caps boot");
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    (chassis, port)
}

/// Hub-shaped route fixture serving live dynamic terrain descriptors and a
/// caller-controlled queue of opaque reply events.
pub(super) fn try_boot_hub_with_terrain_route_loopback(
    inventory: ListKindsResult,
    calls: Arc<Mutex<Vec<RouteEnvelope>>>,
    replies: Arc<Mutex<VecDeque<TerrainRouteReply>>>,
) -> Result<(PassiveChassis<TestChassis>, u16), BootError> {
    let registry = Arc::new(Registry::new());
    for descriptor in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(descriptor);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>((), ())
        .with_actor::<TerrainRouteSink>((), TerrainRouteLoopbackParams { inventory, calls, replies })
        .with_actor::<RpcServerCapability>(
            RpcServerConfig { bind_addr: Some("127.0.0.1:0".into()) },
            RpcServerParams {
            peer_kind: PeerKind::Substrate {
                engine_name: "test-hub".into(),
                engine_version: "0.1.0".into(),
                kinds: vec![],
            },
            #[allow(clippy::disallowed_methods)] // hub-shaped fixture forwards engine-addressed calls to the well-known engines-cap mailbox
            route_target: Some(mailbox_id_from_name("aether.engine")),
        },
        )
        .build_passive()?;
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    Ok((chassis, port))
}
