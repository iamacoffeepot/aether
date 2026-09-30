//! The boot loader: publishes each boot component's module, spawns its
//! instance keys, and reports each entry's answer to the chassis thread
//! that spawned it (issues #6413, #6637, #7155).
//!
//! `load_boot_components` spawns [`Autoloader`] at the chassis root after the
//! build and waits on the channel whose sending half rides in
//! [`AutoloaderParams`], one answer per entry, each within the boot-load
//! budget. For each entry the loader sends one `Publish` of its wasm, then
//! one `Spawn` per instance key in order, one in flight at a time; it
//! reports the entry `Ok` only after its publish and every one of its
//! spawns has answered. The loader holds nothing else: the chassis thread
//! keeps the entries' labels and the RPC bind gate, names any failure or
//! timeout, and opens the gate only after every entry has answered `Ok`.

use std::collections::VecDeque;
use std::sync::mpsc::Sender;

use aether_actor::{DependsOn, actor};
use aether_component::ComponentHostCapability;
use aether_kinds::{Publish, PublishResult, PublishedType, Spawn, SpawnResult};
use aether_substrate::actor::native::{NativeActor, NativeCtx, NativeInitCtx};
use aether_substrate::actor::wasm::kind_manifest;
use aether_substrate::chassis::error::BootError;

use super::AutoloadComponent;

/// One entry's answer: `Ok` when its publish and every spawn answered, or
/// the first error naming why it stopped.
pub type LoadAnswer = Result<(), String>;

/// Composer-supplied construction input: the boot components, in load
/// order, and the channel each entry's answer is reported on.
pub struct AutoloaderParams {
    /// The components to load, in manifest order.
    pub components: Vec<AutoloadComponent>,
    /// The sending half the chassis thread waits on.
    pub report: Sender<LoadAnswer>,
}

/// One entry's publish resolving into its spawns: the namespace it spawns
/// from (`None` until the publish reply resolves it) and the instance keys
/// not yet spawned, in order, plus the config every one of them spawns
/// with.
struct CurrentEntry {
    namespace: Option<String>,
    config: Vec<u8>,
    keys: VecDeque<Option<String>>,
}

/// Loads the boot components sequentially: one entry in flight, its publish
/// then its spawns one key at a time, the next entry sent on the previous
/// entry's last `Ok`, and nothing sent after the first `Err`.
pub struct Autoloader {
    /// The components not yet sent, in load order.
    remaining: VecDeque<AutoloadComponent>,
    /// The entry currently publishing or spawning.
    current: Option<CurrentEntry>,
    /// The channel each entry's answer is reported on.
    report: Sender<LoadAnswer>,
}

#[actor(instanced, root, depends(ComponentHostCapability))]
impl NativeActor for Autoloader {
    type Config = ();
    type Params = AutoloaderParams;
    const NAMESPACE: &'static str = "aether.chassis.autoload";

    fn init((): (), params: AutoloaderParams, _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        let AutoloaderParams { components, report } = params;
        Ok(Self { remaining: components.into(), current: None, report })
    }

    fn wire(&mut self, ctx: &mut NativeCtx<'_>) {
        self.send_next(ctx);
    }

    /// The current entry's module published: resolve which of its bound
    /// types to spawn from, then send its first `Spawn`, or fail the entry
    /// naming a refusal or an unresolvable namespace.
    #[handler::single]
    fn on_publish_result(&mut self, ctx: &mut NativeCtx<'_>, result: PublishResult) {
        match result {
            PublishResult::Ok { types } => match self.resolve_namespace(&types) {
                Ok(namespace) => {
                    self.current.as_mut().expect("a publish result answers the entry that sent it").namespace =
                        Some(namespace);
                    self.send_next_spawn(ctx);
                }
                Err(error) => self.fail_current(error),
            },
            PublishResult::Err { error } => self.fail_current(format!("publish: {error}")),
        }
    }

