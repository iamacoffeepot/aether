#![allow(clippy::wildcard_imports)]
// The hub-shaped test fixtures are deliberate embedders: they build a bare
// `TestChassis` via `Builder::new` rather than the `composed` boot seam.
#![allow(clippy::disallowed_methods)]

use super::*;
pub(super) use crate::args::*;
pub(super) use aether_data::with_tag;
pub(super) use aether_fleet::{FleetConfig, FleetServer};
pub(super) use aether_kinds::descriptors;
pub(super) use aether_rpc::{
    PeerKind, RpcBind, RpcServerCapability, RpcServerConfig, RpcServerHandle, RpcServerParams,
};
pub(super) use aether_substrate::chassis::builder::{Builder, PassiveChassis};
pub(super) use aether_substrate::mail::mailer::Mailer;
pub(super) use aether_substrate::mail::outbound::HubOutbound;
pub(super) use aether_substrate::mail::registry::Registry;
pub(super) use aether_substrate::testing::TestChassis;
use aether_substrate::testing::boot_authority;
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
use aether_actor::{Manual, OutboundReply, actor};
use aether_inventory::kinds::ResolvedName;
use aether_rpc::{CallSettled, ForwardEnvelope, RegisterEngineRoute, RegisterEngineRouteResult};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::chassis::error::BootError;
use aether_substrate::{Erased, Subname};
/// The canned live vocabulary a [`RouteInventorySink`] replies with, plus
/// a counter of how many refresh RPCs it has fielded (issue 2672), and the
/// one engine the sink registers for. Shared by value into the fixture so a
/// test both controls the widened schema the refresh observes and asserts
/// the refresh fired exactly once.
#[derive(Clone)]
pub(super) struct RouteLoopbackParams {
    pub(super) engine: EngineId,
    pub(super) reply: ListKindsResult,
    pub(super) calls: Arc<AtomicUsize>,
}

/// `#[cfg(test)]` loopback engine-proxy double (issue 2672). Registers
/// itself with the `RpcServerCapability` as the route for its one engine,
/// as a real proxy does, so every `engine = Some(engine)` `Call` reaches it
/// as a `ForwardEnvelope`, and answers the harness's
/// `aether.inventory.kinds` refresh RPC locally with a canned
/// [`ListKindsResult`], so the field-mismatch refresh-and-retry path in
/// [`Mcp::resolve_and_encode`] is exercised end-to-end without forking a
/// real substrate + proxy.
///
/// Lives at file root (not nested in `mod tests`) so the `#[actor]`
/// macro's marker emission stays addressable, mirroring the engines-cap's
/// own `ReplySink`. On a `ForwardEnvelope` it replies with the canned
/// vocabulary and the `CallSettled` terminal through its inbound, under the
/// forward's correlation, so the forwarded wire call closes the way a
/// proxy's `CallSettled` would. It registers from `wire`, during the
/// chassis wire pass, before any test connects.
pub(super) struct RouteInventorySink {
    engine: EngineId,
    reply: ListKindsResult,
    calls: Arc<AtomicUsize>,
}

#[derive(Clone)]
pub(super) struct AddressRouteLoopbackParams {
    pub(super) engine: EngineId,
    pub(super) canonical_path: String,
    /// Tagged id → the path `aether.inventory.resolve` answers for it; an id
    /// absent here is answered with no name.
    pub(super) names: HashMap<String, String>,
    pub(super) calls: Arc<Mutex<Vec<ForwardEnvelope>>>,
    pub(super) replies: Arc<Mutex<VecDeque<ScriptedRouteReply>>>,
}

/// Routed-engine double for address-boundary tests. It answers
/// `aether.inventory.resolve_address` with a caller-chosen canonical path
/// that differs from the supplied text, and `aether.inventory.resolve` from a
/// scripted id-to-path map, so a forwarded application envelope shows
/// whether the client sent the engine's answer or its own text. It
/// registers as the route for its one engine from `wire`, like a real proxy.
pub(super) struct AddressRouteSink {
    engine: EngineId,
    canonical_path: String,
    names: HashMap<String, String>,
    calls: Arc<Mutex<Vec<ForwardEnvelope>>>,
    replies: Arc<Mutex<VecDeque<ScriptedRouteReply>>>,
    mailer: Arc<Mailer>,
}

#[actor(singleton, root, depends(RpcServerCapability))]
impl NativeActor for AddressRouteSink {
    type Config = ();
    type Params = AddressRouteLoopbackParams;
    const NAMESPACE: &'static str = "aether.test.address_route";

