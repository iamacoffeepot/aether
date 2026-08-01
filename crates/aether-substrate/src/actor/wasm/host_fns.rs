// Host-function surface exposed to WASM components. Adding one is an
// explicit capability decision per ADR-0002 — every host function
// becomes reachable by every component that gets linked against this
// surface. Growth of this surface should be reviewed as deliberately
// as any other architectural change.

use core::str::from_utf8;

use aether_actor::{AssetCatalog, AssetWindow};
use aether_data::wire;
use wasmtime::{Caller, Linker};

use crate::actor::wasm::component::{ComponentCtx, PendingSpawn, StateBundle, TRAMPOLINE_NAMESPACE};
use crate::mail::registry::PreparedAliasRoute;
use crate::mail::{KindId, MailboxId, SourceAddr};
use crate::runtime::log_install;

/// Status codes returned by the `reply_mail` host fn (ADR-0013 §3).
/// `0` is success; non-zero values distinguish call-site errors
/// (unknown handle, OOB guest memory, unregistered kind) from each
/// other so the SDK can surface a useful message. "Session gone" is
/// a named status but not yet populated — V0 cannot synchronously
/// detect that the hub has dropped a session; the outbound frame is
/// queued and if the session is gone the hub discards it silently.
pub const REPLY_OK: u32 = 0;
pub const REPLY_UNKNOWN_HANDLE: u32 = 1;
pub const REPLY_SESSION_GONE: u32 = 2;
pub const REPLY_OOB: u32 = 3;
pub const REPLY_KIND_NOT_FOUND: u32 = 4;

/// ADR-0016 §2: maximum size of a single state bundle. A `save_state`
/// call with `len > MAX_STATE_BUNDLE_BYTES` is rejected (status 3) and
/// the failure is recorded on the ctx so the substrate can abort the
/// replace. 1 MiB is conservative and matches ADR-0006's `MAX_FRAME_SIZE`
/// — revisitable once a real component actually hits the cap.
pub const MAX_STATE_BUNDLE_BYTES: usize = 1 << 20;

/// Status codes returned by the `save_state` host fn. 0 is success —
/// non-zero values let the SDK distinguish component bugs (OOB, no
/// memory) from policy rejection (over the size cap).
pub const SAVE_STATE_OK: u32 = 0;
pub const SAVE_STATE_NO_MEMORY: u32 = 1;
pub const SAVE_STATE_OOB: u32 = 2;
pub const SAVE_STATE_TOO_LARGE: u32 = 3;

