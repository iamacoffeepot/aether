# ADR-0224: Programs Are WASM Bundles

- **Status:** Proposed
- **Date:** 2026-09-18
- **Amended:** 2026-09-19 — one `bundle` export generator and one root per bundle digest serving programs, reactors, or both (ADR-0225 decision 8).
- **Amended:** 2026-09-24 — `export!` takes keyed entries only; a bundle is spelled `export!(public = [..], generators = [aether_bloomery_bundle::bundle])` (issue 6584).

## Context

[ADR-0220](0220-the-journal-is-an-append-only-log-of-typed-events.md)
§Programs defines a program as a stored declaration (name, input kind,
result kind, mode, intent) whose digest is its identity. An executor is a
native runtime type that claims declarations and is never stored. A
`Transition` records the program, input, result, and executor. The code on
main follows that shape: `aether-bloomery-program` carries the
`Executors` registry, the `Execute` trait, `ReadArtifacts`, and a
synchronous `apply` over a SQLite `Journal`, and `Transition` / `Fault`
in `aether-bloomery-kinds` carry an `ExecutorName`.

Reactors went the other way. [ADR-0222](0222-reactor-bundles-own-their-views.md)
puts reactor code in WASM bundles, and
[ADR-0223](0223-journal-selected-reactor-set.md) selects those bundles by
head and loads each version beside its predecessor. A reactor is
authored as an ordinary function over injected data (the trigger, views,
and guards), and generated code (`bundle_reactors`) owns the actor. With
programs still native, the two halves of the system are glued together
through different mechanisms. There is no mail contract between a driver
and program code, and a program cannot ship as a journal artifact the way
a reactor bundle does.

Nothing outside `aether-bloomery-program` consumes the native registry
today (`git grep -n "aether_bloomery_program" -- crates xtask`). No
deployed journal holds `bloomery.transition` or `bloomery.fault` rows: the
Bloomery rewrite has not been deployed.

## Decision

1. **A program is part of a WASM bundle.** A bundle is one WASM artifact
   that carries one or more programs, written as
   `#[program] impl Program for X` and exported with
   `export!(…, generators = [aether_bloomery_bundle::bundle])`, the one
   generator for programs and reactors (ADR-0225 decision 8).
   The bundle's digest is its identity. A program is identified by
   `ProgramRef { bundle: Digest, name: ProgramName }`, and `ProgramName`
   is unique within a bundle, enforced at compile time. A head names a
   bundle, the same way a cluster head names a reactor bundle. A request
   resolves that head at the trigger's prefix and pins the resulting
   digest.

2. **There are no native executors.** The `Executors` registry, the
   `Execute` trait, `ReadArtifacts`, and `apply` are removed. The code
   that ran is the bundle digest. `Transition` becomes
   `{ program: ProgramRef, input, result }` and `Fault` becomes
   `{ program: ProgramRef, input, reason }`. `ExecutorName` is removed.
   The stored encodings of `bloomery.transition` and `bloomery.fault`
   change without an upcast, because no persisted rows exist.

3. **Programs run in an injected-data sandbox, synchronously, in `Pure`
   mode only.** The author writes a stateless function,
   `fn run(input: Self::Input, env: &mut Env<Pure>) -> Result<Self::Result, Refusal>`.
   `Env<Pure>::read` returns only artifacts from the closure the driver
   injected with the request. Staging is in memory and produces
   `EncodedArtifact`s. The existing orphan rule holds: every staged blob
   must be reachable from the result. The program cannot read the
   journal, send mail, or perform I/O. `Refusal` stays the closed set
   from ADR-0220's fault rules (`Refused`, `InputMissing`,
   `InputDecode`). The driver still assigns everything else from outside.
   The closure is transitive. The driver caps its size and records a
   `Fault` rather than invoking a program whose closure exceeds the cap.

4. **The declaration lives in the bundle.** The `bundle` generator writes
   every program's `Program` declaration into an
   `aether.bloomery.programs` custom section. The driver reads that
   section from the artifact bytes to check that a program name exists
   and that its input kind matches, before loading anything. A separate
   declaration artifact is not stored.

