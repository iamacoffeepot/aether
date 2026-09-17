# Kernel reactor work orders

- **Status:** Owner approved for implementation; individual PRs require review.
- **Date:** 2026-09-17
- **Grounding:** `78493530dc4bbe2cc0c434516c345dc9cf7a319a`
- **Design:** [Kernel reactor](bloomery-kernel-reactor.md)
- **Decision:** [ADR-0223](../adr/0223-journal-selected-kernel-reactor.md)

These owner-approved work orders are transferred into bounded issue Plans
before implementation. Refresh main and reconcile source paths at dispatch.
Keep one concept per PR. The owner authorized implementation after reviewing
this packet; merging still requires review and separate authorization.

## Sequence and execution

| Order | Deliverable | Prerequisites | Proposed executor |
| --- | --- | --- | --- |
| 1 | Transactional component replacement | Reviewed lifecycle design | Sol |
| 2 | Bootstrap lifecycle prohibition flags | Reviewed policy design | Grok, if allowance remains |
| 3 | Generated reactor peer restoration | Existing export/lifecycle contract | Grok, if allowance remains |
| 4 | Journal-selected kernel integration | Orders 1–3 and reviewed integration contract | Sol |

Spend Grok's remaining one or two orders on the bounded flags and restoration
changes. Use Sol for remaining work, especially the cross-cutting replacement
transaction. Orders 1 and 2 overlap in trampoline code: serialize those edits
or land/rebase one before the other. Order 3 can proceed independently.

All workers receive the agreed contract and source anchors. They should not
follow the implement skill mechanically where the owner's instructions override
it. Use light direct review, focused behavioral tests, draft PRs, and GitHub CI.
Work locally while Eve is offline. Avoid full local workspace builds unless
needed for a focused reproduction. Keep durable work in issue-specific worktrees.
The owner reviews PRs before landing; this packet grants no merge permission.

## Order 1 — Restore transactional component replacement

**Problem.** Replacement currently destroys the predecessor before all
candidate failure points have passed. An error can leave an empty slot or
a failed successor. Retaining the object alone also fails to undo hook effects.

**Result.** Every rejected replacement preserves the predecessor's usable state,
peer identities, routing, restrictions, and published metadata. No rejected
candidate or preparation hook publishes partial lifecycle effects.

**Owned surfaces, to narrow when filed:**

- `crates/aether-component/src/trampoline/runtime/replace.rs`
- `crates/aether-component/src/trampoline/runtime/state.rs`
- `crates/aether-component/src/component/runtime/load.rs`
- `crates/aether-substrate/src/actor/wasm/component/**`
- `crates/aether-component/tests/**`
- Relevant existing WASM fixtures under `crates/aether-substrate/tests/**`
- `docs/adr/0016-persistent-state-across-hot-reload.md`
- `docs/guide/operating/components/replacement-failure-states.md`

**Implementation steps.**

1. Reproduce current failure behavior with a guest that reports its build and
   state through a side-effect-free query. Cover candidate init and rehydrate
   failures. Trace every externally visible operation reachable during
   unwire, dehydration, init, and rehydration.
2. Before changing production behavior, record the transaction mechanics:
   how predecessor state remains recoverable, what candidate effects are
   staged, when aliases/metadata publish, and which operations commit.
   Include guest-memory mutation and host calls; an outbound-mail buffer alone
   is not a full rollback strategy.
3. Implement the preparation/commit boundary using the existing serialized
   trampoline handler. Do not destroy or irreversibly retire the predecessor
   before candidate acceptance. Discard speculative effects on rejection.
4. Publish the new module, type tag, artifact bytes, capability metadata,
   aliases, and active instance consistently. Preserve existing boot-reference
   bookkeeping and reply correlation.
5. Define teardown-trap behavior explicitly and update the lifecycle docs to
   match. Do not report success while silently losing required migration state.
6. Replace the old failure-state guide with the actual new contract and any
   explicit limits. Preserve a note explaining the historical regression.

**Tests / acceptance.**

- Reject invalid WASM, invalid export, save-state failure, init trap, and
  rehydrate trap; query predecessor build, state, and child state afterward.
- Adversarial migration hooks mutate state and attempt outbound effects.
  Rejection leaves the observable application unchanged.