/// Register the substrate host functions on `linker`. Components that
/// want these capabilities must be instantiated via a linker that this
/// function has been called on.
//
// One linker.func_wrap block per host fn — extracting them into per-fn
// helpers would force per-fn Caller<'_, ComponentCtx> glue without
// saving readability; the v0 host-fn list is small and stable.
#[allow(clippy::too_many_lines)]
pub fn register(linker: &mut Linker<ComponentCtx>) -> wasmtime::Result<()> {
    linker.func_wrap(
        "aether",
        "send_mail_p32",
        |mut caller: Caller<'_, ComponentCtx>,
         recipient: u64,
         kind: u64,
         ptr: u32,
         len: u32,
         count: u32,
         detached: u32,
         from: u64|
         -> u32 {
            let Some(memory) = caller.get_export("memory").and_then(wasmtime::Extern::into_memory) else {
                return 1; // guest exports no memory
            };

            // Copy the bytes out of guest memory so the mail outlives
            // the current host-function call (queues, other threads).
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return 2, // out-of-bounds
            };
            let payload = data[start..end].to_vec();

            // ADR-0080 §7: the host stamps the in-flight dispatch
            // lineage onto the guest's send by default; `detached != 0`
            // (the guest's `send_detached`) opts out and starts a fresh
            // causal chain.
            let ctx = caller.data();
            // Issue 1987: the guest carried its own dispatch identity as
            // `from`; validate it is in-cluster (own id or a registered
            // inline-child alias) before trusting it as origin — a zero or
            // foreign value falls back to the component's own id, so a guest
            // cannot spoof a foreign origin.
            let identity = resolve_dispatch_identity(ctx, MailboxId(from));
            let recipient = MailboxId(recipient);
            let kind = KindId(kind);
            if detached == 0 {
                ctx.send(recipient, kind, payload, count, identity);
            } else {
                ctx.send_detached(recipient, kind, payload, count, identity);
            }
            0
        },
    )?;

    // HOST_FN_OK: ADR-0097 — sibling spawn is a synchronous host fn by
    // design. The mail-sink alternative (spawn-via-mail to
    // aether.component) was considered and rejected there: it makes the
    // call site async and loses the native `spawn_child` symmetry. The
    // host fn only *stages* the request (it can't name the
    // capabilities-layer WasmTrampoline); the trampoline performs the
    // spawn after `receive` returns.
    //
    // ADR-0097: stage a sibling-spawn request. The guest passes the
    // sibling's actor-type `tag`, an `is_counter` flag, the subname
    // (the full prefixed name for `Named`, the type-namespace prefix
    // for `Counter`), and the encoded `Config` bytes. This host fn
    // can't perform the spawn itself — `spawn_child::<WasmTrampoline>`
    // names a capabilities-layer type substrate can't see — so it
    // stages the request onto the ctx and returns the new instance's
    // `MailboxId` synchronously (`hash("{TRAMPOLINE_NAMESPACE}:{subname}")`,
    // ADR-0029). The trampoline drains the request after `receive`
    // returns and runs the real spawn (ADR-0097 §4). On any host-side
    // error (no memory, OOB, bad UTF-8, no spawner) it warn-logs and
    // returns 0 without staging — the sibling simply never appears.
    linker.func_wrap(
        "aether",
        "spawn_sibling_p32",
        |mut caller: Caller<'_, ComponentCtx>,
         tag: u64,
         is_counter: u32,
         subname_ptr: u32,
         subname_len: u32,
         config_ptr: u32,
         config_len: u32|
         -> u64 {
            // Copy subname + config out of guest memory, ending the
            // immutable borrow before the `data_mut` stage below.
            let copied = {
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(wasmtime::Extern::into_memory)
                else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_sibling: guest exports no memory");
                    return 0;
                };
                let data = memory.data(&caller);
                let read = |ptr: u32, len: u32| -> Option<&[u8]> {
                    let start = ptr as usize;
                    let end = start.checked_add(len as usize)?;
                    (end <= data.len()).then(|| &data[start..end])
                };
                let (Some(subname_bytes), Some(config_bytes)) =
                    (read(subname_ptr, subname_len), read(config_ptr, config_len))
                else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_sibling: subname/config pointer out of bounds");
                    return 0;
                };
                let Ok(subname) = from_utf8(subname_bytes) else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_sibling: subname is not valid UTF-8");
                    return 0;
                };
                (subname.to_owned(), config_bytes.to_vec())
            };
            let (subname_prefix, config) = copied;

            // `Counter`: the discriminator is the bare counter value — a
            // flat segment with no prefix, per the convention that `.`
            // appears only inside namespace atoms (ADR-0099 §4).
            let full_subname = if is_counter == 0 {
                subname_prefix
            } else {
                let Some(n) = caller
                    .data()
                    .binding
                    .as_ref()
                    .and_then(|binding| binding.spawner())
                    .map(|spawner| spawner.next_counter())
                else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_sibling: no spawner on the binding (counter subname unresolvable)");
                    return 0;
                };
                n.to_string()
            };

            // ADR-0099 §3: a spawned sibling nests under this trampoline,
            // so its id folds the sibling's instanced node onto the
            // trampoline's lineage carry — the same fold the drain-time
            // `spawn_child::<WasmTrampoline>` runs (it carries the
            // trampoline's binding carry), so the synchronous prediction
            // matches the registered id.
            let Some(trampoline_carry) =
                caller.data().binding.as_ref().map(|binding| binding.carry())
            else {
                tracing::warn!(target: "aether_substrate::component", "spawn_sibling: no binding on the trampoline (cannot fold sibling id)");
                return 0;
            };
            let sibling_node = aether_data::ActorId::instanced(TRAMPOLINE_NAMESPACE, &full_subname);
            let mailbox_id = aether_data::with_tag(
                aether_data::Tag::Mailbox,
                aether_data::fold_lineage(trampoline_carry, sibling_node),
            );
            caller.data_mut().pending_spawns.push(PendingSpawn {
                tag,
                subname: full_subname,
                config,
            });
            mailbox_id
        },
    )?;

    // HOST_FN_OK: ADR-0114 — inline-child spawn is a synchronous host fn,
    // like `spawn_sibling`. Unlike `spawn_sibling` (which stages a
    // detached spawn the trampoline drains after `receive`), the inline
    // child's state remains co-located in the parent's wasm instance. The
    // host folds the deterministic alias id and stages a logical route to
    // the parent; the trampoline drains it after the guest call and the
    // registry owner publishes it at handler flush (ADR-0165). The guest
    // runs the child's `init` in-process and dispatches it behind a membrane
    // keyed on the routed recipient (`aether-actor`'s `export!`). No config
    // crosses: the guest owns construction.
    //
    // ADR-0114: register an inline child's alias route. The guest passes
    // an `is_counter` flag and the bare subname (empty for `Counter`). The
    // alias id is `with_tag(Mailbox, fold_lineage(parent_carry,
    // instanced(aether.embedded, subname)))` — the same fold a detached
    // sibling renders post-#1920 (so the synchronous prediction matches a
    // `Call`-by-name resolution). On any host-side error (no memory, OOB,
    // bad UTF-8, no binding/spawner, or missing parent name) it warn-logs and
    // returns 0 without staging — the child simply never becomes addressable.
    linker.func_wrap(
        "aether",
        "spawn_inline_child_p32",
        |mut caller: Caller<'_, ComponentCtx>,
         is_counter: u32,
         subname_ptr: u32,
         subname_len: u32|
         -> u64 {
            // Copy the subname out of guest memory (empty for `Counter`),
            // ending the immutable memory borrow before the reads below.
            let subname_prefix = {
                let Some(memory) = caller
                    .get_export("memory")
                    .and_then(wasmtime::Extern::into_memory)
                else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_inline_child: guest exports no memory");
                    return 0;
                };
                let data = memory.data(&caller);
                let start = subname_ptr as usize;
                let Some(end) = start
                    .checked_add(subname_len as usize)
                    .filter(|e| *e <= data.len())
                else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_inline_child: subname pointer out of bounds");
                    return 0;
                };
                let Ok(subname) = from_utf8(&data[start..end]) else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_inline_child: subname is not valid UTF-8");
                    return 0;
                };
                subname.to_owned()
            };

            // `Counter`: the discriminator is the bare counter value — the
            // same source `spawn_sibling` draws from, so inline + detached
            // children never collide under one parent (ADR-0099 §4).
            let full_subname = if is_counter == 0 {
                subname_prefix
            } else {
                let Some(n) = caller
                    .data()
                    .binding
                    .as_ref()
                    .and_then(|binding| binding.spawner())
                    .map(|spawner| spawner.next_counter())
                else {
                    tracing::warn!(target: "aether_substrate::component", "spawn_inline_child: no spawner on the binding (counter subname unresolvable)");
                    return 0;
                };
                n.to_string()
            };

            let ctx = caller.data();
            // ADR-0099 §3: fold the alias id onto the parent trampoline's
            // lineage carry — identical to `spawn_sibling`'s fold, so the
            // id matches a written-name `Call` resolution.
            let Some(parent_carry) = ctx.binding.as_ref().map(|binding| binding.carry()) else {
                tracing::warn!(target: "aether_substrate::component", "spawn_inline_child: no binding on the trampoline (cannot fold alias id)");
                return 0;
            };
            let child_node = aether_data::ActorId::instanced(TRAMPOLINE_NAMESPACE, &full_subname);
            let alias_id = MailboxId(aether_data::with_tag(
                aether_data::Tag::Mailbox,
                aether_data::fold_lineage(parent_carry, child_node),
            ));

            // The parent may still be `Starting` while its `wire` hook runs,
            // so retain only its logical id + rendered name here. The owner
            // resolves the target lifecycle when the staged batch lands.
            let Some(parent_name) = ctx.registry.mailbox_name(ctx.sender) else {
                tracing::warn!(target: "aether_substrate::component", "spawn_inline_child: parent has no registered name (cannot render alias)");
                return 0;
            };
            let target_parent = ctx.sender;
            let alias_name = format!("{parent_name}/{TRAMPOLINE_NAMESPACE}:{full_subname}");
            caller
                .data_mut()
                .stage_alias(PreparedAliasRoute::new(alias_id, alias_name, target_parent));
            alias_id.0
        },
    )?;

    // HOST_FN_OK: the teardown half of `spawn_inline_child_p32` above. An
    // inline child is created and destroyed synchronously inside one guest
    // call (ADR-0114 §2) — it owns no resource a capability could hold, and
    // routing its teardown through mail would put the retirement an actor turn
    // behind the guest-side removal, which is the window this fixes. A
    // capability here would also have to be addressable from every component,
    // and would still need this host fn to learn the despawn happened.
    //
    // ADR-0114 teardown (#4228): retire an inline child's alias route when the
    // guest despawns the child. It stages the retirement the way the spawn half
    // stages a publication, and the trampoline drains it after the guest call,
    // retiring the route through the registry owner and firing the departure
    // notices the alias's watchers are owed.
    //
    // A guest can only retire an alias of its own — the same in-cluster
    // question `send_mail`'s origin stamping asks, so it takes the same
    // answer (`is_own_cluster_alias`: the prepared local fact, or the
    // owner-published relation). Anything that is not this component's alias
    // warn-logs and returns 0 without staging, so a malformed id cannot retire
    // a peer's child.
    linker.func_wrap(
        "aether",
        "despawn_inline_child_p32",
        |mut caller: Caller<'_, ComponentCtx>, alias: u64| -> u32 {
            let alias = MailboxId(alias);
            let ctx = caller.data();
            if !is_own_cluster_alias(ctx, alias) {
                tracing::warn!(
                    target: "aether_substrate::component",
                    %alias,
                    parent = %ctx.sender,
                    "despawn_inline_child: id is not this component's inline-child alias",
                );
                return 0;
            }
            caller.data_mut().stage_alias_retirement(alias);
            1
        },
    )?;

    // `resolve_kind_p32` was retired in ADR-0030 Phase 2: kind ids are
    // the `fnv1a_64(KIND_DOMAIN ++ canonical(name, schema))` hash,
    // computed on the
    // guest side via the `Kind` derive's `const ID`. The host fn and
    // its `KIND_NOT_FOUND` sentinel are gone. Input-stream auto-
    // subscribe (the side-effect that used to ride this host fn)
    // moved to the guest SDK — ADR-0033 phase 3 has `#[actor]`
    // prepend `ctx.subscribe_input::<K>()` for every `K::IS_INPUT`
    // handler kind to the user's `init` body.

    // ADR-0016 §2: save_state buffers the component's migration payload
    // into a substrate-owned slot on the store ctx. The guest passes a
    // `version` (opaque to the substrate) and a `(ptr, len)` pair
    // pointing at its own linear memory. Bytes are copied out so the
    // old instance can drop its memory normally; the substrate later
    // hands them to the new instance via `on_rehydrate`.
    //
    // Size cap is enforced before the guest memory is read — an
    // oversized request records an error and aborts without touching
    // memory. A subsequent `save_state` in the same `on_dehydrate` call
    // overwrites; this matches ADR-0016 §2's "zero or one times" clause
    // for the success path and doesn't change behavior on error.
    linker.func_wrap(
        "aether",
        "save_state_p32",
        |mut caller: Caller<'_, ComponentCtx>, version: u32, ptr: u32, len: u32| -> u32 {
            if len as usize > MAX_STATE_BUNDLE_BYTES {
                caller.data_mut().save_state_error =
                    Some(format!("save_state: bundle size {len} exceeds {MAX_STATE_BUNDLE_BYTES} byte cap"));
                return SAVE_STATE_TOO_LARGE;
            }
            let Some(memory) = caller.get_export("memory").and_then(wasmtime::Extern::into_memory) else {
                return SAVE_STATE_NO_MEMORY;
            };
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return SAVE_STATE_OOB,
            };
            let bytes = data[start..end].to_vec();
            let ctx = caller.data_mut();
            ctx.saved_state = Some(StateBundle { version, bytes });
            SAVE_STATE_OK
        },
    )?;

    // ADR-0013 + ADR-0017: `reply_mail` addresses the originator of
    // the inbound mail whose sender handle the guest received.
    // Branches on the `ReplyEntry` variant:
    //   - Session: ship as a `ClaudeAddress::Session` frame through
    //     `HubOutbound` (same route as ADR-0013's original design).
    //   - Component: enqueue on the local `Mailer` via
    //     `ComponentCtx::send`. Dropped-mailbox discard is handled
    //     there already, so a component that vanished between the
    //     request and the reply silently drops — the same contract
    //     as any other send to a dropped mailbox.
    linker.func_wrap(
        "aether",
        "reply_mail_p32",
        |mut caller: Caller<'_, ComponentCtx>,
         sender: u32,
         kind: u64,
         ptr: u32,
         len: u32,
         count: u32,
         from: u64|
         -> u32 {
            let Some(memory) = caller.get_export("memory").and_then(wasmtime::Extern::into_memory) else {
                return REPLY_OOB;
            };
            let data = memory.data(&caller);
            let start = ptr as usize;
            let end = match start.checked_add(len as usize) {
                Some(e) if e <= data.len() => e,
                _ => return REPLY_OOB,
            };
            let payload = data[start..end].to_vec();

            // A reply handle is one-shot: take (not resolve) so the
            // entry is removed here, capping the table at in-flight
            // replies rather than lifetime traffic. The mutable
            // borrow ends with this statement, before the `&self`
            // uses below.
            let Some(entry) = caller.data_mut().reply_table.take(sender) else {
                return REPLY_UNKNOWN_HANDLE;
            };
            let ctx = caller.data();
            // ADR-0042: echo the inbound correlation on every reply
            // path so the originating actor's handler can match its
            // own reply to the request it sent out of a busy inbox.
            let correlation = entry.correlation_id;
            let kind = KindId(kind);
            match entry.addr {
                SourceAddr::Session(token) => {
                    let Some(kind_name) = ctx.registry.kind_name(kind) else {
                        return REPLY_KIND_NOT_FOUND;
                    };
                    let origin = ctx.registry.mailbox_name(ctx.sender);
                    ctx.outbound.egress_to_session(token, &kind_name, payload, origin, correlation);
                }
                SourceAddr::Component(mbox) => {
                    // Validate the kind id cheaply — the guest might
                    // have passed a bogus one and we'd rather return
                    // a meaningful status than silently enqueue mail
                    // that the receiver can't decode.
                    if ctx.registry.kind_name(kind).is_none() {
                        return REPLY_KIND_NOT_FOUND;
                    }
                    // Issue iamacoffeepot/aether#1465: `reply` (not
                    // `send`) so the outgoing reply echoes the inbound
                    // `correlation` with target `None` — matching native
                    // `Mailer::send_reply` and the `Session` /
                    // `EngineMailbox` arms above. `send` would
                    // fresh-mint a `Component(self)` correlation,
                    // dropping the originator's id so the reply can't
                    // be matched home over the RPC `in_flight` table.
                    //
                    // Issue 1987: the reply's lineage identity is the
                    // guest-carried `from`, validated in-cluster (a zero /
                    // foreign value falls back to the component's own id).
                    let identity = resolve_dispatch_identity(ctx, MailboxId(from));
                    ctx.reply(mbox, kind, payload, count, correlation, identity);
                }
                SourceAddr::EngineMailbox { engine_id, mailbox_id } => {
                    // ADR-0037 Phase 2: reply to a component on
                    // another engine. Validate the kind exists
                    // locally so we surface a meaningful status
                    // rather than shipping a frame the receiver
                    // can't decode. The hub forwards the frame to
                    // the target engine's connection as
                    // `HubToEngine::MailById`.
                    if ctx.registry.kind_name(kind).is_none() {
                        return REPLY_KIND_NOT_FOUND;
                    }
                    ctx.outbound.egress_to_engine_mailbox(engine_id, mailbox_id, kind, payload, count, correlation);
                }
                SourceAddr::None => {
                    // Shouldn't happen — `ReplyEntry`s only get
                    // allocated for mail with a real reply target.
                    // Treat as unknown-handle to avoid silent drops.
                    return REPLY_UNKNOWN_HANDLE;
                }
            }
            REPLY_OK
        },
    )?;

    // `resolve_mailbox_p32` was retired in ADR-0029: mailbox ids are
    // now a deterministic hash of the mailbox name, computed on the
    // guest side. The corresponding host fn is gone.

    // ADR-0042: read back the correlation id the substrate minted
    // for this component's most recent `send_mail`. A guest handler
    // captures the id right after a send, then matches it against the
    // inbound reply's correlation to pick its own reply out of any
    // prior async-request replies that share the same kind. Returns
    // `0` (the `NO_CORRELATION` sentinel) before any send has been made.
    linker.func_wrap("aether", "prev_correlation_p32", |caller: Caller<'_, ComponentCtx>| -> u64 {
        caller.data().prev_correlation()
    })?;

    linker.func_wrap("aether", "reply_correlation_p32", |caller: Caller<'_, ComponentCtx>| -> u64 {
        caller.data().reply_correlation()
    })?;

    // HOST_FN_OK: ADR-0002 / issue 531. The ActorInitError plumbing
    // can't ride a mail sink because mail is not dispatched until
    // the component finishes booting — the `init` FFI call itself
    // is the entry point, and a `Result::Err` returned from it
    // needs a side channel to ship the error string back to the
    // substrate before the FFI call returns. A host fn is the
    // only mechanism that's available pre-`init`-completion.
    //
    // Issue 525 Phase 4b / issue 531: stage a `ActorInitError` message
    // for `Component::instantiate` to surface in `LoadResult::Err`
    // after the guest's `init` returns non-zero. The bytes are
    // copied out of guest memory before the call returns; OOB or
    // missing-memory drops silently — the guest's non-zero return
    // still triggers the failure path, just without a message
    // (`Component::instantiate` falls back to a generic "init
    // returned <rc> without staging an error" diagnostic).
    linker.func_wrap("aether", "init_failed_p32", |mut caller: Caller<'_, ComponentCtx>, ptr: u32, len: u32| {
        let Some(memory) = caller.get_export("memory").and_then(wasmtime::Extern::into_memory) else {
            return;
        };
        let data = memory.data(&caller);
        let start = ptr as usize;
        let end = match start.checked_add(len as usize) {
            Some(e) if e <= data.len() => e,
            _ => return,
        };
        let msg = String::from_utf8_lossy(&data[start..end]).into_owned();
        caller.data_mut().init_failure = Some(msg);
    })?;

    // ADR-0081 §7: `log_event_p32` re-fires a guest `tracing::*` event
    // on the host side. `ForwardingSubscriber::event` calls this (via the
    // installed log sink) per event
    // (no buffer, no flush hop — the pre-ADR-0081 `LogBatch` route
    // retired alongside `LogCapability`). The host re-emits via
    // `emit_host_event` on the trampoline's dispatcher thread, where
    // the `ActorAwareLayer` is already stamped against the
    // trampoline's `ActorSlots` and lands the entry in the
    // trampoline's `ActorLogRing`. Bytes are copied out of guest
    // memory before the call returns; OOB or missing-memory drops
    // silently.
    //
    // HOST_FN_OK: ADR-0081 §7 — log emission is intentionally a host
    // fn, not a mail sink. The mail surface is the *query* path
    // (`aether.log.tail` / `aether.log.engine`); emission lives on the
    // hot path of every guest `tracing::*` event and going through
    // mail would add an inbox round-trip per log line. The pre-
    // ADR-0081 batched `LogBatch` flush hop was the cost this ADR
    // retires.
    linker.func_wrap(
        "aether",
        "log_event_p32",
        |mut caller: Caller<'_, ComponentCtx>,
         level: u32,
         target_ptr: u32,
         target_len: u32,
         message_ptr: u32,
         message_len: u32| {
            let Some(memory) = caller.get_export("memory").and_then(wasmtime::Extern::into_memory) else {
                return;
            };
            let data = memory.data(&caller);
            let copy = |ptr: u32, len: u32| -> Option<String> {
                let start = ptr as usize;
                let end = start.checked_add(len as usize)?;
                if end > data.len() {
                    return None;
                }
                Some(String::from_utf8_lossy(&data[start..end]).into_owned())
            };
            let Some(target) = copy(target_ptr, target_len) else {
                return;
            };
            let Some(message) = copy(message_ptr, message_len) else {
                return;
            };
            log_install::emit_host_event(level, &target, &message);
        },
    )?;

    // HOST_FN_OK: ADR-0163 §3 asset load window (#3984). This cannot be a
    // native capability addressed by mail: `asset` is a synchronous read
    // inside the guest's own `init` / `wire`, and the bytes live host-side
    // in the module file — the guest must pull them across the FFI at that
    // instant and get them back into its own linear memory. That is the
    // same host-mediated byte-transport shape as config delivery into
    // `init` (ADR-0090) and mail delivery into `receive`, none of which a
    // mail-round-trip capability can express. The surface is window-scoped
    // (traps after `wire`), so it grows no persistent capability.
    //
    // Pull one asset's bytes through the load window: the guest passes the
    // asset name (a slice in guest memory), the host looks it up in the
    // `LoadWindow` installed on the ctx, allocates a buffer in guest memory
    // through the guest's own `realloc_p32`, writes the bytes, and returns
    // the ADR-0163 packed `(ptr << 32) | len`. Encoding:
    //   - `ASSET_NOT_FOUND` (`u64::MAX`) — the window is open but carries
    //     no asset by that name; the guest maps it to `None`.
    //   - any other value — `(ptr << 32) | len`, a live guest buffer the
    //     SDK copies out and frees.
    // A call after the window closed (post-`wire`) or with no window at all
    // traps, so the type-fence (`asset` lives only on the init/wire ctx) is
    // backed by a loud runtime failure for a hand-rolled guest, never a
    // silent empty. Not-found vs closed is thus a returned sentinel vs a
    // trap — two unambiguous outcomes.
    linker.func_wrap(
        "aether",
        "asset_fetch_p32",
        |mut caller: Caller<'_, ComponentCtx>, name_ptr: u32, name_len: u32| -> wasmtime::Result<u64> {
            let name = read_guest_utf8(&mut caller, name_ptr, name_len)?;
            let bytes = {
                let ctx = caller.data_mut();
                let Some(window) = ctx.load_window.as_mut() else {
                    return Err(wasmtime::Error::msg("asset_fetch: this component has no asset load window"));
                };
                if !window.is_open() {
                    return Err(wasmtime::Error::msg(
                        "asset_fetch: called outside the load window — asset payload access ends when `wire` \
                         returns (ADR-0163 §3)",
                    ));
                }
                window.asset(&name)
            };
            bytes.map_or_else(|| Ok(ASSET_NOT_FOUND), |bytes| deliver_bytes_to_guest(&mut caller, &bytes))
        },
    )?;

    // HOST_FN_OK: ADR-0163 §3 (#3984) — the catalog companion of the
    // asset_fetch pull above, backing the guest's `AssetCatalog::assets()`
    // (the `AssetWindow: AssetCatalog` supertrait). Same host-mediated
    // byte-transport rationale; a mail capability cannot serve a
    // synchronous in-`wire` read into guest memory. Returns the window's
    // catalog as a wire-encoded `Vec<AssetInfo>` delivered like
    // `asset_fetch_p32`; an empty catalog encodes to a valid empty
    // sequence (no sentinel). Unlike `asset_fetch`, this reads the catalog
    // metadata retained past `close()`, so it answers for the instance's
    // life; it traps only when no window was ever installed.
    linker.func_wrap(
        "aether",
        "asset_catalog_p32",
        |mut caller: Caller<'_, ComponentCtx>| -> wasmtime::Result<u64> {
            let bytes = {
                let ctx = caller.data();
                let Some(window) = ctx.load_window.as_ref() else {
                    return Err(wasmtime::Error::msg("asset_catalog: this component has no asset load window"));
                };
                wire::to_vec(window.assets())
                    .map_err(|e| wasmtime::Error::msg(format!("asset_catalog: encode failed: {e}")))?
            };
            deliver_bytes_to_guest(&mut caller, &bytes)
        },
    )?;

    Ok(())
}