    fn init((): (), params: AddressRouteLoopbackParams, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            engine: params.engine,
            canonical_path: params.canonical_path,
            names: params.names,
            calls: params.calls,
            replies: params.replies,
            mailer: ctx.mailer(),
        })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        ctx.send::<RpcServerCapability>(&RegisterEngineRoute { engine_id: self.engine });
    }

    #[handler::single]
    #[allow(clippy::unused_self)] // aether-suppression-request: a test double acts on no registration answer
    fn on_route_registered(&mut self, _ctx: &mut NativeCtx<'_>, _mail: RegisterEngineRouteResult) {}

    #[handler::single]
    #[allow(clippy::needless_pass_by_value)] // Native actor handlers receive owned decoded kinds.
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_>, mail: ForwardEnvelope) {
        use aether_substrate::mail::{Mail, Source, SourceAddr};

        self.calls.lock().expect("address-route calls mutex is never poisoned").push(mail.clone());
        let SourceAddr::Component(target) = ctx.reply_target().addr else {
            return;
        };
        let correlation = ctx.reply_target().correlation_id;
        if mail.kind == ResolveAddress::ID {
            self.mailer.push(
                Mail::new(
                    target,
                    ResolveAddressResult::ID,
                    ResolveAddressResult::Ok { canonical_path: self.canonical_path.clone() }.encode_into_bytes(),
                    1,
                )
                .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
            );
        } else if mail.kind == Resolve::ID {
            let request = Resolve::decode_from_bytes(&mail.payload).expect("test resolve request decodes");
            let resolved =
                request.ids.into_iter().map(|id| ResolvedName { name: self.names.get(&id).cloned(), id }).collect();
            self.mailer.push(
                Mail::new(target, ResolveResult::ID, ResolveResult { resolved }.encode_into_bytes(), 1)
                    .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
            );
        } else {
            let reply = self.replies.lock().expect("address-route replies mutex is never poisoned").pop_front();
            let Some(reply) = reply else {
                self.mailer.push(
                    Mail::new(target, CallSettled::ID, CallSettled::Ok.encode_into_bytes(), 1)
                        .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
                );
                return;
            };
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
            if !reply.settle {
                return;
            }
            return;
        }
        self.mailer.push(
            Mail::new(target, CallSettled::ID, CallSettled::Ok.encode_into_bytes(), 1)
                .with_reply_to(Source::with_correlation(SourceAddr::None, correlation)),
        );
    }
}

/// One dynamically-typed reply event emitted by [`ScriptedRouteSink`].
#[derive(Clone)]
pub(super) struct ScriptedReplyEvent {
    pub(super) kind: KindId,
    pub(super) payload: Vec<u8>,
}

/// Scripted outcome for one non-inventory terrain request.
#[derive(Clone)]
pub(super) struct ScriptedRouteReply {
    pub(super) events: Vec<ScriptedReplyEvent>,
    pub(super) settle: bool,
}

/// Dynamic route fixture for task-level terrain relay tests. The live
/// descriptors come only from `inventory`; request envelopes and reply bytes
/// remain opaque so the test never copies the kit's Rust wire vocabulary.
/// One instance serves one engine; instances for several engines share the
/// capture cells.
#[derive(Clone)]
pub(super) struct ScriptedRouteLoopbackParams {
    engine: EngineId,
    inventory: ListKindsResult,
    calls: Arc<Mutex<Vec<ForwardEnvelope>>>,
    replies: Arc<Mutex<VecDeque<ScriptedRouteReply>>>,
}

pub(super) struct ScriptedRouteSink {
    engine: EngineId,
    inventory: ListKindsResult,
    calls: Arc<Mutex<Vec<ForwardEnvelope>>>,
    replies: Arc<Mutex<VecDeque<ScriptedRouteReply>>>,
    mailer: Arc<Mailer>,
}

#[actor(instanced, root, depends(RpcServerCapability))]
impl NativeActor for ScriptedRouteSink {
    // ADR-0156 §3: the canned replies + shared capture cells are construction
    // wiring, not operator config, so they ride the `Params` channel.
    type Config = ();
    type Params = ScriptedRouteLoopbackParams;
    const NAMESPACE: &'static str = "aether.test.scripted_route";

