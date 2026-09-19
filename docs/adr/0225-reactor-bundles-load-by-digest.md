# ADR-0225: Reactor Bundles Load by Digest and Answer Their Caller

- **Status:** Proposed
- **Date:** 2026-09-18
- **Amended:** 2026-09-19 — one `bundle` export generator and one root per bundle digest serving programs, reactors, or both (decision 8).

## Context

[ADR-0222](0222-reactor-bundles-own-their-views.md) puts reactor code and
view folds in one WASM bundle. It says a views actor hands owned prepared
data to reactor peers through typed mail.
[ADR-0223](0223-journal-selected-reactor-set.md) selects bundles by cluster
head and says each running version is "identified by its cluster head and
artifact digest". It also says a dormant instance "may be dropped" and
that activation runs through the `core.cluster.activate` and
`core.cluster.retire` programs.

The code on main follows that shape. `bundle_reactors`
(`crates/aether-bloomery-reactor-derive/src/bundle.rs`) generates a
coordinator at `aether.bloomery.reactor`. It takes a `ClusterConfig { output, ack }`
of runtime mailbox names and spawns one persistent inline peer per reactor,
named `r_<fnv64(NAMESPACE)>`. The coordinator binds a caller-supplied
stream token (`Cluster::check_admit`, `StreamMismatch`). On a live `Event`
it folds the views, encodes snapshots with `aether_bloomery_view::Publish`
into a `PreparedPrefix`, and mails that prefix to every peer. Peers send
outputs with `send_to_named(output)` and report `PeerEvaluated` to their
parent, and the coordinator acks with `send_to_named(ack)`. `EventBatch` is
fold-only warmup. `Intent` (`crates/aether-bloomery-reactor/src/evaluate.rs`)
carries a kind name and bytes, but no reactor or rule.

[ADR-0224](0224-programs-are-wasm-bundles.md) settled programs: one
generated root per bundle, loaded once per engine under its digest, a reply
to every request, and only native code writes the journal. Reactors should
use the same loading and reply model. Several parts of the current reactor
shape don't fit it. The configured mailboxes assume one fixed consumer, and
the stream token ties an instance to one caller. Snapshot mail re-encodes
every view on every event, even though every reactor runs in the same WASM
instance.

A second native actor, the driver, loads instances and routes events to
them. It is specified separately in ADR-0226. This ADR covers only the
bundle side of that boundary.

## Decision

1. **One root per bundle, named by digest.** The `bundle` generator
   generates one root actor at export namespace `aether.bloomery.bundle`
   (decision 8). The driver
   loads it once per engine with `aether.component.load` (`LoadComponent`).
   `name` is the lowercase-hex `Digest` of
   `artifact_digest(OpaqueBytes::ID, wasm)`. That is the journal artifact
   digest, a sha256 over the artifact prefix plus the payload. It is not
   the raw sha256 of the WASM bytes. Reactor-set members are
   `Head<OpaqueBytes>`, so this is the digest a head resolves to. `config` is
   empty, and `export` is `Some("aether.bloomery.bundle")`. The root's
   address is `aether.component/aether.embedded:<digest>`.