/// ADR-0163 packed-return marker for "the load window is open but carries
/// no asset by the requested name" — distinct from a real `(ptr << 32) |
/// len` (a 4 GiB asset at pointer `0xFFFF_FFFF` is impossible under the
/// frame and address bounds). The guest maps it to `None`.
const ASSET_NOT_FOUND: u64 = u64::MAX;

/// Alignment the asset delivery buffer is allocated with. Byte payloads
/// need no alignment, so `1` keeps the guest's free (`realloc_bytes(ptr,
/// len, 1, 0)`) layout-exact. The guest's instantiate-time small region
/// has already claimed the reused scratch, so this allocation always falls
/// through to the global allocator — never the scratch — so the guest can
/// free it.
const ASSET_ALLOC_ALIGN: u32 = 1;

/// Read a UTF-8 string from `(ptr, len)` in the caller's guest memory.
/// Traps on no-memory, out-of-bounds, or invalid UTF-8 — a malformed asset
/// name from a hand-rolled guest fails loud rather than resolving to a
/// wrong asset.
fn read_guest_utf8(caller: &mut Caller<'_, ComponentCtx>, ptr: u32, len: u32) -> wasmtime::Result<String> {
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest exports no memory"))?;
    let data = memory.data(&caller);
    let start = ptr as usize;
    let end = start
        .checked_add(len as usize)
        .filter(|end| *end <= data.len())
        .ok_or_else(|| wasmtime::Error::msg("asset name pointer out of bounds"))?;
    from_utf8(&data[start..end]).map(str::to_owned).map_err(|_| wasmtime::Error::msg("asset name is not valid UTF-8"))
}

