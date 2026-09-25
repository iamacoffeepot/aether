# ADR-0226: Native Bundle Driver

- **Status:** Proposed
- **Date:** 2026-09-18
- **Amended:** 2026-09-19 — decision 9's reactor restart point is the higher of the reaction and activation watermarks; decision 11's `Processed` also waits for requests at or below `through` (issue #6208).
- **Amended:** 2026-09-19 — one `bundle` export generator and one root per bundle digest serving programs, reactors, or both (ADR-0225 decision 8).
- **Amended:** 2026-09-21 — chassis mounting lands in `aether-chassis-bloomery`: base stratum + component host + RPC server, with the journal owner and the driver spawned post-build as `aether.bloomery.journal:journal` / `aether.bloomery.driver:driver`, which widens the unauthenticated-writes consequence to any local process reaching a bound RPC port (issue #6244).
- **Amended:** 2026-09-23 — decision 10: the driver sends `WatchHead` on a fresh chain (issue #6401).
- **Amended:** 2026-09-23 — the bloomery's RPC listener binds only after the journal owner and the driver are mounted, so a reachable engine can take driver calls (issue #6399).
- **Amended:** 2026-09-24 — a bundle's fetch-on-miss travels invocation → bundle root → driver, and the driver forwards it to the journal owner with the reply pinned to the root; a bundle addresses no journal position (issue #6478).
- **Amended:** 2026-09-24 — the driver answers a bundle's fetch-on-miss itself, from a byte-bounded cache of found artifacts or one journal read shared by every fetch of that digest, and never caches a missing artifact; `SetHead` destination checks consult the same cache (issue #6258).
- **Amended:** 2026-09-25 — the chassis opens one journal root (the SQLite database plus its `blobs` directory, ADR-0220) rather than one journal file.
- **Amended:** 2026-09-25 — decision 10: `ReadClosure` answers with engine blob handles rather than inline bytes, and `ClosureLimit::MAX_BYTES` rises from 16 MiB to 4 GiB (ADR-0238 decision 10).

## Context

[ADR-0224](0224-programs-are-wasm-bundles.md) makes programs WASM bundles,
and ADR-0225 does the same for reactors. In both, a bundle root is loaded
once per engine under its digest, answers every request to its caller,
and never writes the journal. Both ADRs leave the native side to a driver
that "resolve[s] the head, assemble[s] the closure, load[s] the bundle,
invoke[s], record[s]". This ADR specifies that driver.

[ADR-0223](0223-journal-selected-reactor-set.md) describes a "native
feeder" that uses `LoadComponent` and `DropComponent`. In that design,
activation runs as the `core.cluster.activate` and `core.cluster.retire`
programs and yields `Activated` or `Rejected`. It also says policy
(admission, membership, moving back after a rejection) belongs in rules.
Nothing on main can carry any of this yet:

- **No caused writes.** The journal owner (`JournalActor`,
  `crates/aether-bloomery-journal/src/actor.rs`) has two writes, `MoveHead`
  and `Publish`. `write.rs` says "No mail carries a journal cause; every
  event these commands append is uncaused". `Publish` carries only
  `RecordedHeadMove`s. The store can write a cause
  (`Batch::push_event(event, cause)`, `insert_events`), but no mail reaches
  it. So ADR-0224 ¶7's "a `Publish` of the staged artifacts plus a
  `Transition`" can't be expressed today.
- **No closure read.** `verify_citations` checks each staged citation and
  then discards it. `Ref<K>` encodes exactly like `[u8; 32]`
  (`StorageLeaves for Ref<K>`), so citations can't be recovered from
  stored bytes.
- **No push.** Watching the journal is a polling loop over `read`
  ([ADR-0220](0220-the-journal-is-an-append-only-log-of-typed-events.md)).
- **No request or activation records.** `Transition` and `Fault` exist
  and are "written only by the driver" (`program/events.rs`,
  `program/fault.rs`). Neither has a request field. `Requested`,
  `Activated`, and `Rejected` don't exist. `FaultReason` is a closed set
  with no variant for load failures, oversized closures, or interrupted
  attempts.
- **Selection is all-or-nothing.** `select_reactors`
  (`crates/aether-bloomery-view/src/selection.rs`) fails the whole set
  with `MemberUnbound` when one member is unbound, and fails with
  `SetUnbound` at genesis. `Heads::get` (`heads.rs`) is public, and
  `ReactorSet::clusters` lists member heads without merging equal
  digests.
- **Traps kill the process.** A WASM trap calls `ctx.fatal_abort`
  ([ADR-0063](0063-fail-fast-on-abnormal-component-lifecycle.md);
  `crates/aether-component/src/trampoline/runtime/mod.rs`, the
  `component.deliver` error arm) and kills the whole substrate.

## Decision

1. **One native driver, the base case.** `aether.bloomery.driver` is a
   native actor. Native code spawns it; chassis mounting comes later. It
   is the system's fixed base case, so no head selects it. It manages
   bundle roots by sending `LoadComponent` to `aether.component`. It is
   not their parent. There is one driver per engine for now. Reactor
   roots are shared by digest, and `Invoke.seq` is unique only within one
   journal, so two drivers would collide. The driver has two roles that
   share one table of loaded bundles:
   - it loads and invokes programs on demand;
   - it follows the reactor set.

   Chassis mounting has since landed in `aether-chassis-bloomery` (issue #6244):
   the chassis composes the shared base stratum plus `ComponentHostCapability`
   and the RPC server, then spawns the journal owner and the driver post-build
   over one journal root (ADR-0220), then binds the RPC listener (issue #6399). No
   full-stack cap rides the engine.

2. **Roots are named by digest and never dropped.** A root's `name` is the
   lowercase-hex `artifact_digest(OpaqueBytes::ID, wasm)`, as in ADR-0224
   §5 and ADR-0225 §1. The driver never sends `DropComponent`, because the
   name stays registered and a dropped digest could never be loaded
   again. Retiring an instance means the driver stops routing to it.
   Dormant instances stay loaded. A digest has one root, which serves
   every role its bundle declares (ADR-0225 decision 8). The driver
   loads the digest the first time either role needs it, and the other
   role reuses that root. A role the bundle doesn't declare is refused
   from the missing custom section before any load: a program request
   faults with `BundleUnavailable`, and an activation is rejected.

3. **Programs.** For each request, the driver:
   1. resolves the program head (see decisions 7 and 11);
   2. records `Requested`, which pins the digest in its `ProgramRef`;
   3. reads the bundle's `aether.bloomery.programs` section and checks
      the name and input kind;
   4. reads the input's transitive closure under a byte cap
      (`ReadClosure`);
   5. loads the bundle if it isn't loaded yet;
   6. sends `Invoke { seq, … }`, where `seq` is the `Requested` seq;
   7. records a `Transition` (with the staged artifacts in the same
      append) or a `Fault`, caused by the `Requested` seq.

   Before recording a `Transition`, the driver checks the result's prefix
   against the declaration. `Transition` and `Fault` gain no request
   field; the entry's `cause` is the link. Only one `Invoke` is in flight
   per root. Other requests for that digest wait in a queue.

4. **The driver assigns four more fault reasons.** `FaultReason` gains:
   - `ClosureTooLarge { limit_bytes }`: the closure exceeded the cap.
     Nothing was loaded.
   - `BundleUnavailable { reason: Detail }`: a failure before `Invoke`.
     This covers a load error, an unreadable section, an unknown program
     name, or the wrong input kind.
   - `ProtocolViolation { reason: Detail }`: a failure after `Invoke`.
     This covers `Invoked::Rejected` and a result whose prefix doesn't
     match the declaration.
   - `Interrupted`: the request was outstanding when the engine stopped
     (decision 9).

   A missing closure member is `InputMissing`. `Invoked::Refused` maps to
   its `Refusal`. This amends ADR-0220 fault rule 3's closed set.

5. **Following the reactor set.** The driver processes the journal one
   seq at a time. For event `N` it:
   1. selects members from prefix `N−1`, the ADR-0223 boundary;
   2. delivers `Event` once to each distinct selected digest;
   3. waits for every reply;
   4. appends that seq's records in one batch.

   Membership comes from the `ReactorSet` artifact plus `Heads::get` for
   each member, not from `select_reactors`. An unbound member is skipped
   and doesn't fail the set. An unbound or empty set selects nothing and
   is a valid state. This covers the old W6 genesis item.

   When a member head's selection changes at `N`, the predecessor still
   evaluates `N`. The successor is activated before `N+1`: the driver
   loads it and sends fold-only `Warm` batches through `live_from − 1`,
   then records `Activated { head, bundle, live_from }`, caused by `N`.
   If activation fails, the driver records `ActivationRejected`. The
   rejected interval is owed to the next successful activation of that
   head, and that activation's `live_from` is the first owed seq. The
   interval is never skipped. A shared instance that has already gone
   live past the owed start can't take it, so that activation is
   rejected as well.

6. **Reactor intents.** A rule may return exactly two intent kinds. Both
   are caused by the trigger seq `N`:
   - `CallProgram { program: Head<OpaqueBytes>, name: ProgramName, input: Digest }`.
     The driver records `Requested` with source
     `Reaction { bundle, reactor, rule, ordinal }`. `ordinal` is the
     intent's position within that rule's output for `N`.
   - `SetHead { head, from: Option<Ref>, to: Ref }`, kind
     `aether.bloomery.driver.set_head`. It is named apart from the
     journal's `MoveHead` command. The driver records a head move. `from`
     is a compare-and-swap. If the head's binding
     when the driver appends isn't `from`, the move is refused and
     recorded as `ReactionFailed`. A `to` that the journal doesn't hold
     under the head's kind is refused the same way.

   Any other intent kind is refused and recorded as `ReactionFailed`. A
   refusal applies to that one intent. The bundle's other intents for `N`
   still stand, because the refusal happens when the driver applies the
   intent, not while the reactor evaluates. A
   reactor may move any head. Restricting that is deferred. Moves are how
   ADR-0223's policy-in-rules is expressed: admission, membership, and
   moving back after a rejection. Reactors never stage artifacts. Only
   programs create content.

7. **Intent heads resolve through the trigger, inclusive.** The driver
   resolves a `CallProgram` head from `Heads` folded through `N`, the
   trigger's own seq. Four facts decide this:
   - A rule sees views folded through `N`, including the trigger.
     `Owner::prepare` catches views up to the last retained entry, which
     is the trigger. The `aether-bloomery-reactor` crate doctest asserts
     that `heads.cursor()` equals the trigger's seq and that the result
     includes the trigger's own move.
   - `Heads` is a pure fold. Resolving through `N` therefore gives what
     the rule's own `heads.get(head)` returns. Any other point gives a
     different answer whenever the head moved in between, which breaks
     "one valid way".
   - "Latest at write time" isn't deterministic on replay. After a crash
     between evaluation and append, the intent is derived again at a
     different journal position.
   - `N−1` fails when the trigger is itself the program's head move,
     because the rule would call the version being replaced.

   Selection's `N−1` answers a different question: which bundles
   evaluate `N`. That has to be decided before `N` is known, so the two
   rules don't conflict.

8. **Recorded kinds.** These are stored kinds, and only the driver
   writes them:

   | Kind | Shape | Cause |
   | --- | --- | --- |
   | `bloomery.requested` | `Requested { program: ProgramRef, input, source: RequestSource }` | trigger seq for `Reaction { bundle, reactor, rule, ordinal }`; none for `Native { origin: NativeOrigin, key }` |
   | `bloomery.activated` | `Activated { head, bundle, live_from }` | boundary seq |
   | `bloomery.activation_rejected` | `ActivationRejected { head, bundle, reason: Detail }` | boundary seq |
   | `bloomery.reaction_failed` | `ReactionFailed { bundle, reactor: Option<ReactorName>, reason: Detail }` | trigger seq |

   The dedup key is `(cause, source)`. The driver enforces it as the only
   fenced writer of these kinds. There is no retirement record, because
   retirement is derived from the set and head folds. Replies map to
   records as follows:

   | Reply | Recorded |
   | --- | --- |
   | `Evaluated::Completed` | one `Requested` per `CallProgram`, one head move per `SetHead` |
   | `Evaluated::Failed { reactor }` | `ReactionFailed { reactor: Some }`; no intents from that bundle for that seq |
   | `Evaluated::Poisoned` | `ReactionFailed { reactor: None }`, plus `ActivationRejected` for every head the digest serves |
   | `Warmed::Poisoned` / `Warmed::OutOfSequence` during activation | `ActivationRejected` |
   | live `Evaluated::OutOfSequence` | nothing; this is a driver bug, and the driver resyncs with `StatusQuery` |

9. **Restart.** The driver rebuilds everything from folds.
   - **Programs.** Any `Requested` with no `Transition` or `Fault` is
     recorded as `Fault { Interrupted }` and isn't run again. Whether to
     retry is up to the graph: a reactor rule can issue a new request
     (ADR-0220: "A retry is a new event"). The reason is that a trap
     kills the substrate. Re-running a trapping program automatically
     would crash-loop the engine.
   - **Reactors.** After a restart, every root is gone. Restart
     replays from max(reaction watermark, activation watermark). The
     reaction watermark is the highest cause among reaction-sourced
     records: `Requested` with a `Reaction` source, caused head moves,
     and `ReactionFailed`. The activation watermark is the highest
     cause among `Activated` and `ActivationRejected` records: an
     activation-only batch still moves the restart fence. The driver
     loads the instances the current set selects. It warms each one
     fold-only from seq 1 through the watermark, then evaluates live
     after it. A seq's records commit in one batch, so no seq is
     half-recorded. Per-digest failures during the warm reject every
     head the digest serves at the watermark cause, without stopping
     the restart.
   - **Adopting a loaded root.** `StatusQuery` is only for a root that
     is already loaded, meaning `LoadComponent` failed with
     `SubnameInUse`. The driver resumes from that root's cursor.
   - **Deferred: the reactor poison pill.** A reactor that traps at seq
     `k` traps again on every restart. The fix is a record written before
     delivery that raises the watermark past `k`.

10. **Journal commands.** The journal owner gains three commands:
    - `AppendRecords`. It carries driver records and caused head moves,
      plus staged artifacts, fenced on `expected_seq`. Each cause must be
      within `1..=expected_seq`. It is all-or-nothing, like every append.
    - `WatchHead { after }`, answered by `HeadAdvanced { head }` once the
      head passes `after`. This is a long poll. The driver sends the watch
      on a fresh chain (`send_detached_with_context`, ADR-0080 §7), because a parked watch holds its chain open and the
      chain that re-arms it did not cause it.
    - `ReadClosure { root, limit_bytes }`. It walks a stored table of
      citation edges and answers with the artifacts, a missing digest, or
      "too large". The walk never truncates. The table is created if it
      doesn't exist and is filled when a staged artifact is inserted.

    `AppendRecords` is what makes ADR-0224 ¶7 true.

11. **Driver mail.**
    - `Call { program: Head<OpaqueBytes>, name, input, origin: NativeOrigin, key }`
      gets exactly one `CallOutcome`: `Transition`, `Fault`, or
      `Refused`. The driver resolves the head at the journal head when it
      records `Requested`. `Refused` means the head is unbound, and
      nothing is recorded. A repeated `(origin, key)` with the same
      program, name, and input is answered from the first request's
      recorded outcome. A repeated `(origin, key)` for a different request
      is a caller bug: it is answered `Refused`, and nothing is recorded.
    - `AwaitProcessed { through }` gets `Processed` once its bound is
      quiescent: routing has passed `through`, no routing write is
      queued or in flight, and no request at or below `through` is
      still outstanding.

    The driver pulls events with `ReadEvents` and uses `WatchHead` to
    wake. Its outbound mail is `LoadComponent`, the root protocols of
    ADR-0224 and ADR-0225, and the journal commands.

## Consequences

- **Deduplication is complete, but the driver is a serialization
  point.** A single fenced writer makes dedup complete. Per-seq lockstep
  means one slow reactor holds up every later seq, and one `Invoke` per
  root serializes each bundle's programs.
- **Dormant instances hold memory.** They stay loaded for the engine's
  lifetime. A poisoned digest stays poisoned for reactor routing until
  restart, so moving a reactor-set head back to it is rejected. Its
  programs keep answering.
- **The poison pill is deferred.** A trapping reactor crash-loops the
  engine on restart until the pre-delivery watermark record exists.
  Programs don't crash-loop, because they are faulted `Interrupted`.
- **Journal writes are not authenticated.** Any engine-local mailer can
  send `AppendRecords` or write kinds meant only for the driver. Since
  chassis mounting (issue #6244), the journal owner and the driver are
  RPC-addressable on the bloomery engine, so any local process that can
  reach a bound RPC port can do the same. Authentication is deferred.
- **History growth.** The dedup index and the request, activation, and
  head folds grow with history. Restart refolds from seq 1 and warms
  every selected instance serially. Checkpoints and parallel warmup are
  deferred.
- **One driver per engine.** Several journals per engine are deferred.
- **Head moves are unrestricted.** Any selected reactor can move any
  head, including the reactor-set root. Ownership rules are deferred as
  unneeded for now.
- **Closures follow only `Ref<K>`.** Content reached only through a bare
  `Digest` field isn't part of a closure. Artifacts stored before the edge
  table existed have no edges. No deployed journal holds any.
- **New crate.** The driver lives in a new native crate. Its kinds live
  in `aether-bloomery-kinds` and its folds in `aether-bloomery-view`.
  Chassis mounting landed as the `aether-chassis-bloomery` crate (issue
  #6244): base stratum + component host + RPC server, with the journal
  owner and the driver spawned post-build as
  `aether.bloomery.journal:journal` / `aether.bloomery.driver:driver`,
  then binds the RPC listener (issue #6399). A bundle's fetch-on-miss
  travels invocation → bundle root → driver, and the root relays the
  answer to the invocation, so bundles address no journal position
  (issue #6478). The driver answers the fetch itself, from a 64 MiB cache
  of found artifacts evicted least recently used first, or from one
  journal read shared by every fetch of that digest; a missing artifact
  is never cached, because it can be stored later (issue #6258).
- **Amendments.** Following ADR-0224's precedent, the older ADRs stay
  unedited. This ADR amends:
  - ADR-0223: the feeder becomes this driver; `DropComponent` is never
    used; the `core.cluster.activate` and `core.cluster.retire` programs
    are dropped; `Activated` and `Rejected` become the native records
    `bloomery.activated` and `bloomery.activation_rejected`; native
    requests are uncaused and keyed by `(origin, key)`.
  - ADR-0224: in ¶1, heads in reactor intents resolve through the
    trigger inclusive; in ¶7, outcomes are recorded through
    `AppendRecords`; and a trap ends the process, not just the
    in-flight invocations (as ADR-0225 also notes).
  - ADR-0220: fault rule 3's closed variant set grows; driver writes
    carry causes; a citation-edge table is stored; `WatchHead` replaces
    polling for the driver; the deferred idempotency keys arrive as
    `(cause, source)`.

## Alternatives considered

- **Two actors (a program loader and a reactor manager).** Rejected. They
  would need a shared digest table, and two loaders would collide on
  `SubnameInUse`. Two fenced writers would race on the fence and split
  dedup. Each intent would take an extra hop before becoming a request.
- **A WASM driver.** Rejected. The driver is the base case, so no head
  can select it anyway. A guest can't attest journal records. Bundle
  bytes would pass through guest memory.
- **Activation as programs (`core.cluster.activate` / `retire`).**
  Rejected. Activation is native lifecycle work with no content result,
  and a program can't load components.
- **The driver walks closures by decoding kinds, with no stored edges.**
  Rejected. `Ref<K>` is indistinguishable from `[u8; 32]` in stored
  bytes, so the driver would need a kind registry.
- **Re-invoke outstanding programs on restart.** Rejected. A trapping
  program would crash-loop the engine. Retry belongs to the graph.
- **Drop an instance when it retires.** Rejected. `DropComponent` keeps
  the name, so that digest could never be loaded again.
- **Resolve intent heads at `N−1` or at write time.** Rejected for the
  reasons in decision 7.
- **Restrict which heads a reactor may move.** Deferred. There is no
  current need, and the compare-and-swap already prevents lost updates.