    /// The current entry's spawn answered: continue to its next key, finish
    /// the entry once every key has spawned, or fail it naming the refusal
    /// (a `Live` answer means two boot entries name one instance).
    #[handler::single]
    fn on_spawn_result(&mut self, ctx: &mut NativeCtx<'_>, result: SpawnResult) {
        match result {
            SpawnResult::Spawned { .. } => self.send_next_spawn(ctx),
            SpawnResult::Live { path, .. } => {
                self.fail_current(format!("{path} is already live: two boot entries name one instance"));
            }
            SpawnResult::Err { error } => self.fail_current(format!("spawn: {error}")),
        }
    }
}

impl Autoloader {
    /// Send the next component's publish, if any remain.
    fn send_next<A: DependsOn<ComponentHostCapability>>(&mut self, ctx: &mut NativeCtx<'_, A>) {
        let Some(component) = self.remaining.pop_front() else {
            return;
        };
        let AutoloadComponent { wasm, config, namespace, keys } = component;
        // An entry that names no type spawns the module's default, the type
        // its `aether.namespace` section names. A section the read refuses is
        // left to the publish, which parses the same section and refuses the
        // module naming why.
        let namespace = namespace.or_else(|| kind_manifest::read_namespace_from_bytes(&wasm).ok().flatten());
        self.current = Some(CurrentEntry { namespace, config, keys: keys.into() });
        ctx.send::<ComponentHostCapability>(&Publish { code: wasm.into(), configs: Vec::new() });
    }

    /// Send the current entry's next key's `Spawn`, or, once every key has
    /// spawned, report it `Ok` and advance to the next entry.
    fn send_next_spawn<A: DependsOn<ComponentHostCapability>>(&mut self, ctx: &mut NativeCtx<'_, A>) {
        let current = self.current.as_mut().expect("a spawn is sent only while an entry is current");
        let Some(key) = current.keys.pop_front() else {
            self.current = None;
            let _ = self.report.send(Ok(()));
            self.send_next(ctx);
            return;
        };
        let namespace = current.namespace.clone().expect("the namespace resolves before any spawn is sent");
        let config = current.config.clone();
        ctx.send::<ComponentHostCapability>(&Spawn { namespace, key, parent: None, config });
    }

    /// Report the current entry `Err`, and stop: nothing after it in
    /// `remaining` is sent (issue #6413's boot-order guarantee: a later
    /// entry never comes up out of order behind a failed one).
    fn fail_current(&mut self, error: String) {
        self.current = None;
        self.remaining.clear();
        let _ = self.report.send(Err(error));
    }

    /// Which of a `Publish`'s bound `types` the current entry spawns: the one
    /// its namespace names, bound as that name or as its `NS.<64 hex>`
    /// content-addressed publication (ADR-0241 §3); or, when the entry names
    /// none and the module declares no default, the sole type it binds.
    fn resolve_namespace(&self, types: &[PublishedType]) -> Result<String, String> {
        let current = self.current.as_ref().expect("a publish result answers the entry that sent it");
        current.namespace.as_deref().map_or_else(
            || match types {
                [one] => Ok(one.namespace.clone()),
                _ => Err(format!(
                    "the entry names no export and the module declares no default, but it binds several types: {:?}",
                    bound_names(types)
                )),
            },
            |declared| {
                published_as(types, declared).ok_or_else(|| {
                    format!("published module does not bind {declared:?}; it binds {:?}", bound_names(types))
                })
            },
        )
    }
}

/// The namespaces a `Publish` bound, for a failure to name.
fn bound_names(types: &[PublishedType]) -> Vec<&str> {
    types.iter().map(|published| published.namespace.as_str()).collect()
}

/// The `types` entry bound as `declared`: its published name matches
/// exactly, or is `declared` plus a `.<64-lowercase-hex>` content-address
/// suffix (ADR-0241 §3).
fn published_as(types: &[PublishedType], declared: &str) -> Option<String> {
    types
        .iter()
        .find(|published| published.namespace == declared || is_content_addressed_alias(&published.namespace, declared))
        .map(|published| published.namespace.clone())
}

/// Whether `published` is `declared` republished under its content-address
/// suffix: `declared` plus a `.` plus exactly 64 lowercase hex digits.
fn is_content_addressed_alias(published: &str, declared: &str) -> bool {
    published.strip_prefix(declared).and_then(|rest| rest.strip_prefix('.')).is_some_and(|hex| {
        hex.len() == 64 && hex.bytes().all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    })
}