/// Allocate `bytes.len()` bytes in guest memory through the guest's
/// `realloc_p32` export, write `bytes` there, and return the ADR-0163
/// packed `(ptr << 32) | len`. The SDK copies the buffer out and frees it
/// via the same guest allocator (`realloc_bytes(ptr, len,
/// ASSET_ALLOC_ALIGN, 0)`), so host and guest agree on the layout. Traps
/// when the guest exports no allocator or memory, or the allocator returns
/// null — a non-conforming guest, consistent with the config-delivery
/// path. A grow may relocate linear memory, so `memory` is re-fetched
/// after the allocator call.
fn deliver_bytes_to_guest(caller: &mut Caller<'_, ComponentCtx>, bytes: &[u8]) -> wasmtime::Result<u64> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| wasmtime::Error::msg("asset payload exceeds the 4 GiB guest-address bound"))?;
    let realloc_export = caller.get_export("realloc_p32").and_then(wasmtime::Extern::into_func);
    let Some(realloc) = realloc_export else {
        return Err(wasmtime::Error::msg("guest exports no realloc_p32 allocator; cannot deliver asset bytes"));
    };
    let realloc = realloc.typed::<(u32, u32, u32, u32), u32>(&caller)?;
    let ptr = realloc.call(&mut *caller, (0, 0, ASSET_ALLOC_ALIGN, len))?;
    if ptr == 0 {
        return Err(wasmtime::Error::msg("guest allocator returned null for the asset buffer"));
    }
    let memory = caller
        .get_export("memory")
        .and_then(wasmtime::Extern::into_memory)
        .ok_or_else(|| wasmtime::Error::msg("guest exports no memory"))?;
    memory.write(&mut *caller, ptr as usize, bytes)?;
    Ok((u64::from(ptr) << 32) | u64::from(len))
}