- Candidate mail, aliases, spawns, and capability changes do not escape rejection.
- Successful replacement retains stable addresses and transfers state once.
- Existing queued-message and tracked-reply behavior remains intact.
- Ordinary replacement of an intentionally empty, unprotected slot still works.

**Boundary.** Repair the existing read-only, fallible dehydration contract;
do not add a second snapshot hook or FFI. Any host-call semantics outside the
declared surface require a concrete plan amendment. Do not replace the agreed
invariant with best-effort rollback. This order is large and needs a
transaction-mechanics checkpoint before its final implementation issue surface
can be frozen.

**Search:** `rg -n 'handle_replace|on_dehydrate|call_on_rehydrate|take_save_error|pending_alias|pending_spawn' crates/aether-component crates/aether-substrate/src/actor/wasm crates/aether-actor/src/wasm`

## Order 2 — Component lifecycle prohibition flags

**Problem.** Any normal component may currently be individually dropped or
replaced. Required components need host-enforced lifecycle restrictions.

**Result.** Native bootstrap accepts bitwise prohibition flags. The kernel can
prohibit drop while permitting replacement. Policy survives guest replacement.

**Owned surfaces, to narrow when filed:**

- `crates/aether-component/src/trampoline/**`
- `crates/aether-component/src/component/runtime/**`
- `crates/aether-component/src/lib.rs`
- `crates/aether-component/tests/**`
- `docs/guide/operating/component-registry.md`

**Implementation steps.**

1. Define a flags type with `DROP`, `REPLACE`, and empty/default. Reuse the
   repository's flags conventions; expose `prohibit` on trusted bootstrap.
2. Carry it from `WasmTrampolineConfig` into stable host state. Audit every
   config constructor, including sibling spawn and host load.
3. Reject prohibited operations at their actual handlers before hooks or state
   mutation. A request sent directly to the trampoline cannot bypass the check.
4. Preserve policy on replacement; do not add a guest-controlled setter or
   infer policy from a replacement manifest.
5. Keep whole-application shutdown operational. Document that individual
   components do not obtain a shutdown bypass.
6. Keep normal loading backward-compatible with empty restrictions. Do not
   add load-mail fields or positional wire changes unless the bootstrap
   integration proves they are needed and the Plan is amended first.

**Tests / acceptance.**

- Empty flags preserve ordinary drop/replace.
- `DROP` refuses direct and host-forwarded drop before any guest hook runs.
- `REPLACE` refuses replacement; the combined flags refuse both.
- Allowed replacement preserves flags; failed replacement cannot clear them.
- Protected guest still answers after rejection; application shutdown completes.
- A separately spawned sibling receives its own declared policy.

**Boundary.** Lifecycle restrictions are not general authorization, journal
governance, or protection against terminating the application process.

**Search:** `rg -n 'WasmTrampolineConfig|on_drop_component|on_replace_component|handle_replace' crates`

## Order 3 — Restore generated reactor peers during replacement

**Problem.** The reactor generator omits hidden peer types from final exports,
but inline-child reconstruction dispatches over that exported type list.

**Result.** Generated reactor peers are public exports for debugging and other
consumers, and keep their identities through replacement. Private actors must
also remain discoverable by the reconstruction machinery; restoration cannot
depend solely on the public export list.

**Owned surfaces, to narrow when filed:**

- `crates/aether-bloomery-reactor-derive/src/bundle.rs`
- `crates/aether-actor-derive/src/export_emit.rs`
- `crates/aether-actor/src/wasm/mod.rs`
- `crates/aether-actor/src/wasm/inline/compose.rs` only if necessary
- `crates/aether-substrate/tests/reactor_bundle.rs`
- Existing reactor fixture and macro tests, located before filing

**Implementation steps.**

1. Add a focused failing replacement scenario to the real generated-bundle
   fixture. Show that initial execution works and post-replacement peer
   reconstruction is missing.
2. Include private actor factories in the reconstruction inventory and test
   private-child replacement independently of reactor exports. Preserve inline
   placement restrictions, ordinary actor exports, coordinator default
   selection, and generator metadata for later generators.
3. Export generated reactor peers publicly as the owner requested. Public
   visibility is useful independently of restoration and must not be the fix
   for private-actor reconstruction.
