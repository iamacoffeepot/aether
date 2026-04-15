// Substrate-side control plane for ADR-0010. Reserved mailbox name:
// `aether.control`. Agents drive runtime component loading / dropping
// / replacement by mailing here; the substrate handles each reserved
// kind inline on the sink-handler thread and replies with a
// matching `aether.control.*_result` addressed at the originating
// session.
//
// Surface area: `load_component`, `drop_component`, `replace_component`.
// Each has its own result kind so an agent can disambiguate replies
// without threading a correlation token through the payload.
//
// Error discipline: agent-visible failures (bad postcard, kind
// conflict, name conflict, invalid WASM, wasmtime instantiation
// error, unknown/wrong-type mailbox) surface as an `Err` variant on
// the matching result. Panics are reserved for invariant violations
// that the agent cannot have caused — e.g. a poisoned lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub, KindDescriptor};
use aether_mail::Kind;
use aether_substrate_mail::{
    DropComponent, DropResult, LoadComponent, LoadResult, ReplaceComponent, ReplaceResult,
};
use serde::{Deserialize, Serialize};
use wasmtime::{Engine, Linker, Module};

use crate::component::Component;
use crate::ctx::SubstrateCtx;
use crate::hub_client::HubOutbound;
use crate::mail::MailboxId;
use crate::queue::MailQueue;
use crate::registry::{Registry, SinkHandler};
use crate::scheduler::ComponentTable;

/// Well-known mailbox name for the ADR-0010 control plane. Mail to
/// this name is routed to the control-plane sink handler rather than
/// a component. Kept as a constant so substrate init, tests, and any
/// future tooling share one spelling.
pub const AETHER_CONTROL: &str = "aether.control";

/// Payload decoded from an incoming `aether.control.load_component`
/// mail. Postcard on the wire; substrate-internal type. Agents that
/// want to trigger a load construct the same shape on their side and
/// pass the bytes through the MCP `send_mail` tool's `payload_bytes`
/// escape hatch (the kind itself is Opaque — ADR-0007's schema-driven
/// encoding doesn't model variable-length byte buffers).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoadComponentPayload {
    /// Raw WASM module bytes. Must satisfy the substrate's component
    /// contract (exports `memory` and `receive`; optional `init`).
    pub wasm: Vec<u8>,
    /// Kind descriptors the component intends to use. The substrate
    /// registers each at load time; conflicts against an existing
    /// descriptor cause the load to fail (ADR-0010 §4).
    pub kinds: Vec<KindDescriptor>,
    /// Optional human-readable mailbox name. If absent, the substrate
    /// picks a default like `"component_<n>"` with a monotonic counter.
    pub name: Option<String>,
}

/// Payload of an outbound `aether.control.load_result` reply. Either
/// the assigned mailbox id and resolved name, or an error describing
/// why the load failed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum LoadResultPayload {
    Ok { mailbox_id: u32, name: String },
    Err { error: String },
}

/// Payload decoded from `aether.control.drop_component`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DropComponentPayload {
    /// Mailbox id previously returned by a `load_component` reply.
    pub mailbox_id: u32,
}

/// Payload of an outbound `aether.control.drop_result` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum DropResultPayload {
    Ok,
    Err { error: String },
}

/// Payload decoded from `aether.control.replace_component`. Target is
/// addressed by id — the new module reuses the mailbox's name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReplaceComponentPayload {
    pub mailbox_id: u32,
    pub wasm: Vec<u8>,
    pub kinds: Vec<KindDescriptor>,
}

/// Payload of an outbound `aether.control.replace_result` reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ReplaceResultPayload {
    Ok,
    Err { error: String },
}

/// State the control-plane sink handler captures. Grouping it in a
/// struct keeps the closure body short and makes the dependencies
/// explicit — useful since the handler needs a broad slice of
/// substrate state (wasmtime, registry, scheduler table, outbound).
pub struct ControlPlane {
    pub engine: Arc<Engine>,
    pub linker: Arc<Linker<SubstrateCtx>>,
    pub registry: Arc<Registry>,
    pub queue: Arc<MailQueue>,
    pub outbound: Arc<HubOutbound>,
    pub components: ComponentTable,
    /// Monotonic counter for default component names. Only consulted
    /// when the load payload omits `name`.
    pub default_name_counter: Arc<AtomicU64>,
}

