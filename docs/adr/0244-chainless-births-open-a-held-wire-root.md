# ADR-0244: Chainless Births Open a Held Wire Root

- **Status:** Proposed
- **Date:** 2026-09-29

Builds on [ADR-0080](0080-substrate-mail-tracing-and-settlement.md) (settlement:
`(in_flight == 0 && held_open == 0)` is exact, and a `SettlementHold` keeps a
root open) and [ADR-0168](0168-settlement-completeness-for-staged-effects.md)
(a staged effect holds its causing chain; an effect with no chain is declared
through `EffectChain::Uncaused`). Settles the case ADR-0168 left as an effect
nobody can wait on: the `wire` of a birth with no causing chain.

## Context

An actor's `wire` hook runs on a context that dispatches no inbound. For a
handler-staged birth that context carries the staging chain as
`NativeCtx::causing_chain` (ADR-0168 §1), and a birth-completing effect holds
it. Two births have no causing chain, and declare so (ADR-0168 §3):

- **a chassis boot** — `Uncaused::ChassisBoot`: the Pass 3 wire
  (`NativeActorBoot::wire`), a pre-seal direct commit
  (`Spawner::commit_directly`), and a driver `Start` pumped actor
  (`assemble_pumped_slot`);
- **an embedder spawn** — `Uncaused::EmbedderCall`: a post-seal
  `SpawnBuilder::finish`, which runs `wire` in the staged activation
  (`DispatcherSlot::wire_activation`).

`NativeCtx::for_wire` left `in_flight_root` unset on both. Each send `wire`
made therefore minted a root of its own from its own mail id
(`NativeBinding::push_envelope_buffered_with_reply_to`,
`inherited_root.unwrap_or(mail_id)`), and no caller learned any of them.
Nothing else in reach says when `wire`'s mail has landed:

- `wire`'s sends are buffered, and the flush only schedules a burst on the
  pool. A caller's own later send can overtake them, so FIFO is no signal.
- `SpawnBuilder::finish` returns when the activation promotes the route to
  `Live`, before anything `wire` sent is delivered.
- The pool has no idle signal, and one would also wait on unrelated work.

A test that needed `wire`'s effects to have landed polled for them against the
clock (`chassis/builder/tests/wire_pass.rs`). That breaks the rule that tests
wait on settlement.

ADR-0168 rejected giving the `wire` context the causing chain as its root: the
context serves both effects that complete the birth, which the causing chain
must cover, and the actor's own startup sends, which it must not, and one root
cannot tell the two apart. That argument is about a root the birth's *caller*
already holds. A chainless birth's caller holds none, so a fresh root has
nothing to be confused with.

## Decision

**A chainless birth runs its `wire` under one fresh, held root: its wire
root. Every send `wire` makes, and everything those sends cause, settles
under it.**

```rust
// main: each wire send mints a root nobody learns
NativeCtx::for_wire(&binding, EffectChain::Uncaused(Uncaused::EmbedderCall))
// in_flight_root: None

// decision: the birth opens one held root and the ctx carries it
let wire_root = WireRoot::open(&mailer);   // mint, then hold
NativeCtx::for_wire(&binding, EffectChain::Uncaused(Uncaused::EmbedderCall), Some(wire_root.root()))
// in_flight_root: Some(root); sends inherit it through outbound_root
```

1. **`WireRoot` owns a fresh root and the hold on it.**
   `WireRoot::open` (`runtime/wire_root.rs`) mints the root through
   `Mailer::mint_wire_root`, from the same chassis-root counter as
   `Mailer::mint_chassis_root`, so it collides with no other root. It records
   no `Sent`: no mail carries the root as its own id. It then takes the
   `SettlementHold`. A root held with no sends settles when its hold is
   released, so the hold is what lets a `wire` that sends nothing settle, and
   what keeps a root whose sends are still being buffered from settling early.
   Every buffered send bumps the root's `in_flight` when it is buffered, so
   once the hold is released the root settles exactly when the last mail it
   caused has been handled.

2. **The root is always fresh, never the causing chain.**
   `for_wire(binding, chain, wire_root)` sets `in_flight_root` to `wire_root`,
   and buffered sends inherit it through `outbound_root`. A handler-staged
   birth (`EffectChain::Held`) passes `None`, so its `wire` sends mint their
   own roots as before and ADR-0168's separation is untouched. A
   `debug_assert!` refuses a wire root beside a causing chain.

