use aether_data::MailboxId;
use wasmtime::{Engine, Linker, Memory, Module, Store, TypedFunc};

use super::{ComponentCtx, DELIVERY_ALIGN, MAX_DELIVERABLE_MAIL_BYTES, ReallocFunc, ReceiveFunc, SMALL_REGION_BYTES};

pub struct Component {
    pub(super) store: Store<ComponentCtx>,
    pub(super) memory: Memory,
    pub(super) receive: ReceiveFunc,
    /// Issue 584 Phase 2b: post-init mail-allowed hook. Stored (rather
    /// than called inside [`Self::instantiate`]) so the trampoline
    /// can fire it AFTER its mailbox is registered — issue 640
    /// Phase 2 surfaced a race where `wire`-time `subscribe_input`
    /// mail was rejected by the input cap's
    /// `validate_subscriber_mailbox` because the trampoline mailbox
    /// hadn't been registered yet (init runs in
    /// `spawn_actor` step 4, registration is step 5–7).
    /// `WasmTrampoline::wire` invokes [`Self::wire`] post-registration.
    pub(super) wire: Option<TypedFunc<u64, u32>>,
    /// Issue 584 Phase 2b: pre-shutdown mail-allowed hook. Called by
    /// the trampoline (via [`Self::unwire`]) before `on_dehydrate` on
    /// the dying instance, or before the `Component` value drops on a
    /// `DropComponent`.
    pub(super) unwire: Option<TypedFunc<u64, u32>>,
    pub(super) on_dehydrate: Option<TypedFunc<(), u32>>,
    pub(super) on_rehydrate: Option<TypedFunc<(u32, u32, u32), u32>>,
    /// ADR-0095: the guest's generic delivery allocator
    /// (`realloc_p32`, `cabi_realloc`-shaped). Every payload — mail, config,
    /// state — is written into a region obtained from it. `None` for a
    /// non-conforming guest that exports no allocator; such a guest can't
    /// receive any payload (delivery drops with a loud log).
    pub(super) realloc: Option<ReallocFunc>,
    /// SMALL delivery region: `SMALL_REGION_BYTES` bytes, allocated once at
    /// instantiate from [`Self::realloc`] and cached. Non-null when an
    /// allocator is present; `0` for a no-allocator guest. A payload that fits
    /// is written here directly with no per-payload allocator call.
    pub(super) small_ptr: u32,
    /// LARGE delivery region: grown on demand to the largest over-floor payload
    /// (`large_cap` bytes) and reused. `0` until the first such payload. The
    /// pointer is re-fetched from each grow, since a grow may relocate it.
    pub(super) large_ptr: u32,
    /// Current capacity (bytes) of the LARGE region; `0` until first grown.
    pub(super) large_cap: u32,
    /// Mailbox id stamped at instantiate-time, replayed into `wire`
    /// and `unwire` calls. Same value the guest's `init` shim received.
    pub(super) self_mailbox_id: u64,
}

/// The two channels an init call delivers (ADR-0090 config, ADR-0170 params),
/// carried as one value because they occupy one contiguous delivery region:
/// `[config][params]` at the pointer the init shim receives. Either half may
/// be empty.
#[derive(Clone, Copy)]
pub(super) struct InitPayload<'a> {
    pub config: &'a [u8],
    pub params: &'a [u8],
}

/// Result of choosing a destination region for a host→guest payload
/// ([`Component::place`]).
pub(super) enum Placement {
    /// Write the payload at this guest pointer, then call the entry point. The
    /// pointer is non-null and `DELIVERY_ALIGN`-aligned, so a zero-length
    /// payload still yields a valid pointer for the guest's slice construction.
    At(u32),
    /// Payload exceeds [`MAX_DELIVERABLE_MAIL_BYTES`]; the caller drops (mail)
    /// or rejects (config / state).
    Oversize,
    /// Guest exports no `realloc_p32` allocator (non-conforming guest); nothing
    /// can be delivered into it.
    NoAllocator,
}