impl ControlPlane {
    /// Build the sink handler that should be registered against the
    /// `AETHER_CONTROL` mailbox. The returned `SinkHandler` is
    /// `Send + Sync`; it captures `self` by value (through `Arc`s) so
    /// the caller can discard the `ControlPlane` after registration.
    pub fn into_sink_handler(self) -> SinkHandler {
        Arc::new(
            move |kind_name: &str,
                  _origin: Option<&str>,
                  sender: aether_hub_protocol::SessionToken,
                  bytes: &[u8],
                  _count: u32| {
                self.dispatch(kind_name, sender, bytes);
            },
        )
    }

    fn dispatch(&self, kind_name: &str, sender: aether_hub_protocol::SessionToken, bytes: &[u8]) {
        if kind_name == LoadComponent::NAME {
            let result = self.handle_load(bytes);
            self.reply(sender, LoadResult::NAME, &result);
        } else if kind_name == DropComponent::NAME {
            let result = self.handle_drop(bytes);
            self.reply(sender, DropResult::NAME, &result);
        } else if kind_name == ReplaceComponent::NAME {
            let result = self.handle_replace(bytes);
            self.reply(sender, ReplaceResult::NAME, &result);
        } else {
            eprintln!(
                "aether-substrate: {AETHER_CONTROL} received unrecognised kind {kind_name:?} — dropping"
            );
        }
    }

    fn handle_load(&self, bytes: &[u8]) -> LoadResultPayload {
        let payload: LoadComponentPayload = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return LoadResultPayload::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };

        // Kind descriptors first: if any conflict with the registry's
        // existing view, abort before allocating a mailbox or compiling
        // WASM. Pre-check with the read path so a conflict doesn't
        // leave half the new kinds partially registered.
        for kind in &payload.kinds {
            if let Some(id) = self.registry.kind_id(&kind.name)
                && let Some(existing) = self.registry.kind_descriptor(id)
                && existing.encoding != kind.encoding
            {
                return LoadResultPayload::Err {
                    error: format!(
                        "kind {:?} already registered with a different encoding",
                        kind.name
                    ),
                };
            }
        }
        for kind in payload.kinds {
            // Pre-validated above; the only way this can still fail is
            // a concurrent registration, which today doesn't exist (all
            // descriptor-bearing registrations go through here or the
            // single init path). Panic on violation of that invariant
            // rather than surfacing an internal race as a user error.
            self.registry
                .register_kind_with_descriptor(kind)
                .expect("pre-validated; no concurrent descriptor registrations");
        }

        let module = match Module::new(&self.engine, &payload.wasm) {
            Ok(m) => m,
            Err(e) => {
                return LoadResultPayload::Err {
                    error: format!("invalid wasm module: {e}"),
                };
            }
        };

        let name = payload.name.unwrap_or_else(|| {
            let n = self.default_name_counter.fetch_add(1, Ordering::Relaxed);
            format!("component_{n}")
        });

        let mailbox = match self.registry.try_register_component(&name) {
            Ok(id) => id,
            Err(e) => {
                return LoadResultPayload::Err {
                    error: e.to_string(),
                };
            }
        };