    fn init((): (), params: ScriptedRouteLoopbackParams, ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self {
            engine: params.engine,
            inventory: params.inventory,
            calls: params.calls,
            replies: params.replies,
            mailer: ctx.mailer(),
        })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        ctx.send::<RpcServerCapability>(&RegisterEngineRoute { engine_id: self.engine });
    }

    #[handler::single]
    #[allow(clippy::unused_self)] // aether-suppression-request: a test double acts on no registration answer
    fn on_route_registered(&mut self, _ctx: &mut NativeCtx<'_>, _mail: RegisterEngineRouteResult) {}

    #[handler::single]
    #[allow(clippy::needless_pass_by_value)] // Native actor handlers receive owned decoded kinds.
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_>, mail: ForwardEnvelope) {
        use aether_substrate::mail::{Mail, Source, SourceAddr};

        let reply = if mail.kind == ResolveAddress::ID {
            let request = ResolveAddress::decode_from_bytes(&mail.payload).expect("test resolver request decodes");
            ScriptedRouteReply {
                events: vec![ScriptedReplyEvent {
                    kind: ResolveAddressResult::ID,
                    payload: ResolveAddressResult::Ok { canonical_path: request.address }.encode_into_bytes(),
                }],
                settle: true,
            }
        } else if mail.kind == ListKinds::ID {
            ScriptedRouteReply {
                events: vec![ScriptedReplyEvent {
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
                .unwrap_or(ScriptedRouteReply { events: Vec::new(), settle: true })
        };
        if mail.kind != ResolveAddress::ID {
            self.calls.lock().expect("terrain calls mutex is never poisoned").push(mail);
        }
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

#[actor(singleton, root, depends(RpcServerCapability))]
impl NativeActor for RouteInventorySink {
    // ADR-0156 §3: the canned reply + shared call counter are construction
    // wiring, not operator config, so they ride the `Params` channel.
    type Config = ();
    type Params = RouteLoopbackParams;
    const NAMESPACE: &'static str = "aether.test.route_inventory";

    fn init((): (), params: RouteLoopbackParams, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self { engine: params.engine, reply: params.reply, calls: params.calls })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        ctx.send::<RpcServerCapability>(&RegisterEngineRoute { engine_id: self.engine });
    }

    #[handler::single]
    #[allow(clippy::unused_self)] // aether-suppression-request: a test double acts on no registration answer
    fn on_route_registered(&mut self, _ctx: &mut NativeCtx<'_>, _mail: RegisterEngineRouteResult) {}

    #[handler::manual]
    fn on_forward(&mut self, ctx: &mut NativeCtx<'_, Erased, Manual>, _mail: ForwardEnvelope) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        // The reply is the ReplyEvent the server matches to the in-flight
        // wire call by the echoed correlation; the CallSettled terminal then
        // closes the forwarded call, which has no local chain to settle.
        ctx.reply(&self.reply);
        ctx.reply(&CallSettled::Ok);
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

/// Boot a hub-shaped passive chassis: `RpcServerCapability` + the
/// engines cap + `TraceObserver` (so the `RpcServer`'s local Calls
/// settle and close). Returns the
/// chassis (kept alive for its dispatcher threads) and the RPC
/// port an `RpcSession` dials.
pub(super) fn boot_hub() -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor_configured::<FleetServer>((), FleetConfig::default())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: "test-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
                bind: RpcBind::Boot,
            },
            RpcServerConfig { port: Some(0), port_file: None },
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
pub(super) fn boot_hub_with_inventory(extras: &[KindDescriptor]) -> (PassiveChassis<TestChassis>, u16) {
    use aether_inventory::InventoryCapability;

    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
    }
    for d in extras {
        // Component-defined kinds enter the substrate's `Registry`
        // via `ComponentHostCapability::handle_load` staging a
        // `RegistryBatch::register_kinds` through the ADR-0165 owner;
        // here we shortcut that with a direct register so the test
        // doesn't need a real wasm load lifecycle (the ADR-0091 surface
        // under test is the *projection*, not the loader).
        let _ = registry.register_kind_with_descriptor(&boot_authority(), d.clone());
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor_configured::<FleetServer>((), FleetConfig::default())
        // The inventory cap answers through its ctx read verbs over the
        // chassis registry, so it sees the extra kinds we just wrote.
        .with_actor::<InventoryCapability>(())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: "test-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
                bind: RpcBind::Boot,
            },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("hub caps boot");
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    (chassis, port)
}

/// Hub-shape chassis whose route for `engine` is a [`RouteInventorySink`]
/// loopback (issue 2672) rather than a real proxy, so the harness's
/// `engine = Some(engine)` `aether.inventory.kinds` refresh RPC lands
/// locally and returns `reply`. `calls` counts the refreshes the sink
/// fielded, so a test can assert the refresh-and-retry fired exactly once
/// (no loop). Unlike `boot_hub_with_inventory` this installs no
/// `FleetServer` and no `InventoryCapability` (the sink answers `ListKinds`
/// from the canned reply directly).
pub(super) fn boot_hub_with_route_loopback(
    engine: EngineId,
    reply: ListKindsResult,
    calls: Arc<AtomicUsize>,
) -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for d in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), d);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<RouteInventorySink>(RouteLoopbackParams { engine, reply, calls })
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: "test-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
                bind: RpcBind::Boot,
            },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("hub caps boot");
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    (chassis, port)
}