2. **The root is one actor.** It owns the views and calls each reactor's
   `evaluate` directly. There are no per-reactor peer actors, no per-event
   children, and no snapshot mail inside the bundle. Reactors differ from
   programs here. Reactor evaluation is stateless and finishes inside one
   handler, so a separate actor would own nothing. An inline child's
   dispatch would drain in the same `receive` call. It would gain no
   isolation or concurrency, and it would add spawn, alias, and despawn
   round trips on every event. Program invocations keep per-seq inline
   children (ADR-0224 §5, #6180). The reason is that asynchronous `run`
   will hold suspended state, and that state needs its own actor and reply
   address.

3. **Identity is the digest only.** An instance has no stream token and no
   head binding. Heads that name the same digest share one instance, and
   each event is evaluated once per digest. Only the driver assigns event
   intervals. It delivers entries to an instance contiguously from seq 1:
   first fold-only `Warm` batches, then live `Event`s. The root rejects any
   entry out of sequence. This amends ADR-0223's "identified by its cluster
   head and artifact digest".

4. **Every request is answered to its caller.** The root answers every
   request with `ctx.reply`. It has no `send_to_named`, no configured
   output or ack mailbox, and it never writes the journal. A request with
   no reply target is ignored and logged. Events arrive as `JournalEntry`
   envelopes (`crates/aether-bloomery-kinds/src/journal/mod.rs`), never as
   mail of the stored kind: storage `KindId`s are nominal
   (`crates/aether-data/src/hash.rs`, `storage_kind_id_from_name`), while
   mail `KindId`s hash the canonical schema. Intents come back as
   `ReactorIntent { reactor: ReactorName, rule: RuleName, kind, bytes }`,
   where `kind` is the output's mail `KindId` and `bytes` its mail-codec
   encoding. Evaluation is all-or-nothing per event. If any reactor fails,
   the event yields no intents, and the views stay advanced past it.

5. **Protocol kinds.** These kinds live in `aether-bloomery-kinds` under
   `reactor/`, next to the program `Invoke` / `Invoked` mail:

   | Request | Reply |
   | --- | --- |
   | `aether.bloomery.reactor.warm` `Warm { entries }`, first seq = cursor + 1 | `aether.bloomery.reactor.warmed` `Warmed`: `Folded { through }` \| `OutOfSequence { first, expected }` \| `Poisoned { last_trusted, reason }` |
   | `aether.bloomery.reactor.event` `Event { entry }`, seq = cursor + 1 | `aether.bloomery.reactor.evaluated` `Evaluated`: `Completed { seq, intents }` \| `OutOfSequence { seq, expected }` \| `Poisoned { seq, last_trusted, reason }` \| `Failed { seq, reactor, reason }` |
   | `aether.bloomery.reactor.status_query` `StatusQuery` | `aether.bloomery.reactor.status` `Status { cursor, poisoned }` |

   A failed fold poisons the instance for the rest of the engine's
   lifetime. `Failed` means a reactor's evaluation failed: the views
   advanced, and the event produced no intents. Mail schemas that a bundle
   sees are fixed for a running engine. The mail registry refuses a second
   schema under an existing kind name
   (`crates/aether-substrate/src/mail/registry/errors.rs`,
   [ADR-0010](0010-runtime-component-loading.md)). A bundle built against
   a changed protocol schema can't load beside older ones.

6. **The declaration lives in the bundle.** The `bundle` generator writes an
   `aether.bloomery.reactors` custom section. For each reactor it records
   the reactor name, and for each rule the rule name, the trigger kind, and
   the output kind. The layout follows #6180's `aether.bloomery.programs`.
   The driver reads the section from the artifact bytes before loading.
   View and guard dependencies are not recorded.

7. **A reactor's `NAMESPACE` is its identity inside the bundle.** It is
   validated as a `ReactorName` at compile time. Rule names are validated
   as `RuleName`s the same way.

8. **One generator, one root, programs and reactors together.** A bundle
   provides programs, reactors, or both. `#[program]` and `#[reactor]`
   still validate and tag their types, and one export generator,
   `aether_bloomery_bundle::bundle`, replaces `bundle_programs` and
   `bundle_reactors`. It reads both tags from the `export!` set and
   generates the single root at `aether.bloomery.bundle`, with the
   program handlers (`Invoke`, `Invoked`) when the bundle has programs
   and the reactor handlers (`Warm`, `Event`, `StatusQuery`) when it has
   reactors. It writes each present role's custom section. A module with
   neither is a compile error. The handled kinds don't overlap, so each
   handler answers its own caller and nothing routes between roles. The
   program root's state moves into a library type beside
   `aether_bloomery_reactor::Root`, so the generated root stays a thin
   shell. Each role keeps its own state inside the root, so a poisoned
   reactor stays poisoned in the reactor state and the bundle's programs
   keep answering.

9. **Retiring an instance only stops routing to it.** The driver stops
   routing events to a retired instance. The instance is never dropped,
   because `DropComponent` keeps the name and a reload under the same
   digest would fail with `SubnameInUse`. Loading, warmup, routing,
   activation records, and recovery belong to the driver (ADR-0226).

## Consequences

- Reactors and programs load the same way: once per engine, under the
  journal artifact digest, with no configuration. Each answers its caller,
  and native code does all journal writes. A bundle that provides both
  loads once and has one address.
- In a bundle that provides both roles, program invocations and reactor
  evaluation run in one instance, so each waits behind the other. A
  bundle that provides one role is unaffected.
- A program-only bundle links the reactor runtime through the
  `aether-bloomery-bundle` facade. The unused code is stripped from the
  WASM, so the cost is compile time.
- A WASM trap in a reactor root calls `fatal_abort` and kills the whole
  substrate ([ADR-0063](0063-fail-fast-on-abnormal-component-lifecycle.md);
  `crates/aether-component/src/trampoline/runtime/mod.rs`, the
  `component.deliver` error arm). It doesn't just lose one bundle. This
  also corrects ADR-0224's consequence, which says a trap "loses every
  in-flight invocation of that bundle". There, too, it ends the process.
- Dormant instances stay loaded and hold memory for the engine's lifetime.
- Reactors in one bundle share a single 4 GiB memory, and the root keeps
  the whole retained prefix (`Owner`). Bounding that is deferred (#6132).
- The root doesn't authenticate senders. Any engine-local mailer can
  advance an instance by sending the next entry. Authentication is
  deferred.
- The design assumes one driver per engine. Instances are shared by
  digest, and seqs are unique only within one journal.
- The snapshot path is retired: `PreparedPrefix`, `PublishedView`, the
  `Publish`-encoded view snapshots, and the peer and ack machinery.
  `aether_bloomery_view::Publish` itself stays.
- `ClusterConfig`, the stream token, `EventBatch`, `PeerEvaluated`,
  `PreparedResult`, and `EvaluatedResult` are removed. The new kinds have
  to replace the old reactor event kind in one change, because the
  registry refuses a changed schema under the same name.
- The reactor end-to-end test moves out of `aether-substrate`
  (`crates/aether-substrate/tests/reactor_bundle.rs` and its two
  dev-dependencies) into the reactor crates. No runtime code in
  `aether-substrate` changes.
- The decisions to amend ADR-0223 follow ADR-0224's precedent, so
  ADR-0222 and ADR-0223 stay unedited. This ADR amends:
  - ADR-0222's peers and owned-data mail;
  - ADR-0223's instance identity, stream token, "may be dropped", and
    activation programs.
- Deferred, not foreclosed:
  - dropping and reloading an instance;
  - a structured "already loaded" reply;
  - several journals per engine;
  - view and guard dependencies in the custom section;
  - renaming `NAMESPACE` to `NAME`.

## Alternatives considered

- **Separate program and reactor roots that can't share an `export!`**
  (this ADR's decision 8 before the 2026-09-19 amendment). Rejected: a
  feature whose rules call its own programs would need two bundle
  crates, and upgrading the pair would take two head moves. The failure
  isolation it seemed to buy doesn't exist, because a trap kills the
  substrate either way and poison is reactor state.
- **One module loaded as two instances with role-qualified names**
  (`program.<digest>`, `reactor.<digest>`). Rejected: it splits one
  bundle across two addresses and two memories, and restart recovery
  would adopt each role separately.
- **A routing root with the role roots as inline children.** Rejected: a
  child's reply returns to the root, so the root would relay every
  reactor reply. It has the same single-instance cost as one root and
  adds a layer.
- **Per-reactor peer actors (today's shape).** Rejected: every peer runs
  in the same WASM instance. They buy no isolation but cost a snapshot
  encode and decode per event.
- **One inline child per event, named by seq.** Rejected: an in-place
  child's context has no sender, so the root would have to hold reply
  handles for each seq. It would add two host round trips per event and
  gain neither isolation nor attribution.
- **Keep the stream token.** Rejected: it ties an instance to one caller.
  Reply correlation and the contiguity check on seqs already reject stale
  or foreign input.
- **One instance per (head, digest).** Rejected: heads that name the same
  code would fold the same history twice and hold two copies of it.
- **The bundle writes its own receipts or intents to the journal.**
  Rejected: guest code would attest to its own execution, and reply
  envelopes carry no verifiable sender address.
- **Activation as programs (`core.cluster.activate` / `retire`).**
  Rejected in favor of native driver records; see ADR-0226.
