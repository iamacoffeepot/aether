# Concurrency smell patterns

Catalogue used by `/audit concurrency <crate>`. Each entry is a symbolic question the skill asks via RustRover MCP tools, not a regex. The IDE knows when `Slot<T>` aliases to `Arc<Mutex<Option<T>>>` and when a use of `Mutex` actually resolves to `parking_lot::Mutex` instead of `std::sync::Mutex` — leaning on the resolver catches both cases that regex would miss.

**Append-only.** Mark deprecated patterns inline with `status: deprecated — <reason>` rather than deleting; the history matters for reproducing older reports.

Entry shape:

```
## <stable-id> — <one-line summary>

- **Symbolic question**: <plain-English description of what the audit is asking>
- **Primary tool**: <mcp__rustrover__... + the query>
- **Confirmation tool**: <follow-up call to confirm the match isn't an alias false-positive>
- **kts script** (optional): `patterns/kts/<name>.kts` for patterns that need PSI walking
- **Severity**: high | medium | low
- **Suggested action**: <one or two sentences>

<one paragraph rationale, citing memories or ADRs that motivate the pattern>
```

---

## P-Arc-Mutex-Option — `Arc<Mutex<Option<...>>>` in cap state

- **Symbolic question**: which fields in this crate have a resolved type of `Arc<Mutex<Option<T>>>` for any `T`, including through type aliases?
- **Primary tool**: `mcp__rustrover__search_symbol` with `q: "Arc"` scoped to `<crate>/**/src/**/*.rs` to enumerate every `Arc` usage site. (RustRover returns symbol coordinates for type references, not just declarations.)
- **Confirmation tool**: for each hit, `mcp__rustrover__get_symbol_info` at the coords; inspect the resolved generic args. Only count the hit if the resolved type tree matches `Arc<Mutex<Option<*>>>` (any inner type). This filters out plain `Arc<T>` / `Arc<Mutex<T>>` and catches aliased `Slot<T>` forms because the resolver follows aliases.
- **kts script**: `patterns/kts/arc-mutex-option.kts` — walks PSI for struct fields, asks the resolver for the resolved type, matches the three-deep generic shape. Use the kts route when the per-symbol roundtrip from primary + confirmation is too slow on a large crate.
- **Severity**: high
- **Suggested action**: refactor to a bare field; if the slot truly needs to be reset at runtime, use `RefCell<Option<T>>` (actor is single-threaded post-ADR-0038).

Symptomatic of issue-629-era cap state retrofitted from the pre-actor model. Per memory `feedback_actor_state_no_locks`, actor state is plain fields — Mutex / RwLock / Atomic in cap fields signals legacy. Per memory `feedback_existing_primitive_smell`, the first `WasmTrampoline` draft had this exact shape and collapsed to a bare `Sender` field with no synchronization once the design realized `NativeTransport` already had it. -1000 lines.

## P-Mutex-Atomic-coexist — Mutex + Atomic in the same struct

- **Symbolic question**: which structs in this crate have at least one field whose resolved type contains `std::sync::Mutex` and at least one other field whose resolved type contains `core::sync::atomic::Atomic*`?
- **Primary tool**: `mcp__rustrover__run_inspection_kts` with `patterns/kts/mutex-atomic-coexist.kts` — a per-file PSI walker that enumerates struct decls, asks the resolver for each field's type, and emits a problem when both primitive families appear in the same struct.
- **Confirmation tool**: not needed; the kts script confirms symbolically.
- **Severity**: medium
- **Suggested action**: pick one sync primitive. If the Mutex and the Atomic protect related state, the Atomic is usually redundant (the Mutex already orders access). If they protect independent state, name them so the audit reads them as separate concerns.

Two primitives in one struct often means a writer picked an Atomic for one field, hit a needs-more-than-CAS scenario for another, added a Mutex, and never collapsed. Per the WasmTrampoline collapse in memory `feedback_existing_primitive_smell`, layered primitives usually unwind to one once the design is understood.

## P-loop-sleep — bare polling loop without a channel signal

- **Symbolic question**: which functions in this crate contain a `loop {}` block whose body calls `thread::sleep` (or any function whose resolved fully-qualified path ends in `::sleep`), with no concurrent `select!` / `recv_timeout` / `Condvar::wait_timeout` in the same loop scope?
- **Primary tool**: `mcp__rustrover__run_inspection_kts` with `patterns/kts/loop-sleep-without-channel.kts` — walks function bodies, finds `loop` PSI nodes, asks the resolver to confirm the inner call resolves to a `sleep` API, then scans the loop body's PSI for any channel-receive call. Reports the loop site only when no channel-receive is present.
- **Confirmation tool**: not needed; the kts script's resolver query distinguishes `std::thread::sleep` from `tokio::time::sleep` from `cpal::*::sleep` — each is reported separately so the suggested-action paragraph can call out the right replacement (sync vs async).
- **Severity**: medium
- **Suggested action**: replace the sleep with a `select!` (sync-channel + shutdown channel) or a `Condvar::wait_timeout` keyed on the actual state being polled.

