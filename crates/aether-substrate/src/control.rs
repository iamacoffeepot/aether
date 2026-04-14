// Substrate-side control plane for ADR-0010. Reserved mailbox name:
// `aether.control`. Agents drive runtime component loading / dropping
// / replacement by mailing here; the substrate handles each reserved
// kind inline on the sink-handler thread and replies with a
// `aether.control.load_result` addressed at the originating session.
//
// Today's surface area: `aether.control.load_component` only. Drop
// and replace are wired in a subsequent PR; the sink handler already
// routes on kind name so adding them is strictly additive.
//
// Error discipline: agent-visible failures (bad postcard, kind
// conflict, name conflict, invalid WASM, wasmtime instantiation
// error) surface as a `LoadResult::Err` on the reply. Panics are
// reserved for invariant violations that the agent cannot have
// caused — e.g. a poisoned lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use aether_hub_protocol::{ClaudeAddress, EngineMailFrame, EngineToHub, KindDescriptor};
use aether_mail::Kind;
use aether_substrate_mail::{DropComponent, LoadComponent, LoadResult, ReplaceComponent};
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
            self.reply_load_result(sender, result);
        } else if kind_name == DropComponent::NAME || kind_name == ReplaceComponent::NAME {
            self.reply_load_result(
                sender,
                LoadResultPayload::Err {
                    error: format!("kind {kind_name:?} not yet implemented"),
                },
            );
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

        LoadResultPayload::Ok {
            mailbox_id: mailbox.0,
            name,
        }
    }

    fn insert_component(&self, id: MailboxId, component: Component) {
        self.components
            .write()
            .unwrap()
            .insert(id, std::sync::Mutex::new(component));
    }

    fn reply_load_result(
        &self,
        sender: aether_hub_protocol::SessionToken,
        result: LoadResultPayload,
    ) {
        let payload = match postcard::to_allocvec(&result) {
            Ok(b) => b,
            Err(e) => {
                eprintln!("aether-substrate: load_result encode failed: {e}");
                return;
            }
        };
        self.outbound.send(EngineToHub::Mail(EngineMailFrame {
            address: ClaudeAddress::Session(sender),
            kind_name: LoadResult::NAME.to_owned(),
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
    fn dispatch_rejects_unimplemented_control_kinds() {
        let plane = make_plane();
        // Capture send attempts via the outbound channel is awkward
        // without a live hub; exercise that dispatch at least doesn't
        // panic on the drop/replace paths.
        plane.dispatch(DropComponent::NAME, SessionToken::NIL, &[]);
        plane.dispatch(ReplaceComponent::NAME, SessionToken::NIL, &[]);
    }
}