/// Issue 1987: resolve the dispatch identity a guest claimed on a send /
/// reply (`from`) to a value the host trusts. A guest may claim only an
/// origin inside its own cluster — the component's own id (`ctx.sender`) or
/// one of its registered inline-child aliases (`is_own_cluster_alias`). A
/// zero (`MailboxId::NONE`) or foreign `from` falls back to the component's
/// own id, so the host stays authoritative on cross-cluster origin and a
/// guest cannot spoof a foreign id. This is the in-cluster check that the
/// retired `set_dispatch_source_p32` host fn used to gate the ambient cell.
fn resolve_dispatch_identity(ctx: &ComponentCtx, from: MailboxId) -> MailboxId {
    if from != MailboxId::NONE && (from == ctx.sender || is_own_cluster_alias(ctx, from)) {
        from
    } else {
        ctx.sender
    }
}

/// Whether `candidate` is an inline-child alias of *this* component
/// (ADR-0114, ADR-0165). A just-created child may send during its immediate
/// guest-side `init` / `wire`, before the handler flush publishes its alias,
/// so the host trusts either the local prepared fact or the owner-published
/// logical relation. Neither path clones nor compares an endpoint.
pub(super) fn is_own_cluster_alias(ctx: &ComponentCtx, candidate: MailboxId) -> bool {
    ctx.has_pending_alias(candidate) || ctx.registry.is_alias_to(candidate, ctx.sender)
}