Per memory `feedback_dont_conflate_failfast_with_sync`: polling barriers often carry two invariants (fail-fast AND implicit per-frame sync); deleting one breaks the other silently. Per memory `feedback_pool_worker_holds_own_sender`: even pool workers that "should" terminate via channel disconnect can polling-loop forever if the channel never closes. A channel-signal makes intent explicit.

## P-hand-rolled-Actor — hand-rolled trait impl bypassing `#[actor]`

- **Symbolic question**: which `impl` blocks in this crate implement `NativeActor` / `FfiActor` / `InstancedNativeActor` / `Actor`, and which of those `impl` blocks are NOT inside an expansion of the `#[actor]` proc-macro?
- **Primary tool**: `mcp__rustrover__search_symbol` with `q: "NativeActor"` (and once per other trait name) to find all impl sites in the crate. The resolver returns each impl's coords.
- **Confirmation tool**: `mcp__rustrover__get_symbol_info` on the impl site; inspect whether the IDE's metadata shows the impl was emitted by a proc-macro expansion. Manually-written impls land in the source tree directly; macro expansions are flagged as such by RustRover's resolver.
- **Severity**: medium (high if the impl is in `src/` outside a `tests` or `examples` subdir)
- **Suggested action**: convert to `#[actor]` macro form. If the impl is a test fixture, annotate it with `// hand-rolled: test fixture` so the audit can skip it on re-runs (the skill checks for that exact comment marker on the line above the impl).

Per memory `feedback_trait_default_audit_hand_rolled_impls`: production caps inherit via the `#[actor]` macro and pick up trait defaults from the macro expansion (e.g. `Actor::SCHEDULING`). Hand-rolled `impl Trait` blocks silently take the bare trait default — a different default. PR C learned this with `Actor::SCHEDULING` on the way through Phase 3. Test fixtures legitimately hand-roll; production caps should not.

## P-introspection-Arc — `Arc<A>` peering at sibling actor state

- **Symbolic question**: which call sites in this crate invoke `ctx.peer::<A>()`, `ctx.actor::<A>()`, `ctx.resolve_actor::<A>()`, or `chassis.actor::<A>()`, and which of those sites sit inside a method whose surrounding `impl` is annotated with `#[handler]` (i.e. runtime path, not init)?
- **Primary tool**: `mcp__rustrover__search_symbol` for each of the four method names (one search each — they're distinct symbols, the resolver returns the right call sites).
- **Confirmation tool**: `mcp__rustrover__get_symbol_info` at each call site; the IDE returns enough context to identify the enclosing function and its annotations. Init-time sites (inside `init` / `wire` of a supervisor cap) are skipped; handler-time sites become findings.
- **Severity**: high if called inside a `#[handler]`, medium if init-time on a non-supervisor cap.
- **Suggested action**: if init-time on a supervisor cap, keep — that's the legit bootstrap path. If runtime / handler-time, switch to mail (`ctx.send_to_named`); peering at sibling state violates the actor model.

Per memory `feedback_introspection_shape_constraints`: runtime cap handlers communicate via mail, never via `Arc<A>`. Per memory `feedback_actor_lookup_bootstrap_only`: `actor::<A>()` lookup is bootstrap-only, never on per-handler `NativeCtx`/`WasmCtx`/`WasmInitCtx`. PR 627 closed runtime paths; PR 630 closed init `peer<A>` + chassis `.actor::<X>()`. Cap supervisors hold cap-local children, don't walk the chassis registry.

## P-unbounded-channel — unbounded mpsc on a hot path

- **Symbolic question**: which call sites in this crate invoke `mpsc::channel` / `mpsc::unbounded_channel` / `crossbeam::channel::unbounded` and resolve to an unbounded variant?
- **Primary tool**: `mcp__rustrover__search_symbol` for `channel`, then `mpsc`, then `unbounded_channel` — each returns the relevant call sites.
- **Confirmation tool**: `mcp__rustrover__get_symbol_info` at each call site to read the resolved signature; flag the site only when the resolved API has no bound argument (e.g. `std::sync::mpsc::channel()` vs `std::sync::mpsc::sync_channel(N)`).
- **Severity**: low
- **Suggested action**: assess whether the channel can ever fill faster than it drains. If yes (component inboxes, hub outbound), specify a bound. If no (one-shot reply paths), keep unbounded but annotate `// intentionally unbounded: <reason>`.

Surfaced by the prior CODE_CONCURRENCY_AUDIT (2026-04-29). Memory growth under slow consumers is a tail risk; the audit catches it but the fix usually wants a domain read. False-positive friendly because much of the codebase rightly uses unbounded for short-lived correlation flows.