4. Restore through the existing child composition path; do not run `wire`
   again indiscriminately.
5. Verify view state/cursor behavior. If views are intentionally rebuilt,
   perform explicit fold-only warmup before the next live event. Do not claim
   that restoring peers also serializes their coordinator's aggregation state.

**Tests / acceptance.**

- Replace an actual generated reactor bundle, then fold/execute a subsequent
  event and observe every expected peer response exactly once.
- Preserve peer alias and parent identity; no duplicate child registration.
- Check mixed ordinary-actor/reactor exports and downstream generator metadata.
- Exercise repeated replacement; no silent missing-peer success.
- Keep ordinary inline-child replacement tests green.
- Restore a private inline child even when it is absent from public exports.

**Boundary.** No new bundle authoring macro, no manual user registration, and
no unrelated actor lifecycle redesign.

**Search:** `rg -n 'rewritten_exports|reconstruct_child|reconstruct_inline_children|spawn_peers|__export_emit_classified' crates`

## Order 4 — Integrate the journal-selected kernel

**Problem.** Reactor authoring and event preparation exist, but the kernel
bundle, bootstrap, and lifecycle-output adapter are not connected.

**Result.** A pinned kernel starts with drop prohibited; its rules observe
journal configuration, request native lifecycle work, and expose outcomes as
events. Event execution selects the historical set and head bindings.

This is an integration plan to split into smaller implementation issues after
the prerequisite contracts are verified. It is not one large coding dispatch.

**Proposed slices.**

1. **Shared data and selection.** Introduce the active-set kind in
   `aether-bloomery-kinds`; use existing head/artifact codecs and macros.
   Implement and test prefix-based recipient selection without a duplicate
   activation log. Cover membership changes and several moves in one batch.
2. **Kernel bundle.** Add a logically separate WASM bundle using existing reactor
   signatures, named guards, bundled views, and `export!` generation. Its
   outputs are typed lifecycle intents, not native policy duplicated in WASM.
3. **Native adapter and bootstrap.** Store/load ordinary pinned kernel bytes,
   instantiate through shared component machinery with `prohibit: DROP`,
   translate lifecycle intents and results, and record observable outcome
   events. Select exact artifact bytes rather than mutable hub names.
4. **End-to-end handoff.** Tie selection, fold-only warmup, replacement results,
   activation correlation, and next-event delivery together. Demonstrate
   rejection with a still-usable predecessor and unchanged head. Record a
   successful head move only after the replacement actually succeeds.

**Mandatory integration proof before dispatching slice 4.**

Events record completed facts; intents request work. Show replacement success
followed by its head move, and replacement failure followed by rejection with
the predecessor head unchanged. The former scenario of publishing a candidate
head before attempting replacement is withdrawn. No failed-candidate stream
or corrective head move is needed for that scenario.

Reconcile recipient selection for the successful head-change observation:
the predecessor may already be gone when the event is recorded. Withdraw the
unconditional claim that it handles that event. Prove the chosen recipient and
warmup boundary before native delivery implementation, without engine changes,
activation gates, or a second activation history. Show how the kernel remains
part of the effective configuration; drop prohibition alone does not validate
reactor-set membership.

**Tests / acceptance.**

- Fresh journal boots the pinned bytes without another reactor being available.
- Replay selects the reactor set and bundle bindings at each event boundary.
- Successful head changes are recorded after lifecycle success; failed
  replacements leave the predecessor head unchanged.
- The recipient and warmup boundary for the successful head-change observation
  agree with the completed replacement and historical selection.
- Fold-only warmup causes no historical lifecycle effects.
- A-to-B-to-A rejects stale readiness replies.
- Identical bytes under distinct cluster heads produce distinct view state.
- Replacement rejection is observable and leaves the predecessor usable.
- Kernel drop is rejected; normal application shutdown succeeds.

**Boundary.** No program executor, full crash recovery, general governors,
memory64, or zero-copy transport. Freeze concrete crate/file ownership for
each slice before issue approval; do not dispatch this umbrella wholesale.

## Review and handoff

The owner reviewed the ADR, companion, and work orders and authorized the
implementation goal. File bounded issues and track execution there. Source
findings above still require the specified runtime reproductions. This
document records the plan, not live worker or CI status.
