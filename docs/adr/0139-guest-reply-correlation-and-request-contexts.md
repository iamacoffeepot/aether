# ADR-0139: Guest Reply Correlation and Request Contexts

- **Status:** Accepted (shipped — kind-typed request contexts + reply correlation ids in aether-actor; consumers migrated across fs/audio/text/behavior/kit)
- **Date:** 2026-07-08

Amends **ADR-0134** (multi reply class): one-shot request/reply flows correlate on the envelope, not the payload; `multi`-class emissions keep the payload-level keying ADR-0133 established. Builds on the correlation machinery **ADR-0042** left in place at its retirement, the `MailId` / causal-chain model of **ADR-0080**, the reply classes of **ADR-0109** / **ADR-0112**, and the inline-cluster addressing of **ADR-0114**.

## Context

An actor with several requests of the same kind in flight has no way to key the replies back to the requests that caused them. The identity needed for that demux already exists end-to-end on the wire: every send mints a per-actor monotonic `correlation_id` (`NativeBinding::send_mail`, generalised to always-present by ADR-0080 — the value is the `MailId`'s correlation half), and `Mailer::send_reply` echoes the request's correlation onto the reply envelope's `reply_to: Source` unconditionally (`crates/aether-substrate/src/mail/mailer.rs`, the ADR-0042 echo). The value is unreadable at exactly the point where demux happens:

- **Wasm guests never see it.** The `receive_p32` ABI threads `(kind, ptr, byte_len, count, sender, recipient, source)` — no correlation slot. The send-side half ships as `prev_correlation_p32` (kept at ADR-0042's retirement specifically for the send-then-match-across-handlers pattern), but the receive-side mirror was never built.
- **Native reads are buried.** `NativeCtx::reply_target() -> Source` carries the echoed correlation, but the API is named and documented for replying, and `send_tracked -> MailId` is documented for settlement subscription. No consumer uses either for demux.

Every reply consumer therefore hand-rolls a pending map keyed on payload-echoed identity fields: the behavior host's `PendingLoad` queue matched on echoed `(namespace, path)` with a FIFO tiebreak, the text cap's `pending_fonts`, the audio cap's three `pending_tracks` / `pending_instruments` / `pending_samples` maps, and aether-kit's mesh/world single-slot variants. The pattern's defects are structural: two in-flight requests with identical echoed fields are distinguishable only by arrival-order assumption, unmatched replies are warn-dropped, the requester's context (the stashed original `ReplyHandle`, the load's purpose) lives in a side map regardless, and each cap re-decides the same eviction and mismatch policies. The `fs` kind vocabulary states the gap outright: "v1's cross-actor mail has no protocol for allocating correlation ids — operation identity comes from the reply kind itself, target identity from the echoed fields."

The substrate is a general application host; concurrent request demux is the core inner loop of server-shaped work, so the consumer count grows from here.

## Decision

### 1. The reply-echo is a contract

Every reply routed through the reply machinery — a `#[handler::single]` return, a `manual` handler's `ctx.reply` / `ctx.reply_to`, native `Mailer::send_reply` — carries the request's `correlation_id` on its envelope's `reply_to.correlation_id`, with `reply_to.addr = SourceAddr::None`. This has been the implemented behavior since ADR-0042; it is now documented API. The contract costs capability authors nothing: every reply path funnels through the mailer/host code that performs the echo, so it cannot be forgotten per-handler.

A reply envelope is therefore recognisable as such: `addr == SourceAddr::None && correlation_id != NO_CORRELATION`. Broadcast and input mail carry `(None, 0)`; request mail carries the sender's addressable `Source` with the sender's own correlation.

### 2. `reply_correlation_p32`: an additive host import, no ABI widening

A new import on the `aether` module, the receive-side mirror of `prev_correlation_p32`:

```rust
pub fn reply_correlation() -> u64;
```

It returns the current inbound dispatch's echoed correlation when that dispatch is a reply envelope per §1, and `0` (`NO_CORRELATION`) otherwise — the host gates on `addr == None && correlation != 0`, so a request handler reading it during a request dispatch gets `0` rather than the *requester's* correlation (which lives in a different actor's id space and would false-match the reader's own pending keys). The host stashes the value on the component's ctx at dispatch entry; it already holds the envelope there for the reply-echo path.

Additive is the load-bearing property: a new import leaves `receive_p32`'s signature untouched, so component wasm already stored in the hub's content-addressed store keeps linking and running. Folding the correlation into `receive_p32` as a real parameter is the tidier permanent home *if* that ABI is ever revised for other reasons; nothing here forecloses that.

**Inline-cluster boundary (ADR-0114).** Cluster-local sends never reach the host: `route_or_enqueue`'s `Local` branch buffers `QueuedMail { recipient, kind, bytes, count, sender }` — no correlation is minted and the in-place reply table is empty, so correlated replies do not exist on that path. The guest SDK gates accordingly: a ctx constructed for a cluster-drained dispatch answers `in_reply_to()` with `None` unconditionally (otherwise the FFI read would return the *outer* host dispatch's stale value), and `send_tracked` on a `Local`-routed recipient warn-logs and returns the no-correlation sentinel rather than reading a stale `prev_correlation`.

### 3. SDK surface: `send_tracked` and `in_reply_to`

- `WasmActorMailbox::send_tracked(&payload) -> RequestId` — the send plus a `prev_correlation` read, guarded by `route_decision` per §2. The wasm sibling of native `send_tracked`.
- `WasmCtx::in_reply_to() -> Option<RequestId>` — `Some` exactly when the inbound dispatch is a reply envelope (§1), read via the §2 import; `None` on request dispatches, broadcasts, and cluster-drained dispatches.
- `NativeCtx` gains the same two spellings for parity (`in_reply_to` reading the inbound `Source` it already holds), so native caps and guests migrate with the same diff shape.

`RequestId` is a `u64` newtype over the correlation half of `MailId` (the sender half is the requesting actor itself, so the correlation alone is unique within the actor's pending set — per-actor monotonic, never reused within a substrate run). Consumers key pending maps by it directly.

### 4. Kind-typed request contexts (the SDK table)

On top of the primitive, `aether-actor` owns the pending map itself:

- `send_with_context(&request, context)` — a tracked send that stores `context` keyed by the minted `RequestId`. The context type is a **`Kind`**: the table stores `RequestId -> (KindId, encoded bytes)`, so storage is schema-typed rather than type-erased.
- `ctx.take_context::<C>() -> Option<C>` in the reply handler — resolves `in_reply_to()`, removes the entry, checks the stored `KindId` against `C::ID` (a mismatch warn-logs and returns `None` — exact, unlike an `Any` downcast), and decodes.
- **Hot reload:** because entries are `(KindId, bytes)`, the table serialises through the existing `save_state` machinery — pending requests ride `on_dehydrate` / `on_rehydrate` across `replace_component` with no per-consumer code.
- **Eviction:** bounded, drop-oldest with a warning naming the kind and entry age. Capacity is an SDK default constant — the table lives in actor memory (guest-side, a wasm static), which the ADR-0090 chassis-knob seeding does not reach cheaply; a knob follows only if a real consumer needs one. Eviction is memory hygiene only, never correctness — correlation ids are monotonic, so a stale entry can never be wrongly matched by a later reply, and an entry holds nothing open engine-side (no settlement hold; the leak's blast radius is the actor's own heap). Contexts are bookkeeping, not storage: fat state (an assembling bank, a parsed manifest) belongs in actor fields keyed by a small id the context carries.
- **Scope:** take-once semantics serving `single` and `manual` reply flows. `multi`-class streams are out of scope by construction — their emissions are detached chain roots that echo nothing, and payload `stream_id` keying remains their correlation story (ADR-0133 / ADR-0134). Identity lives at the layer that spans the relationship: one reply → the envelope already spans it; N messages over time → only the payload does.

### 5. Echoed payload fields are demoted to informational

Reply kinds keep their echoed `namespace` / `path` style fields for log and MCP-caller readability, and the `fs` kind vocabulary's doc prose is updated to say so; they stop being the demux key. No kind schema changes.

## Consequences

- Every hand-rolled pending map migrates to an exact key: behavior host, text, audio, aether-kit mesh/world (issues #2793–#2796), with the FIFO tiebreaks, field-scans, and their tripwire tests deleted. New request/reply consumers start from `send_with_context` / `take_context`.
- ADR-0134's consequence clause ("payload correlation is the domain-level linkage") narrows to `multi`-class emissions; one-shot request/reply correlates on the envelope.
- The wasm syscall table grows by one read-only import that mirrors an existing one; no new concept enters the FFI vocabulary. No wire-format change anywhere — the envelope already carries the field.
- The unmatched-reply, eviction, and hot-reload policies for pending requests are decided once in `aether-actor` instead of re-decided per capability.
- A future revision of `receive_p32` (for unrelated reasons) should fold the correlation in as a real parameter and retire the pull-style import.
- Settlement-tied per-request reclamation (subscribe to the request chain's settlement; evict on settle-without-take) remains available as an opt-in later rung — it costs a detached send plus a settlement subscription per request, wrong as a default for a table of small entries.

## Alternatives considered

- **Payload-embedded context / request tags** (caller writes a tag or context blob into the request kind; responder echoes it) — perpetual cost in every kind pair, visible in every schema, requires responder compliance forever, and sends requester-local data (a `ReplyHandle` is a session-local host handle) on a round trip through a foreign actor. The envelope already round-trips the eight bytes needed.
- **Blessing the echoed-field convention with a derive** (declare "this reply correlates by `(namespace, path)`", macro-generate the pending map) — cheapest-looking option, but it codifies a key that does not uniquely name a request; the generated map still needs the arrival-order tiebreak. Machinery that launders an ambiguity.
- **Ephemeral reply mailboxes** (a throwaway child mailbox per request; the mailbox is the identity) — the serious in-model alternative; loses on weight and fit: actor-lifecycle churn per file read, and inline children are deliberately init-time structures, not per-request disposables. Identity-by-mailbox suits long-lived conversations, which is what ADR-0133 stream handles already cover.
- **Widening `receive_p32`** — the same primitive delivered as an ABI break: every stored component wasm stops linking. Rejected for delivery, recorded as the eventual home if the ABI is revised anyway.
- **A blocking reply wait** — re-litigating ADR-0042's retirement; an in-handler block can park a shared pool worker and deadlock. The send-then-match-across-handlers shape stays the sanctioned pattern; this ADR makes it exact.