5. **Addressing.** The driver loads each bundle once per engine with
   `aether.component.load` (`LoadComponent`), using `name` set to the
   bundle digest in lowercase hex and `export` set to the generated root
   namespace `aether.bloomery.bundle`. The root's address is
   `aether.component/aether.embedded:<digest>`. For each `Invoke`, the
   root spawns an inline child
   ([ADR-0114](0114-inline-child-actors.md), `spawn_inline_child`) with
   `Subname::Named(<seq>)`, where `seq` is the `Requested` entry's
   journal sequence number in decimal. The child's address is
   `aether.component/aether.embedded:<digest>/aether.embedded:<seq>`. It
   is built by the same `fold_lineage` over
   `ActorId::instanced(TRAMPOLINE_NAMESPACE, subname)` that detached
   siblings use. Both segments are legal under
   `validate_namespace_segment` ([ADR-0079](0079-instanced-actors-as-a-first-class-category.md)).
   The address carries no program name and no child type namespace:
   WASM children always render as `aether.embedded:<subname>`.

6. **The root owns its invocation names.** It refuses an `Invoke` whose
   seq is already live, because `insert_child` would silently replace the
   running child. It never spawns with `Subname::Counter`, whose bare
   decimals would collide with seqs. It despawns each child after its
   reply, which retires the alias (`RegistryBatch::retire_alias`) without
   leaving a tombstone. A loaded bundle is never dropped: `DropComponent`
   keeps the name, so a reload under the same digest would fail with
   `SubnameInUse`.

7. **Only native code writes journal records.** The bundle receives
   `Invoke { seq, program, input, closure }` and replies `Invoked`:
   - `Completed { seq, result, staged }` when the program ran;
   - `Refused { seq, refusal }` when the program refused;
   - `Rejected { seq, reason }` for a protocol refusal, such as an
     unknown name or a live seq.

   The driver records the outcome as a `Publish` of the staged artifacts
   plus a `Transition`, or as a `Fault`. A bundle never writes the
   journal and never attests to its own execution.

## Consequences

- Programs and reactors share one model: authored as functions, shipped
  as bundles, selected by head, loaded by digest, fed injected data, and
  recorded by native code.
- `aether-bloomery-program` becomes a portable `no_std` guest SDK. A new
  `aether-bloomery-program-derive` crate carries `#[program]` and the
  generator. Work orders: #6179 (SDK and mail) and #6180 (macro and
  bundle root).
- Every invocation of one bundle shares a single WASM instance, which
  means:
  - a trap loses every in-flight invocation of that bundle;
  - they share one 4 GiB memory;
  - they run one at a time.

  This is acceptable for synchronous `Pure` programs.
- Logs and costs are attributed to the bundle root, not the invocation
  (ADR-0114, v1 observability).
- The transitive closure puts a program's whole input in guest memory.
  The driver's size cap turns an oversized input into a recorded `Fault`
  rather than an out-of-memory trap.
- A driver actor (resolve the head, assemble the closure, load the
  bundle, invoke, record) is follow-on work in the ADR-0223 arc.
- Deferred, not foreclosed:
  - asynchronous `run`, where `read` becomes an on-demand fetch and the
    closure becomes a prefetch;
  - `Sampled` programs with effects through `Env`;
  - turning trees into files on disk for native tools;
  - confining the child processes those tools run;
  - per-invocation isolation through detached `spawn_child` siblings,
    which have the same address, but also tombstone when they retire;
  - a structured "already loaded" reply. A duplicate load is currently
    distinguishable only by its `SubnameInUse` error text.
- Nothing here changes `aether-substrate`, `aether-actor`,
  `aether-component`, or any chassis crate. The addressing was verified
  against current behavior in SubstrateHarness.

## Alternatives considered

- **Keep native executors alongside WASM programs.** Rejected: two valid
  ways to run a program, and native code cannot be stored or selected as
  a journal artifact.
- **Identify a program by its declaration digest.** Rejected: programs in
  one bundle share their code, and the bundle is what gets loaded and
  selected.
- **One root for all programs, loading each invocation as its own
  component.** Rejected: foreign modules cannot be inline children
  ([ADR-0097](0097-wasm-sibling-spawn.md) §3), each load costs an
  instantiation, and each dropped component leaves its name registered.
- **Put the program name in the invocation subname.** Rejected: the seq
  is already unique, and the program is recorded in `Invoke` and
  `Requested`.
- **Address invocation children by their type namespace.** Rejected: it
  would require changing how `aether-substrate` renders aliases and how
  `aether-actor` folds `EmbeddedMany`.
- **Let the bundle write its own `Transition`.** Rejected: guest code
  would attest to its own execution, and reply envelopes carry no
  verifiable sender address.
- **Asynchronous `run` from the start.** Deferred: synchronous `Pure`
  first, with `read` as the only method expected to become asynchronous.