pub(super) fn boot_hub_with_address_route_loopback(
    engine: EngineId,
    canonical_path: &str,
    calls: Arc<Mutex<Vec<ForwardEnvelope>>>,
) -> (PassiveChassis<TestChassis>, u16) {
    boot_hub_with_address_route_replies(engine, canonical_path, calls, Arc::new(Mutex::new(VecDeque::new())))
}

pub(super) fn boot_hub_with_address_route_replies(
    engine: EngineId,
    canonical_path: &str,
    calls: Arc<Mutex<Vec<ForwardEnvelope>>>,
    replies: Arc<Mutex<VecDeque<ScriptedRouteReply>>>,
) -> (PassiveChassis<TestChassis>, u16) {
    boot_hub_with_address_route(AddressRouteLoopbackParams {
        engine,
        canonical_path: canonical_path.to_owned(),
        names: HashMap::new(),
        calls,
        replies,
    })
}

/// Hub-shape chassis whose route for `params.engine` is an
/// [`AddressRouteSink`], with every answer it gives scripted by `params`.
pub(super) fn boot_hub_with_address_route(params: AddressRouteLoopbackParams) -> (PassiveChassis<TestChassis>, u16) {
    let registry = Arc::new(Registry::new());
    for descriptor in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), descriptor);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor::<AddressRouteSink>(params)
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: "test-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
                bind: RpcBind::Boot,
            },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()
        .expect("address route loopback caps boot");
    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    (chassis, port)
}

/// Hub-shaped route fixture serving live dynamic descriptors and a
/// caller-controlled queue of opaque reply events. One
/// [`ScriptedRouteSink`] is spawned per engine in `engines`, each
/// registering its own engine's route and sharing `calls` and `replies`.
pub(super) fn try_boot_hub_with_scripted_route_loopback(
    engines: &[EngineId],
    inventory: &ListKindsResult,
    calls: &Arc<Mutex<Vec<ForwardEnvelope>>>,
    replies: &Arc<Mutex<VecDeque<ScriptedRouteReply>>>,
) -> Result<(PassiveChassis<TestChassis>, u16), BootError> {
    let registry = Arc::new(Registry::new());
    for descriptor in descriptors::all() {
        let _ = registry.register_kind_with_descriptor(&boot_authority(), descriptor);
    }
    let (outbound, _rx) = HubOutbound::attached_loopback();
    let mailer = Arc::new(Mailer::new(Arc::clone(&registry)).with_outbound(outbound));
    let chassis = Builder::<TestChassis>::new(Arc::clone(&registry), Arc::clone(&mailer))
        .with_actor::<TraceDispatchCapability>(())
        .with_actor_configured::<RpcServerCapability>(
            RpcServerParams {
                peer_kind: PeerKind::Substrate {
                    engine_name: "test-hub".into(),
                    engine_version: "0.1.0".into(),
                    kinds: vec![],
                },
                bind: RpcBind::Boot,
            },
            RpcServerConfig { port: Some(0), port_file: None },
        )
        .build_passive()?;

    for engine in engines {
        let params = ScriptedRouteLoopbackParams {
            engine: *engine,
            inventory: inventory.clone(),
            calls: Arc::clone(calls),
            replies: Arc::clone(replies),
        };
        chassis
            .spawn_actor_for_test::<ScriptedRouteSink>(Subname::Named(&engine.0.simple().to_string()), (), params)
            .finish()
            .expect("scripted route sink spawns and registers its engine");
    }

    let port = chassis.handle::<RpcServerHandle>().expect("RpcServerHandle published").local_port;
    Ok((chassis, port))
}
