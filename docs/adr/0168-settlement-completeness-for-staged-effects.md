# ADR-0168: Settlement Completeness for Staged Effects

- **Status:** Accepted
- **Date:** 2026-07-31
- **Accepted:** 2026-07-31 — all three requirements implemented: #4200 (requirement 2), #4211 (requirement 1), #4214 (requirement 3).
- **Last amended:** 2026-07-31 (iamacoffeepot/aether#4199) — requirement 2's diagnostic reach corrected, the conforming table extended to the shapes the implementation actually found. See "Amendment" below.

## Context

Settlement exists so that associated actions declare when they have finished propagating. `Settled { root }` answers one question for a consumer: *is everything this mail caused now observable?* A consumer that cannot rely on that answer covering every descendant effect cannot use settlement as a barrier at all, because using it correctly would require knowing which effects it silently omits.

The contract that makes this exact is already binding. ADR-0080's 2026-05-20 revision (iamacoffeepot/aether#1031) rejected a timing window in favour of an explicit hold contract:

> a handler that will send chain mail after it returns holds a `SettlementHold` on the root until its last send […] Under that contract `(in_flight == 0 && held_open == 0)` is an **exact** settlement signal — the counter does not transiently reach zero with work still coming — so `Settled` fires once and is not merely a hint.

and §6's amended resolution states the prohibition directly:

> The transient-zero scenario described above arises in practice only from deferred work that fails to hold — which the contract forbids.

ADR-0165 extended the same obligation from deferred *mail* to staged *effects* when it introduced owner-applied registry batches:

> An ADR-0080 settlement hold is acquired when the staged operation is armed. It remains held through owner apply, activation, and the parent's completion handler.

Both documents therefore already say that a staged effect holds the chain that caused it. Neither is being honoured on every path, and nothing detects the omission.

### How the contract is silently violated

`NativeCtx` carries an `in_flight_root: MailId` naming the chain the current work belongs to. `acquire_settlement_hold` reads it:

```rust
pub fn acquire_settlement_hold(&self) -> SettlementHold {
    self.mailer().acquire_settlement_hold(self.in_flight_root)
}
```

and the counter treats the absent root as nothing to hold:

```rust
pub fn acquire_settlement_hold(&self, root: MailId) -> SettlementHold {
    if root != MailId::NONE {
        self.settlement_counter.record_hold_open(root);
    }
    SettlementHold { handle: self.clone(), root }
}
```

A `SettlementHold` is returned either way. It is `#[must_use]`, it satisfies the type, it reads at the call site exactly like a hold that works, and when `root` is `MailId::NONE` it holds nothing.

The actor activation path constructs its context with no root (`crates/aether-substrate/src/actor/native/dispatcher_slot.rs:194`):

```rust
let mut ctx = NativeCtx::new(&self.binding, Source::NONE, MailId::NONE, MailId::NONE);
A::wire(actor.as_mut(), &mut ctx);
```

So every effect a newborn actor stages from `wire` arms a hold against `MailId::NONE`. The staging code is not opting out — it calls `arm_deferred_completion`, which calls `acquire_settlement_hold`, exactly as ADR-0165 prescribes. It asks for the hold and receives a guard that means nothing. The causal edge is cut by a default value propagating into a position no one checks, with no verb at the call site and nothing in the type recording that it happened.

### What it has cost

Four flakes were filed and fixed as independent test bugs before the shared cause was identified: #4164, #4184, #4186, #4192. Each was discovered as a timing failure on a differently-loaded runner rather than read off a stated contract, and each was worked around in the test by polling for an effect the caller had no way to await. #4186's fix is the clearest illustration — a settlement-gated send was measured not to cover the inline child's alias, because the alias holds no chain to be covered by.

The tests are the cheap consequence. The expensive one is that `Settled`'s exactness is load-bearing for consumers that cannot be idempotent. ADR-0080 §6's amended resolution names the case it was written for: ADR-0047's DAG `Call` node closes its output `Bundle` on `Settled`, "a destructive, un-repeatable act". A chain that settles while a staged effect is still unapplied hands that consumer a premature terminal signal. No such failure has been observed, and the tests found the gap first only because they are the densest current user of settlement as a barrier.

There is a second, distinct shape that this contract does not currently reach. Some effects are emitted with no chain in scope at all: the actor close tail (`finalize_close_and_fan_out`) runs its registry work after the closing chain has already recorded `Finished`, so there is no root left to hold. That is not a violation of the contract — it is an effect the engine publishes outside any causal chain, which settlement therefore cannot describe. It is called out here so that the two are not conflated, and addressed under Consequences rather than by weakening the rule.

## Decision

**Settlement means every effect causally descended from the root is observable. This holds without exception, and cutting a causal edge is an explicit, visible act.**

Three requirements follow.

**1. A staged or deferred effect holds the chain of the work that caused it.** This restates ADR-0080 §12 and ADR-0165's staging paragraph as one rule covering mail and effects alike, and makes explicit that it binds regardless of which context object happens to be in scope at the staging site. Where the causing chain is known at staging time but absent from the immediate context — actor birth is the current instance, where `continue_from(owed, …)` threads the caller's debt to the spawn site while the activation context is rootless — the effect attaches to the causing chain, not to the context's root.

**2. A vacuous hold is not expressible.** `acquire_settlement_hold` must not return a value that presents as a hold while holding nothing. A site with no chain in scope must say so, and be read as saying so:

```rust
// Today — indistinguishable at the call site, and silently vacuous on NONE.
fn acquire_settlement_hold(&self) -> SettlementHold

// Required — the rootless case is a different value, and the caller must handle it.
fn acquire_settlement_hold(&self) -> Option<SettlementHold>
```

The exact spelling is an implementation choice; the requirement is that "there is no chain here" cannot be expressed by accident, and that a reviewer can see which one a site means. The naming should follow the same principle: a hold that may hold nothing should not be named as though it always holds.

**3. An effect emitted outside any chain is declared, not defaulted.** Where an effect genuinely has no causing chain — chassis-boot births, the close tail — that is a property of the effect worth stating at the emission site, in the same way `send_detached` states it for mail. An effect nobody can wait on is a legitimate thing to have and an illegitimate thing to produce silently.

The existing cases that do not hold remain correct and are not exceptions to this rule:

| case | why it does not hold | status |
| --- | --- | --- |
| `send_detached` | the causal edge is cut at the call site by a distinct verb | conforms — declared |
| an actor's post-`wire` activity | caused by later mail, not by the birth | conforms — not descended |
| a chassis-boot birth | no causing mail exists; the chain is empty | conforms — rule over an empty chain |
| an effect staged behind a retained reply debt | the chain cannot settle because the inbound's `Finished` is un-recorded while the debt is held | conforms — ordered by another device |
| a `wire`-staged registry effect on a runtime spawn | context carried `MailId::NONE` | **violates — silent** |

The first four cut causality visibly, have none to cut, or keep the chain open by another means. Only the last cuts it by accident.

The fourth row is the one an auditor is most likely to misread. `DesktopWindowCapabilityState` stages its window child from a rootless `PumpedSlot::host_turn` and acquires no hold, yet the ordering holds: `PendingCreate.reply` retains the `create_window` request's `InboundMail` and answers it only in `finish_window_child_spawn`, so the inbound never records `Finished` across the staged birth. A retained reply debt and a settlement hold are different devices reaching the same guarantee. Requirement 3 asks such a site to say which one it is relying on, because the reasoning is not visible from the staging call alone.

## Consequences

**The four flakes become one fix.** #4164, #4184, #4186 and #4192 were resolved test-side under the assumption that each was a local ordering mistake. Requirement 1 removes the cause for the birth-staged class, and the corresponding polls become removable rather than permanent. That is deliberate follow-on work, not part of this decision: the tests are green and the polls are harmless, so they are retired when the holds land, not before.

**`Settled` regains the exactness its non-idempotent consumers were promised.** ADR-0047's bundle-close is the consumer that forced the hold contract in the first place; requirement 1 restores the property it depends on.

**The rootless-emission case becomes visible instead of implicit.** Requirement 3 does not order the close tail — nothing can, since no chain exists at that point — but it makes the absence a stated property rather than a discovery. Whether such effects should instead carry a chain is a separate question this ADR does not decide; #4196 covers the consumer-side need for a settled-observation primitive in the meantime, and remains necessary regardless of what happens here.

**Holding is not waiting.** Requirement 1 is compatible with the activation suffix's constraint that it "must only submit or schedule work; it must never wait for owner application" (`crates/aether-substrate/src/actor/native/activation.rs:617`). `record_hold_open` is a counter increment under a short stripe lock; taking a hold does not block on the owner, and cannot participate in a cycle with it. The two constraints do not conflict.

**Migration cost is bounded and mechanical.** Requirement 2 changes a signature used at a small number of staging sites; each becomes a place where the author states which case applies. New staging sites inherit the obligation from the type rather than from having read this document.

**Diagnosis stops starting from a wrong prior.** Every instance of this class so far was first observed as a timing failure on an unrelated pull request, and triaged as a too-tight bound before the real mechanism was found. Requirement 2 does not make an existing violation a compile error — see the amendment — but it puts the question in front of the author of the next staging site, at that site's own signature, rather than leaving it to be discovered on a loaded runner months later.

## Alternatives considered

**Amend ADR-0165 in place.** Rejected: the rule spans ADR-0080's mail contract and ADR-0165's effect staging, and the rootless-emission case belongs to neither. Amending 0165 would leave the general statement filed under one of its instances.

**Leave the contract as prose and fix each site.** Rejected: the contract has been prose since May and has been violated by construction on at least one path the whole time, without detection. Prose did not prevent four instances; a signature that cannot express the mistake does.

**Make settlement cover "most" effects and document the exclusions.** Rejected: a barrier is only usable if a consumer can rely on it without enumerating what it omits. This is the option the current behaviour amounts to in practice, and it is what made four independent authors reach for a barrier that did not hold.

**Give the activation context the birth's root.** Rejected as the general fix, though it would resolve the current instance. The activation context is used both for effects completing the birth (which the caller should await) and for the actor's own startup sends (which it should not); one root cannot distinguish them, so attaching the causing chain to the *effect* rather than to the *context* is the narrower and more durable change.

**Add nextest retries so the class stops reddening CI.** Rejected: it suppresses the only signal that has surfaced this defect, and the flake corpus already contains real production bugs (#3730, #3797, and probably #4195) that a flaky test was the sole evidence for.

## Amendment (2026-07-31, iamacoffeepot/aether#4199)

Requirement 2 landed first, deliberately, as a signature change with no behaviour change. Implementing it corrected two claims made above.

**Requirement 2 does not turn an existing violation into a compile error.** The original text said the defect becomes "a compile-time question at the staging site." It does not. `NativeCtx` learns its root at runtime, so the forwarding surface — `dispatch_blocking_with`, `arm_deferred_completion`, `defer_reply_to` — can only propagate the `Option`, never answer it. The known violation at `wire_activation` produced no compiler error; it simply received `None` and carried on. The site inventory that followed came from hand-tracing roots, not from the compiler.

What requirement 2 does deliver is narrower and still worth the change: a hold that holds nothing cannot be constructed, since the acquire is `SettlementHold`'s sole constructor and answers `MailId::NONE` with `None`; every carrier's type now states that it may hold nothing; and the author of a *new* staging site meets the question at their own signature rather than inheriting a value that reads like a working hold. The claim to make for this requirement is unrepresentability, not detection.

**There are four rootless `A::wire` sites, not one.** The conforming table named "a chassis-boot birth" as though it were the only place `wire` runs without a chain. The four are `chassis/builder/native_actor_boot.rs`, `chassis/builder/driver.rs`, `actor/native/dispatcher_slot.rs` (`wire_activation`, the known violation), and `actor/native/spawn.rs`. Requirement 3 therefore covers four sites that must each state their case, not one.

Only `wire_activation` has a causing chain to thread — see the second amendment below, which corrects this paragraph's original claim that two of the others did.

Neither correction changes the decision. Both narrow what the first requirement was claimed to accomplish and widen the surface the second must cover.

## Second amendment (2026-07-31, iamacoffeepot/aether#4207)

Implementing requirement 1 corrected the first amendment's classification of the four rootless `A::wire` sites. The count of four is right; the claim that two of them are runtime spawns carrying a real causing chain is not.

Traced rather than inferred:

| site | reached from | causing chain |
| --- | --- | --- |
| `actor/native/dispatcher_slot.rs` (`wire_activation`) | the staged owner path, post-seal | **yes** — the chain that staged the birth |
| `actor/native/spawn.rs` | `commit_directly`, the **pre-seal** branch of `Spawner::commit`, whose only caller is `SpawnBuilder::finish()` on an embedder thread | no — no dispatch, no mail |
| `chassis/builder/driver.rs` (`assemble_pumped_slot`) | chassis boot, and an embedder's `boot_pumped_actor` | no |
| `chassis/builder/native_actor_boot.rs` | chassis boot | no |

Post-seal, `spawn.rs`'s entry point takes `commit_through_owner` and routes to `wire_activation` instead, so the chain-carrying case is already the first row. The live violation count is one, as the requirement-2 inventory recorded.

What the first amendment actually widened is the number of sites that must **declare** their case, which is requirement 3's scope, not requirement 1's. Requirement 1 threads one site; requirement 3 covers all four.

A related correction to requirement 1's framing: staging does not uniformly carry a chain. `HandlerSpawnBuilder::stage_with` receives `completion_root == MailId::NONE` when the calling context is a `host_turn` — the desktop-window case — which is why requirement 3's declaration belongs on the builder rather than being derived unconditionally from a root.

Both of this ADR's amendments correct claims about which code paths carry a chain, asserted without tracing them. The decision has held under implementation; its supporting factual claims needed the implementation to check them.