        let ctx = SubstrateCtx {
            sender: mailbox,
            registry: Arc::clone(&self.registry),
            queue: Arc::clone(&self.queue),
        };
        let component = match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
            Ok(c) => c,
            Err(e) => {
                // The mailbox and kinds are left in the registry. A
                // retry with a different name will get a fresh mailbox;
                // the kinds are idempotent and re-registering them is
                // a no-op. Rolling back the mailbox would need a
                // Registry API we don't have yet and is parked.
                return LoadResultPayload::Err {
                    error: format!("wasm instantiation failed: {e}"),
                };
            }
        };

        self.insert_component(mailbox, component);
        self.announce_kinds();

        LoadResultPayload::Ok {
            mailbox_id: mailbox.0,
            name,
        }
    }

    fn handle_drop(&self, bytes: &[u8]) -> DropResultPayload {
        let payload: DropComponentPayload = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return DropResultPayload::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };
        let id = MailboxId(payload.mailbox_id);
        if let Err(e) = self.registry.drop_mailbox(id) {
            return DropResultPayload::Err {
                error: e.to_string(),
            };
        }
        // Pull the Component out of the scheduler table, fire the
        // ADR-0015 `on_drop` hook on it, then let it drop at end of
        // scope so wasmtime reclaims linear memory. The mailbox was
        // already marked `Dropped` above, so any mail racing in
        // parallel will be discarded regardless of when the hook
        // runs.
        if let Some(mut component) = self.remove_component(id) {
            component.on_drop();
        }
        DropResultPayload::Ok
    }

    fn handle_replace(&self, bytes: &[u8]) -> ReplaceResultPayload {
        let payload: ReplaceComponentPayload = match postcard::from_bytes(bytes) {
            Ok(p) => p,
            Err(e) => {
                return ReplaceResultPayload::Err {
                    error: format!("postcard decode failed: {e}"),
                };
            }
        };
        let id = MailboxId(payload.mailbox_id);

        // Target must be a live Component. Reject unknown ids, sinks,
        // and already-dropped mailboxes before touching wasmtime.
        match self.registry.entry(id) {
            Some(crate::registry::MailboxEntry::Component) => {}
            Some(crate::registry::MailboxEntry::Sink(_)) => {
                return ReplaceResultPayload::Err {
                    error: format!("mailbox {:?} is a sink, not a component", id),
                };
            }
            Some(crate::registry::MailboxEntry::Dropped) => {
                return ReplaceResultPayload::Err {
                    error: format!("mailbox {:?} already dropped", id),
                };
            }
            None => {
                return ReplaceResultPayload::Err {
                    error: format!("unknown mailbox id {:?}", id),
                };
            }
        }

        // Kind descriptors: pre-validate like load_component.
        for kind in &payload.kinds {
            if let Some(kid) = self.registry.kind_id(&kind.name)
                && let Some(existing) = self.registry.kind_descriptor(kid)
                && existing.encoding != kind.encoding
            {
                return ReplaceResultPayload::Err {
                    error: format!(
                        "kind {:?} already registered with a different encoding",
                        kind.name
                    ),
                };
            }
        }
        for kind in payload.kinds {
            self.registry
                .register_kind_with_descriptor(kind)
                .expect("pre-validated; no concurrent descriptor registrations");
        }

        let module = match Module::new(&self.engine, &payload.wasm) {
            Ok(m) => m,
            Err(e) => {
                return ReplaceResultPayload::Err {
                    error: format!("invalid wasm module: {e}"),
                };
            }
        };

        // ADR-0015 §3: hooks run on the old instance under the write
        // lock before instantiation. Take the lock now, invoke hooks,
        // then keep the lock while we instantiate + swap so no mail
        // races in. Wart named in ADR-0015: if instantiation below
        // fails, `on_drop` will have already fired on the old
        // instance even though it stays live.
        let mut table = self.components.write().unwrap();
        if let Some(cell) = table.get(&id) {
            let mut old = cell.lock().unwrap();
            old.on_replace();
            old.on_drop();
        }

        let ctx = SubstrateCtx {
            sender: id,
            registry: Arc::clone(&self.registry),
            queue: Arc::clone(&self.queue),
        };
        let new_component = match Component::instantiate(&self.engine, &self.linker, &module, ctx) {
            Ok(c) => c,
            Err(e) => {
                // Registry is left as-is; any newly registered
                // kinds stay. The old component is still bound (hooks
                // already fired — see wart above).
                return ReplaceResultPayload::Err {
                    error: format!("wasm instantiation failed: {e}"),
                };
            }
        };

        // Atomic swap in-place under the already-held write lock.
        // The returned old Component drops at end of scope; wasmtime
        // reclaims linear memory + Store.
        let _old = table
            .remove(&id)
            .map(|m| m.into_inner().expect("component mutex poisoned"));
        table.insert(id, std::sync::Mutex::new(new_component));
        drop(table);

        self.announce_kinds();
        ReplaceResultPayload::Ok
    }

    fn insert_component(&self, id: MailboxId, component: Component) {
        self.components
            .write()
            .unwrap()
            .insert(id, std::sync::Mutex::new(component));
    }

    fn remove_component(&self, id: MailboxId) -> Option<Component> {
        self.components
            .write()
            .unwrap()
            .remove(&id)
            .map(|m| m.into_inner().expect("component mutex poisoned"))
    }

    /// Ship the complete current kind vocabulary to the hub so its
    /// per-engine descriptor cache (ADR-0007) reflects kinds that were
    /// registered at runtime (ADR-0010 §4). Called after a successful
    /// load or replace; drop doesn't affect the vocabulary.
    ///
    /// The substrate is authoritative on what it has registered, so we
    /// send the full list rather than a delta — simpler protocol, no
    /// ordering hazard, trivial on the wire (descriptors are small).
    /// If no hub is attached the outbound silently drops — harmless.
    fn announce_kinds(&self) {
        let kinds = self.registry.list_kind_descriptors();
        self.outbound.send(EngineToHub::KindsChanged(kinds));
    }

    fn reply<T: Serialize>(
        &self,
        sender: aether_hub_protocol::SessionToken,
        kind_name: &str,
        result: &T,
    ) {
        let payload = match postcard::to_allocvec(result) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("aether-substrate: {kind_name} encode failed: {e}");
                return;
            }
        };
        self.outbound.send(EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Session(sender),
            kind_name: kind_name.to_owned(),
            payload,
            origin: None,
        }));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aether_hub_protocol::{KindEncoding, SessionToken};

    #[test]
    fn load_payload_roundtrip() {
        let p = LoadComponentPayload {
            wasm: vec![0, 1, 2, 3],
            kinds: vec![KindDescriptor {
                name: "hello.foo".into(),
                encoding: KindEncoding::Signal,
            }],
            name: Some("hello".into()),
        };
        let bytes = postcard::to_allocvec(&p).unwrap();
        let back: LoadComponentPayload = postcard::from_bytes(&bytes).unwrap();
        assert_eq!(back.wasm, p.wasm);
        assert_eq!(back.name.as_deref(), Some("hello"));
        assert_eq!(back.kinds.len(), 1);
    }

    #[test]
    fn load_result_roundtrip() {
        for r in [
            LoadResultPayload::Ok {
                mailbox_id: 7,
                name: "x".into(),
            },
            LoadResultPayload::Err {
                error: "nope".into(),
            },
        ] {
            let bytes = postcard::to_allocvec(&r).unwrap();
            let _back: LoadResultPayload = postcard::from_bytes(&bytes).unwrap();
        }
    }

    /// Minimal WAT module satisfying the substrate's component
    /// contract: exports `memory`, a `receive(i32,i32,i32) -> i32`
    /// that returns 0, and no `init`.
    const WAT: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive") (param i32 i32 i32) (result i32)
                i32.const 0))
    "#;

    /// WAT with lifecycle hooks. Each hook writes a marker to a
    /// distinct offset in linear memory so tests can observe which
    /// hook fired. `on_replace` writes 0x11 at offset 200;
    /// `on_drop` writes 0x22 at offset 204.
    const WAT_HOOKS: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive") (param i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_replace") (result i32)
                i32.const 200
                i32.const 0x11
                i32.store
                i32.const 0)
            (func (export "on_drop") (result i32)
                i32.const 204
                i32.const 0x22
                i32.store
                i32.const 0))
    "#;

    /// WAT where `on_drop` traps via `unreachable`. Used to verify
    /// that a panicking hook does not stall teardown.
    const WAT_TRAPS_ON_DROP: &str = r#"
        (module
            (memory (export "memory") 1)
            (func (export "receive") (param i32 i32 i32) (result i32)
                i32.const 0)
            (func (export "on_drop") (result i32)
                unreachable))
    "#;

    fn make_plane() -> ControlPlane {
        let engine = Arc::new(Engine::default());
        let mut linker: Linker<SubstrateCtx> = Linker::new(&engine);
        crate::host_fns::register(&mut linker).expect("register host fns");
        let registry = Arc::new(Registry::new());
        let queue = Arc::new(MailQueue::new());
        let outbound = HubOutbound::disconnected();
        let components: ComponentTable = Arc::default();

        ControlPlane {
            engine,
            linker: Arc::new(linker),
            registry,
            queue,
            outbound,
            components,
            default_name_counter: Arc::new(AtomicU64::new(0)),
        }
    }

    #[test]
    fn load_component_instantiates_and_registers() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).expect("compile WAT");
        let payload = LoadComponentPayload {
            wasm,
            kinds: vec![KindDescriptor {
                name: "loaded.ping".into(),
                encoding: KindEncoding::Signal,
            }],
            name: Some("loaded".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        match result {
            LoadResultPayload::Ok { mailbox_id, name } => {
                assert_eq!(name, "loaded");
                assert!(plane.registry.kind_id("loaded.ping").is_some());
                assert_eq!(plane.registry.lookup("loaded"), Some(MailboxId(mailbox_id)));
                assert!(
                    plane
                        .components
                        .read()
                        .unwrap()
                        .contains_key(&MailboxId(mailbox_id))
                );
            }
            LoadResultPayload::Err { error } => panic!("load should succeed: {error}"),
        }
    }

    #[test]
    fn load_component_defaults_name_on_absent() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let payload = LoadComponentPayload {
            wasm,
            kinds: vec![],
            name: None,
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        match result {
            LoadResultPayload::Ok { name, .. } => {
                assert!(name.starts_with("component_"), "got {name:?}");
            }
            LoadResultPayload::Err { error } => panic!("load should succeed: {error}"),
        }
    }

    #[test]
    fn load_component_rejects_kind_conflict() {
        let plane = make_plane();
        // Pre-register "shared" as Opaque via the descriptor path.
        plane
            .registry
            .register_kind_with_descriptor(KindDescriptor {
                name: "shared".into(),
                encoding: KindEncoding::Opaque,
            })
            .unwrap();
        let wasm = wat::parse_str(WAT).unwrap();
        let payload = LoadComponentPayload {
            wasm,
            kinds: vec![KindDescriptor {
                name: "shared".into(),
                encoding: KindEncoding::Signal,
            }],
            name: Some("conflict_case".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        assert!(
            matches!(result, LoadResultPayload::Err { .. }),
            "expected conflict error, got {result:?}"
        );
        // Mailbox not allocated on conflict.
        assert!(plane.registry.lookup("conflict_case").is_none());
    }

    #[test]
    fn load_component_rejects_name_conflict() {
        let plane = make_plane();
        plane.registry.register_component("taken");
        let wasm = wat::parse_str(WAT).unwrap();
        let payload = LoadComponentPayload {
            wasm,
            kinds: vec![],
            name: Some("taken".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        assert!(matches!(result, LoadResultPayload::Err { .. }));
    }

    #[test]
    fn load_component_rejects_invalid_wasm() {
        let plane = make_plane();
        let payload = LoadComponentPayload {
            wasm: vec![0, 1, 2, 3],
            kinds: vec![],
            name: Some("bad_wasm".into()),
        };
        let result = plane.handle_load(&postcard::to_allocvec(&payload).unwrap());
        assert!(matches!(result, LoadResultPayload::Err { .. }));
    }

    #[test]
    fn drop_component_removes_component_and_frees_name() {
        let plane = make_plane();
        // Load first, then drop the same mailbox.
        let wasm = wat::parse_str(WAT).unwrap();
        let loaded = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm,
                kinds: vec![],
                name: Some("victim".into()),
            })
            .unwrap(),
        );
        let LoadResultPayload::Ok { mailbox_id, .. } = loaded else {
            panic!("load should succeed");
        };

        let dropped = plane
            .handle_drop(&postcard::to_allocvec(&DropComponentPayload { mailbox_id }).unwrap());
        assert!(matches!(dropped, DropResultPayload::Ok));
        assert!(
            plane.registry.lookup("victim").is_none(),
            "name should be released so it can be reused"
        );
        assert!(
            matches!(
                plane.registry.entry(MailboxId(mailbox_id)),
                Some(crate::registry::MailboxEntry::Dropped),
            ),
            "entry should be marked Dropped",
        );
        assert!(
            !plane
                .components
                .read()
                .unwrap()
                .contains_key(&MailboxId(mailbox_id)),
            "component must be removed from scheduler table",
        );
    }

    #[test]
    fn drop_component_rejects_unknown_id() {
        let plane = make_plane();
        let result = plane
            .handle_drop(&postcard::to_allocvec(&DropComponentPayload { mailbox_id: 99 }).unwrap());
        assert!(matches!(result, DropResultPayload::Err { .. }));
    }

    #[test]
    fn drop_component_rejects_double_drop() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResultPayload::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm,
                kinds: vec![],
                name: Some("once".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let args = postcard::to_allocvec(&DropComponentPayload { mailbox_id }).unwrap();
        assert!(matches!(plane.handle_drop(&args), DropResultPayload::Ok));
        assert!(matches!(
            plane.handle_drop(&args),
            DropResultPayload::Err { .. }
        ));
    }

    #[test]
    fn replace_component_swaps_instance_and_preserves_id() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResultPayload::Ok { mailbox_id, name } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm: wasm.clone(),
                kinds: vec![],
                name: Some("swap_target".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        assert_eq!(name, "swap_target");

        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponentPayload {
                mailbox_id,
                wasm,
                kinds: vec![],
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResultPayload::Ok));
        // Name still resolves to the same id; new Component bound.
        assert_eq!(
            plane.registry.lookup("swap_target"),
            Some(MailboxId(mailbox_id))
        );
        assert!(
            plane
                .components
                .read()
                .unwrap()
                .contains_key(&MailboxId(mailbox_id))
        );
    }

    #[test]
    fn replace_component_rejects_unknown_target() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponentPayload {
                mailbox_id: 99,
                wasm,
                kinds: vec![],
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResultPayload::Err { .. }));
    }

    #[test]
    fn replace_component_rejects_dropped_target() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResultPayload::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm: wasm.clone(),
                kinds: vec![],
                name: Some("gone".into()),
            })
            .unwrap(),
        ) else {
            panic!();
        };
        plane.handle_drop(&postcard::to_allocvec(&DropComponentPayload { mailbox_id }).unwrap());
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponentPayload {
                mailbox_id,
                wasm,
                kinds: vec![],
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResultPayload::Err { .. }));
    }

    #[test]
    fn replace_component_rejects_invalid_wasm() {
        let plane = make_plane();
        let wasm = wat::parse_str(WAT).unwrap();
        let LoadResultPayload::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm,
                kinds: vec![],
                name: Some("target".into()),
            })
            .unwrap(),
        ) else {
            panic!();
        };
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponentPayload {
                mailbox_id,
                wasm: vec![0, 1, 2, 3],
                kinds: vec![],
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResultPayload::Err { .. }));
    }

    #[test]
    fn drop_component_with_hooks_completes_ok() {
        // WAT_HOOKS exports on_drop. handle_drop should fire it and
        // complete without error; the marker write is exercised in
        // component::tests::on_drop_invokes_export_and_writes_marker.
        let plane = make_plane();
        let wasm = wat::parse_str(WAT_HOOKS).unwrap();
        let LoadResultPayload::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm,
                kinds: vec![],
                name: Some("hooked".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let dropped = plane
            .handle_drop(&postcard::to_allocvec(&DropComponentPayload { mailbox_id }).unwrap());
        assert!(matches!(dropped, DropResultPayload::Ok));
    }

    #[test]
    fn drop_component_with_trapping_on_drop_still_ok() {
        // ADR-0015 trap containment: a panicking hook must not stall
        // teardown. The handler logs and returns Ok regardless.
        let plane = make_plane();
        let wasm = wat::parse_str(WAT_TRAPS_ON_DROP).unwrap();
        let LoadResultPayload::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm,
                kinds: vec![],
                name: Some("crasher".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let dropped = plane
            .handle_drop(&postcard::to_allocvec(&DropComponentPayload { mailbox_id }).unwrap());
        assert!(matches!(dropped, DropResultPayload::Ok));
        // Mailbox still marked Dropped; component still removed.
        assert!(matches!(
            plane.registry.entry(MailboxId(mailbox_id)),
            Some(crate::registry::MailboxEntry::Dropped),
        ));
    }

    #[test]
    fn replace_component_fires_hooks_on_old_instance() {
        // handle_replace takes the write lock, fires on_replace +
        // on_drop on the old component, instantiates the new one,
        // and swaps under the same lock. Success means both hooks
        // completed without stalling the replace.
        let plane = make_plane();
        let wasm_old = wat::parse_str(WAT_HOOKS).unwrap();
        let LoadResultPayload::Ok { mailbox_id, .. } = plane.handle_load(
            &postcard::to_allocvec(&LoadComponentPayload {
                wasm: wasm_old,
                kinds: vec![],
                name: Some("swap_me".into()),
            })
            .unwrap(),
        ) else {
            panic!("load should succeed");
        };
        let wasm_new = wat::parse_str(WAT).unwrap();
        let result = plane.handle_replace(
            &postcard::to_allocvec(&ReplaceComponentPayload {
                mailbox_id,
                wasm: wasm_new,
                kinds: vec![],
            })
            .unwrap(),
        );
        assert!(matches!(result, ReplaceResultPayload::Ok));
    }

    #[test]
    fn dispatch_unrecognised_kind_is_silent_drop() {
        let plane = make_plane();
        // No panic; no outbound reply. Unknown kind arriving at the
        // control mailbox just logs and moves on.
        plane.dispatch("aether.control.does_not_exist", SessionToken::NIL, &[]);
    }
}
