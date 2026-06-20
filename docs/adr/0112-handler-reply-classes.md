# ADR-0112: Handler reply classes

- **Status:** Accepted (shipped — the reply-class migration + lock, #1850 / #1874–#1879)
- **Date:** 2026-06-14

## Context

ADR-0109 made a handler's return type its reply contract: `-> R` replies `R`, `-> ()` replies nothing, `-> Pending<R>` replies later. It deliberately kept `ctx.reply` / `ctx.reply_to` as a coexisting second path "for what a return can't express" — a reply redirected to a third party, or one emitted partway through a handler. That second path is the gap this ADR closes.

`ctx.reply` is an ambient capability: it is implemented on the one ctx type every handler receives (`OutboundReply` for `WasmCtx` at `aether-actor/src/ffi/ctx.rs:339`, for `NativeCtx` at `aether-substrate/src/actor/native/ctx.rs:1082`), so any handler can call it regardless of what its return type promises. Three consequences follow:

1. **`-> ()` does not mean "replies nothing" — it means "the macro replies nothing on your behalf."** A `-> ()` handler can still `ctx.reply(&r)` by hand. The manifest records `reply: None` for it (read off the return type, ADR-0109 §4), which is then a false statement: the handler does reply, the introspection surface says it does not, and the in-harness driver that reads the surface to decode a response is misled.

2. **The migration ADR-0109 set up can't be verified.** ADR-0109's migration is "opportunistic — existing `ctx.reply`-tailed handlers keep compiling as `-> ()` that reply by hand." Nothing distinguishes a handler that genuinely replies nothing from one not yet converted, so "is the migration done" has no answer, and the manifest keeps lying for every unconverted handler in the meantime. Across the tree that is ~148 `ctx.reply` sites (99 in `aether-capabilities`).

3. **Streaming and redirected replies have no home in the type system.** ADR-0109 punted streaming entirely and left redirected replies on the ambient `ctx.reply_to`. Neither is *declared*, so neither is introspectable, and the difference between "always replies one `R`" and "emits replies on its own schedule" lives only in the body.

The forces are the ones ADR-0109 named — request/reply is asynchronous (ADR-0074, no in-handler blocking), settlement discharges structurally (ADR-0106), the manifest is the driver's source of truth — plus a new one: the reply surface a handler can reach should match what its signature promises, checked by the compiler, so an agent authoring a handler can't cross wires between "I reply nothing" and "I reply by hand."

## Decision

A handler declares a **reply class**, and the ctx it receives exposes only the reply surface that class permits. Three classes, named for the reply shape:

| class | attribute | replies | ctx reply surface |
|---|---|---|---|
| single | `#[handler::single]` = `#[handler]` | 0 or 1, via the return value (ADR-0109) | none — the return value is the only reply |
| manual | `#[handler::manual]` | issued by the handler | `reply` / `reply_to` |
| stream | `#[handler::stream]` | many, over time | `emit` (reserved — not built) |

`#[handler]` is `#[handler::single]`; the bare form stays the overwhelming common case. The trigger axis composes in parens, unchanged from ADR-0093: `#[handler::manual(task)]` is a manual-class task handler.

**single is total.** A single handler that replies, replies — `-> R` and `-> Pending<R>` reply unconditionally; `-> ()` never replies. There is no conditional-reply shape: an optional reply is a concrete reply kind whose schema names the absent case (`enum Lookup { Found(T), Missing }`), never `-> Option<R>` and never a skipped `ctx.reply`. This keeps the manifest's reply at one declared kind and serves a machine caller, which is better handed an explicit "nothing found" reply than intermittent silence.

**The mode is a marker on the ctx type.** One ctx type per target carries a phantom `ReplyMode` parameter — `NativeCtx<'a, M = Single>`, `WasmCtx<'a, M = Single>` (`aether-substrate/src/actor/native/ctx.rs:57`, `aether-actor/src/ffi/ctx.rs:124`). `OutboundReply` (`reply` / `reply_to`, `aether-actor/src/actor/ctx/outbound_reply.rs:21`) is implemented only for `…<Manual>`; the stream `emit` surface only for `…<Stream>`; the shared inherent surface (`ctx.actor::<Cap>()`, logging) for every mode. So `ctx.reply` in a single handler does not compile. The compiler enforces it — there is no lint to suppress or convention to remember.

**The macro enforces class ↔ ctx agreement.** `#[actor]` reads the class off the marker attribute (an inert marker it parses and strips, so the `::` path never reaches attribute resolution) and generates the dispatch call with that class's ctx view, produced by a downgrade-only coercion from the full ctx the runtime holds (`as_single` / `as_stream` drop capability; there is no escalating `as_manual` a single handler could reach). The written signature must unify with what the macro passes:

- `#[handler::manual]` + `ctx: &mut NativeCtx<'_, Single>` → the macro passes the manual view → mismatch → compile error.
- `#[handler]` (single) + `ctx: &mut NativeCtx<'_, Manual>` → the macro passes `as_single()` → mismatch → compile error.

Because `M` defaults to `Single`, an unmarked `NativeCtx<'_>` is the single ctx, so existing single signatures are unchanged; a manual or stream handler *must* spell its marker to compile. The class is then stated twice — attribute and signature marker — and pinned consistent by the macro. The redundancy is the point: read either and you know the class, and they cannot contradict.

**The manifest reports the class.** ADR-0109's `reply: Option<KindId>` on `InputsRecord::Handler` (`aether-data/src/schema.rs`, encoded in `aether-data/src/canonical/inputs.rs`) becomes `ReplyContract { None | One(KindId) | Stream(KindId) | Manual }`, and the `aether.kinds.inputs` custom-section version bumps `0x03 → 0x04`. `describe_component` / `describe_handlers` then report the real shape: a manual handler is `Manual` (dynamic, no single static kind), not a `None` that claims it never replies.

**stream is reserved, not built.** `#[handler::stream]` is parsed and rejected by the macro with a "not yet implemented" error, and `ReplyContract::Stream` exists in the manifest vocabulary. ADR-0109 placed the streaming shape behind a stream-completion primitive that does not exist yet; this ADR claims the class name and manifest variant so the eventual primitive has a home and no competing streaming model drifts onto `manual` in the meantime.

## Consequences

### Positive

- **`-> ()` is provably silent.** A single handler has no reply method on its ctx, so the manifest's `None` is true by construction — the false-statement gap (context §1) is closed in the type system.
- **The migration is verifiable.** "Is a handler converted" becomes "does it still reach `ctx.reply` from a `Single` ctx," which is a compile error once `single` is locked. The transitional period has a definite end: the `Single` ctx stops implementing `OutboundReply`.
- **Redirected and streaming replies are declared.** `manual` and `stream` are introspectable classes rather than ambient capabilities; the driver sees `Manual` / `Stream` instead of a misreported `None`.
- **Intent can't cross wires.** The class is stated in the attribute and echoed in the signature, both checked — the explicitness an agent authoring a handler needs.

### Negative / limits

- **The ctx gains a type parameter.** `NativeCtx` / `WasmCtx` become `…<M = Single>`, and `OutboundReply` / the stream surface move to per-mode impls. The default keeps the common signature unchanged, but the runtime-facing plumbing (erased dispatch, the `as_*` coercions) carries the marker.
- **A whole-tree migration.** Every handler that replies by hand today (~148 sites) moves to `#[handler] -> R` (the bulk) or `#[handler::manual]` (redirects — the ~33 `reply_to` sites). It is mechanical and behavior-preserving, but large, and lands as a sequence rather than one change.
- **`manual`'s reply stays undeclared.** A `Manual` handler reports `Manual`, not a kind — the contract a single handler gets is unavailable where the body decides the reply at runtime. That is the price of the escape hatch, narrowed to the handlers that genuinely need it.

### Neutral / forward

- **Extends ADR-0109** (the return-type contract becomes the `single` class; the `ctx.reply` second path becomes the `manual` class) and **ADR-0093** (the trigger axis composes as `#[handler::manual(task)]`). Routes through ADR-0106's discharge path unchanged.
- **Sequenced to stay non-breaking.** The mechanism lands first with a transitional `OutboundReply for <Single>` impl so existing `#[handler]` handlers keep compiling; the per-crate migration moves reply handlers onto their class; a final change drops the transitional impl and locks `single`. Tracked as separate issues — #1850 carries the mechanism.
- **stream awaits its primitive.** When the stream-completion primitive ADR-0109 deferred exists, `#[handler::stream]` gets its `emit` surface and `ReplyContract::Stream` becomes live; until then the class is a reserved name.

## Alternatives considered

- **A clippy lint forbidding `ctx.reply` outside declared repliers.** Rejected: a lint is suppressible (`#[allow]`), leaves the manifest lying when suppressed, and makes the migration "defend a lint per call site" rather than "satisfy the compiler." The type system enforces the same rule with no escape that drift can hide in.
- **A `#[handler(reply = R)]` declaration for manual handlers.** Rejected for the reason ADR-0109 rejected it for single handlers: a declaration beside the code that issues the reply is a second source that drifts. The class marker carries no kind; the manifest reports `Manual`.
- **Distinct ctx types (`Ctx` / `ManualCtx` / `StreamCtx`) instead of a mode marker.** Rejected: the signature reads marginally more plainly, but it multiplies the ctx into a trio per target (six across native and wasm) with conversions between them, where the marker keeps one type per target and selects the surface by which traits it implements.
- **Conditional-reply `-> Option<R>` for single.** Rejected: it makes `single` non-total and pattern-matches a std type users reach for meaning other things. Optionality belongs in a concrete reply kind, which keeps the manifest at one declared kind and the caller's expectation explicit.
- **A streaming return `-> Stream<R>`.** Rejected here as in ADR-0109: it competes with the pub-sub topic layer and its completion question is the settlement-closure primitive's. `stream` reserves the class without committing the mechanism.