impl Component {
    /// ADR-0095: choose the destination region for a host→guest payload of
    /// `len` bytes, growing the large region through the guest allocator when
    /// needed. A payload that fits `SMALL_REGION_BYTES` lands in the cached small
    /// region with no allocator call; a larger one grows the reused large
    /// region (re-fetching its pointer, since a grow may relocate it); one past
    /// the ceiling is [`Placement::Oversize`]. Takes the fields explicitly
    /// rather than `&mut self` so [`Self::instantiate`] can call it before the
    /// `Component` value exists.
    pub(super) fn place(
        store: &mut Store<ComponentCtx>,
        realloc: Option<&ReallocFunc>,
        small_ptr: u32,
        large_ptr: &mut u32,
        large_cap: &mut u32,
        len: usize,
    ) -> wasmtime::Result<Placement> {
        let Some(realloc) = realloc else {
            return Ok(Placement::NoAllocator);
        };
        if len <= SMALL_REGION_BYTES {
            return Ok(Placement::At(small_ptr));
        }
        if len > MAX_DELIVERABLE_MAIL_BYTES {
            return Ok(Placement::Oversize);
        }
        // Wasm32 carries u32 byte lengths; `len <= MAX_DELIVERABLE_MAIL_BYTES`
        // (64 MiB) keeps the cast lossless.
        #[allow(clippy::cast_possible_truncation)]
        let new_cap = len as u32;
        if *large_cap < new_cap {
            let ptr = realloc.call(store, (*large_ptr, *large_cap, DELIVERY_ALIGN, new_cap))?;
            if ptr == 0 {
                return Err(wasmtime::Error::msg(format!(
                    "guest allocator returned null growing the delivery buffer to {new_cap} bytes"
                )));
            }
            *large_ptr = ptr;
            *large_cap = new_cap;
        }
        Ok(Placement::At(*large_ptr))
    }

    /// Place + write the init payload into a delivery region (ADR-0095) and
    /// return the guest pointer the init shim receives. Mirrors
    /// [`Self::deliver`]'s routing; a payload past the ceiling, or to a guest
    /// with no allocator, is a clean boot `Err` (surfaces as `LoadResult::Err`)
    /// rather than a write or trap. Factored out of [`Self::instantiate`].
    ///
    /// ADR-0170: the config bytes and the params bag ride **one** region,
    /// written back to back — `[config][params]` — so both channels cost one
    /// placement and the params-bearing init export adds only a length
    /// argument. `place` manages a small and a large region, so a second
    /// independent placement would have to overwrite one of them; splitting a
    /// single region by length avoids that without a third.
    fn place_init_payload(
        store: &mut Store<ComponentCtx>,
        memory: &Memory,
        realloc: Option<&ReallocFunc>,
        small_ptr: u32,
        large_ptr: &mut u32,
        large_cap: &mut u32,
        payload: InitPayload<'_>,
    ) -> wasmtime::Result<u32> {
        let InitPayload { config, params } = payload;
        let total = config.len() + params.len();
        match Self::place(store, realloc, small_ptr, large_ptr, large_cap, total)? {
            Placement::At(ptr) => {
                if !config.is_empty() {
                    memory.write(&mut *store, ptr as usize, config)?;
                }
                if !params.is_empty() {
                    memory.write(&mut *store, ptr as usize + config.len(), params)?;
                }
                Ok(ptr)
            }
            Placement::Oversize => {
                Self::log_oversize_config(store, total, "exceeds the absolute deliverable bound");
                Err(wasmtime::Error::msg(format!(
                    "guest init payload of {total} bytes exceeds the \
                     {MAX_DELIVERABLE_MAIL_BYTES}-byte deliverable bound",
                )))
            }
            Placement::NoAllocator => {
                Self::log_oversize_config(store, total, "guest exports no realloc_p32 allocator (raw-FFI guest)");
                Err(wasmtime::Error::msg(format!(
                    "guest init payload of {total} bytes cannot be delivered: guest exports no realloc_p32 allocator",
                )))
            }
        }
    }