3. **Boot opens one root for every pre-seal birth.** `boot_passives` opens it
   through `Spawner::open_boot_wire` right after the `Spawner` is built, before
   any birth. The `Spawner` keeps it beside its `BootAuthority`, and the Pass 3
   wire (`PassiveBoot::wire`), the pre-seal direct commit, and a driver `Start`
   pumped actor all read it through `Spawner::boot_wire_root`. `Spawner::seal`
   drops it with the authority. After the seal `boot_wire_root` is `None`: a
   later birth is an embedder's and opens its own.

4. **An embedder spawn opens one root per birth.** `commit_through_owner`
   opens a `WireRoot` and passes it through `prepare_commit_as` into the staged
   activation, which runs `wire_activation` under its root. The hold moves into
   the activation's catch-up suffix and is dropped right after
   `release_outbound_after_activation` routes the held mail. A cancelled
   activation drops it with itself, after `discard_outbound_after_activation`
   has balanced every counted send.

5. **A wasm guest's `wire` runs under the same root.**
   `Component::wire(root)` publishes the root on the guest ctx's in-flight
   cells for the call and clears them after, as `Component::deliver` does with
   an inbound's lineage. The trampoline's `wire` passes `ctx.in_flight_root()`.

6. **Holds taken in a chainless `wire` gate its wire root.** `held_chain`
   returns the in-flight root, so a task, a deferred reply, or a staged
   registry batch that `wire` starts is inside the root. The rule: **work that
   `wire` starts on its own chain must finish.** Long-lived work opens a
   detached chain (`send_detached`, `spawn_detached`) instead.

7. **The waits are test-support only.**
   `PassiveChassis::await_boot_settled` and `SpawnBuilder::finish_wire_settled`
   wait on the root's settlement with `testing::await_settled`, and are gated
   on `cfg(any(test, feature = "test-support"))` like
   `PassiveChassis::await_closed`. Each subscribes while the root's hold is
   still held, so the root cannot settle and leave the registry's settled set
   before the subscription lands. `await_boot_settled` is idempotent.
   `finish_wire_settled` on a pre-seal spawn panics naming the boot wait,
   because that spawn's `wire` ran under the boot's root and has none of its
   own. A production readiness signal waits for a consumer that needs one.

## Consequences

### Positive

- **Tests wait on settlement.** `wire_pass.rs` loses its poll: the boot test
  calls `await_boot_settled`, and a spawn test calls `finish_wire_settled`.
  A `wire` send that does not inherit the root, or a hold released before the
  held mail is flushed, makes the wait return before the effect lands.
- **The boot's `wire` traffic is one chain.** Everything the boot's `wire`
  hooks cause settles under one root, instead of one unlearned root per send.

### Negative / limits

- **A chain that never finishes keeps the root open.** A `wire` that starts
  such a chain on its own root makes the wait panic at the settlement cap with
  the gate named (`chassis.boot_settled`, `spawn.wire_settled`). Production
  never waits on it. The root stays live in the settlement table, so
  `SettlementTable::is_live` keeps the trace rings from reclaiming its entries
  and a ring holding them grows toward its ceiling (`trace_max`).
- **A wire root has no trace tree of its own.** It has no `Sent`, so
  `aether_trace::stitch_with` reports `Err { not_found }` for it. The boot's
  trace is one tree whose root has no send event. The id is never handed out,
  so nothing queries it.
- **A post-seal pumped actor gets no wire root.**
  `PassiveChassis::boot_pumped_actor` runs after the seal and opens none; its
  `wire` sends mint their own roots, and `PumpedDriver::settle` covers them.

### Neutral / forward

- `Uncaused::ChassisBoot` and `Uncaused::EmbedderCall` stay the declarations
  of those births: no chain *caused* them. The wire root is fresh, so it does
  not change what ADR-0168 §3 records.
- The fleet proxy's `route_hold`, taken in `wire`, now gates the embedder
  spawn's wire root when a test spawns the proxy directly, so that root
  settles only once the route is registered. Its production birth is
  handler-staged and holds the causing chain as before.

## Alternatives considered

- **Thread the causing chain as the wire root.** ADR-0168 rejected it: one
  root cannot tell birth-completing effects from startup sends.
- **Hold only across Pass 3.** Pre-seal direct commits and driver `Start`
  `wire` sends would escape it, leaving several roots to wait on.
- **Pool-idle detection.** No such signal exists, and it would also wait on
  work unrelated to `wire`.
- **Keep `held_chain` on the causing chain for `wire` contexts.** Tasks staged
  in `wire` would complete on unrelated chains, and the wait would under-report.
- **A FIFO barrier from a test's direct tracked send.** `wire`'s sends are
  buffered and only scheduled on the pool, so a direct send can overtake them.