    /// Instantiate a component from a compiled `Module`. `ctx` becomes
    /// the store data and is what every host function call against this
    /// component will see.
    ///
    /// ADR-0090 / ADR-0170: `config_bytes` is the wire-encoded
    /// `<WasmActor::Config as Kind>` payload and `params_bytes` is the
    /// kind-tagged injection bag threaded through the guest's widest init
    /// shim. Pass `&[]` for an absent channel; legacy init shapes consume
    /// neither params nor non-default config.
    ///
    /// ADR-0095: the combined init-payload write routes through `place`, the same
    /// allocator-backed two-region path [`Component::deliver`] uses for mail. A
    /// payload that fits the small region lands there; a larger one (up to the
    /// `MAX_DELIVERABLE_MAIL_BYTES` ceiling) grows the large region; a payload
    /// past that ceiling, or to a guest with no allocator export, is a clean
    /// boot error (`LoadResult::Err`) — never a write or trap. Whichever pointer
    /// it landed at is what the selected init export receives.
    ///
    /// ADR-0096: `type_tag` selects which exported actor type a
    /// multi-actor module instantiates. `Some(tag)` calls the guest's
    /// typed init export — a missing export is a clean boot error. `None` is
    /// the entry-type / single-actor path.
    ///
    /// ADR-0170: `params_bytes` is the wire-encoded `Vec<ParamEntry>` bag the
    /// component host assembled from its provider registry, empty when the
    /// actor requested nothing. Each path probes the widest parent-and-params
    /// export first, then the parent-aware config-only export, then older
    /// config-only shapes. A guest built before ADR-0170 therefore keeps
    /// loading, while a non-empty bag against one is a clean boot error rather
    /// than a silent drop.
    #[allow(
        clippy::too_many_lines,
        reason = "one cohesive instantiate sequence: build instance, probe the \
                  delivery allocator, select + run the init shim, look up the \
                  optional lifecycle exports. Splitting it would thread a dozen \
                  store/region locals through a helper for no clarity gain."
    )]
    pub fn instantiate(
        engine: &Engine,
        linker: &Linker<ComponentCtx>,
        module: &Module,
        ctx: ComponentCtx,
        config_bytes: &[u8],
        params_bytes: &[u8],
        type_tag: Option<u64>,
    ) -> wasmtime::Result<Self> {
        let mut store = Store::new(engine, ctx);
        let instance = linker.instantiate(&mut store, module)?;
        let memory = instance
            .get_memory(&mut store, "memory")
            .ok_or_else(|| wasmtime::Error::msg("guest exports no `memory`"))?;
        let receive = instance.get_typed_func::<(u64, u32, u32, u32, u32, u64, u64), u32>(&mut store, "receive_p32")?;

        // Optional `init(mailbox_id) -> u32` export: called once before
        // the first `receive`, handed the component's own mailbox id so
        // the SDK's typelist walker can auto-subscribe input kinds
        // (ADR-0030 Phase 2). Falls back to the legacy `init()` shape
        // so raw-FFI components predating the Phase 2 ABI still load —
        // they just don't get auto-subscribe, which they never did.
        //
        // ADR-0090 / ADR-0166 / ADR-0170: probe widest first, then narrow from
        // parent + config + params to parent + config, config only, `(u64)`,
        // and legacy `()`. Macro-built guests export the compatibility shims,
        // so probing a narrower shape first would silently discard metadata.
        //
        // Issue 525 Phase 4b / issue 531: a non-zero return value
        // means the guest's `WasmActor::init` returned `Err(ActorInitError)`
        // and staged the message via `init_failed_p32`. Drain the
        // staged string off the ctx and surface it as a wasmtime
        // error so the existing `dispatch_load_component` failure
        // path reports it via `LoadResult::Err { error }` — same
        // shape as a wasm trap, just with a more informative message.
        let mailbox_id = store.data().sender.0;
        // The component store is wired to the hosting trampoline's native
        // binding before instantiate. That binding owns the logical parent;
        // raw test/legacy contexts without one make the limitation explicit
        // as NONE and continue through the compatibility exports below.
        let parent_mailbox_id =
            store.data().binding.as_ref().map_or(MailboxId::NONE.0, |binding| binding.parent_mailbox().0);
        // ADR-0095: the guest's generic delivery allocator. Probed before the
        // config write because config delivery routes through it, exactly like
        // `deliver` routes mail. Present on macro-built guests (emitted by
        // `export!`); absent on a non-conforming guest, which then can't receive
        // any payload. The allocator is a module-level export ready right after
        // instantiation and independent of the actor's `init`.
        let realloc = instance.get_typed_func::<(u32, u32, u32, u32), u32>(&mut store, "realloc_p32").ok();
        // Allocate the reused SMALL delivery region once, up front, and cache
        // its (non-null) pointer for the hot path. The LARGE region is grown
        // lazily by `place` only when a payload exceeds the small floor.
        let small_ptr = if let Some(realloc) = &realloc {
            #[allow(clippy::cast_possible_truncation)]
            let ptr = realloc.call(&mut store, (0, 0, DELIVERY_ALIGN, SMALL_REGION_BYTES as u32))?;
            if ptr == 0 {
                return Err(wasmtime::Error::msg(
                    "guest allocator returned null for the small delivery region at instantiate",
                ));
            }
            ptr
        } else {
            0
        };
        let mut large_ptr: u32 = 0;
        let mut large_cap: u32 = 0;
        // Wasm32 ABI carries `u32` byte lengths; both payloads are
        // bounded by guest memory size (well below `u32::MAX`).
        #[allow(clippy::cast_possible_truncation)]
        let config_len = config_bytes.len() as u32;
        #[allow(clippy::cast_possible_truncation)]
        let params_len = params_bytes.len() as u32;
        // ADR-0170: the config-only fallback exports cannot carry a bag, so a
        // guest without a params export can only be reached with nothing to
        // inject. Named once here rather than re-derived at each fallback arm.
        let params_dropped = || {
            wasmtime::Error::msg(format!(
                "guest exports no params-bearing init shim but the host has {} bytes of \
                 requested params to deliver; rebuild the component against an ADR-0170 SDK",
                params_bytes.len(),
            ))
        };
        let place = |store: &mut Store<ComponentCtx>, large_ptr: &mut u32, large_cap: &mut u32| {
            Self::place_init_payload(
                store,
                &memory,
                realloc.as_ref(),
                small_ptr,
                large_ptr,
                large_cap,
                InitPayload { config: config_bytes, params: params_bytes },
            )
        };
        let init_rc = if let Some(type_tag) = type_tag {
            // ADR-0096: a multi-actor module loaded with an export selector;
            // the tag picks which exported type to construct. Never fall
            // through to an entry-only init.
            if let Ok(init_typed) = instance.get_typed_func::<(u64, u64, u64, u32, u32, u32), u32>(
                &mut store,
                "init_typed_with_parent_and_params_p32",
            ) {
                let ptr = place(&mut store, &mut large_ptr, &mut large_cap)?;
                Some(
                    init_typed
                        .call(&mut store, (mailbox_id, parent_mailbox_id, type_tag, ptr, config_len, params_len))?,
                )
            } else {
                if !params_bytes.is_empty() {
                    return Err(params_dropped());
                }
                if let Ok(init_typed) =
                    instance.get_typed_func::<(u64, u64, u64, u32, u32), u32>(&mut store, "init_typed_with_parent_p32")
                {
                    let ptr = place(&mut store, &mut large_ptr, &mut large_cap)?;
                    Some(init_typed.call(&mut store, (mailbox_id, parent_mailbox_id, type_tag, ptr, config_len))?)
                } else {
                    let init_typed = instance
                        .get_typed_func::<(u64, u64, u32, u32), u32>(&mut store, "init_typed_p32")
                        .map_err(|e| {
                            wasmtime::Error::msg(format!(
                                "export selector set but guest exports none of \
                                 `init_typed_with_parent_and_params_p32`, `init_typed_with_parent_p32`, \
                                 or `init_typed_p32` (not a multi-actor module?): {e}"
                            ))
                        })?;
                    let ptr = place(&mut store, &mut large_ptr, &mut large_cap)?;
                    Some(init_typed.call(&mut store, (mailbox_id, type_tag, ptr, config_len))?)
                }
            }
        } else if let Ok(init_with_parent_and_params) =
            instance.get_typed_func::<(u64, u64, u32, u32, u32), u32>(&mut store, "init_with_parent_and_params_p32")
        {
            let ptr = place(&mut store, &mut large_ptr, &mut large_cap)?;
            Some(
                init_with_parent_and_params
                    .call(&mut store, (mailbox_id, parent_mailbox_id, ptr, config_len, params_len))?,
            )
        } else {
            if !params_bytes.is_empty() {
                return Err(params_dropped());
            }
            if let Ok(init_with_parent) =
                instance.get_typed_func::<(u64, u64, u32, u32), u32>(&mut store, "init_with_parent_p32")
            {
                let ptr = place(&mut store, &mut large_ptr, &mut large_cap)?;
                Some(init_with_parent.call(&mut store, (mailbox_id, parent_mailbox_id, ptr, config_len))?)
            } else if let Ok(init_with_config) =
                instance.get_typed_func::<(u64, u32, u32), u32>(&mut store, "init_with_config_p32")
            {
                let ptr = place(&mut store, &mut large_ptr, &mut large_cap)?;
                Some(init_with_config.call(&mut store, (mailbox_id, ptr, config_len))?)
            } else if let Ok(init) = instance.get_typed_func::<u64, u32>(&mut store, "init") {
                // Legacy Phase 2 fallback. Discards config bytes — only safe
                // for `Config = ()`; macro-built typed-config guests export a
                // wider shim and never land here.
                Some(init.call(&mut store, mailbox_id)?)
            } else if let Ok(init) = instance.get_typed_func::<(), u32>(&mut store, "init") {
                Some(init.call(&mut store, ())?)
            } else {
                None
            }
        };
        if let Some(rc) = init_rc
            && rc != 0
        {
            let msg = store
                .data_mut()
                .init_failure
                .take()
                .unwrap_or_else(|| format!("guest init returned {rc} without staging an error"));
            return Err(wasmtime::Error::msg(format!("guest init failed: {msg}")));
        }

        // ADR-0015 hook exports are optional. A component whose
        // `WasmActor::on_dehydrate` is the default no-op still emits the
        // symbol via `export!`, but a raw-FFI guest without the macro
        // won't. Either way: look it up, store `None` if missing.
        // (Issue 584 Phase 3 retired `on_drop` — `unwire` is the
        // pre-shutdown hook now.) Named save/restore-side so the two
        // locals don't read as a `de`/`re` minimal pair.
        let save_hook = instance.get_typed_func::<(), u32>(&mut store, "on_dehydrate").ok();
        // ADR-0016: `on_rehydrate` takes `(version, ptr, len)` — the
        // substrate writes the state bytes into a delivery region (ADR-0095,
        // via `call_on_rehydrate`), then calls the shim with `(version, ptr, len)`.
        let restore_hook = instance.get_typed_func::<(u32, u32, u32), u32>(&mut store, "on_rehydrate_p32").ok();
        // Issue 584 Phase 2b: optional wire/unwire exports. Both take
        // the component's own mailbox id (same as `init`) so the guest
        // ctx can self-address. Raw-FFI guests without the macro won't
        // emit them; macro-using guests with default no-op trait
        // bodies still emit the symbol (the shim just calls into the
        // default body).
        let unwire = instance.get_typed_func::<u64, u32>(&mut store, "unwire").ok();

        // Issue 584 Phase 2b / Issue 640 Phase 2: store the `wire`
        // export rather than calling it here. `instantiate` runs
        // inside `spawn_actor` step 4 — BEFORE the trampoline mailbox
        // is registered (step 5–7). A wire-time send like
        // `aether.input.subscribe { mailbox: self.mailbox_id() }`
        // would race the input cap's `validate_subscriber_mailbox`
        // and warn-drop. `WasmTrampoline::wire` fires this hook
        // post-registration via the `NativeActor::wire` lifecycle
        // method. wire stays one-shot — the trampoline drops the
        // typed-func handle after the call.
        let wire = instance.get_typed_func::<u64, u32>(&mut store, "wire").ok();
        Ok(Self {
            store,
            memory,
            receive,
            wire,
            unwire,
            on_dehydrate: save_hook,
            on_rehydrate: restore_hook,
            realloc,
            small_ptr,
            large_ptr,
            large_cap,
            self_mailbox_id: mailbox_id,
        })
    }
}
