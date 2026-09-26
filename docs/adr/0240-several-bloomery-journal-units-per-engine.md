# ADR-0240: Several Bloomery Journal Units Per Engine

- **Status:** Proposed
- **Date:** 2026-09-26

Amends [ADR-0226](0226-native-bundle-driver.md) decisions 1 and 2,
[ADR-0229](0229-program-cap-apis-are-extra-run-arguments.md) decision 2 and
its 2026-09-24 amendment, and [ADR-0237](0237-workspaces-run-steps-over-trees.md)
decisions 2, 3, 7, and 8, whose text carries the change. Uses
[ADR-0231](0231-protocol-typed-references-and-reply-checks.md)'s
protocol paths for the workspace's storage source (D7). Resolves the "several journals per engine" deferral in
[ADR-0225](0225-reactor-bundles-load-by-digest.md) and ADR-0226. Gives the
`resolve` verb that [ADR-0230](0230-proven-actor-references.md) §3 reserves
for typed paths its first consumers: native over a `ProtocolPath<P>` (D7),
guest over an `ActorPath<R>` (D8).

## Context

### What the engine is today

Every row was read from the code on `main`.

| Piece | Code | Shape |
|---|---|---|
| Mount | `crates/aether-chassis-bloomery/src/mount.rs` | `BuiltChassis::spawn_actor` births one `JournalActor` as `aether.bloomery.journal:journal` and one `BundleDriver` as `aether.bloomery.driver:driver`, both `#[actor(instanced, root)]`. The driver is bound to the journal at birth through `DriverParams { journal: ActorRef<JournalActor> }`. |
| Chassis | `crates/aether-chassis-bloomery/src/chassis.rs` | Composes `WorkspaceCapability` (`#[actor(singleton, root)]`) over `WorkspaceParams { artifacts }`, the one `ArtifactStore` of the one journal root the chassis opened. |
| Config | `crates/aether-chassis-bloomery/src/config.rs` | `BloomeryConfig.journal: Option<String>` is one root. `read_cache_bytes` is the journal owner's read-cache budget. |
| Driver | `crates/aether-bloomery-driver/src/actor/` | Loads a bundle by sending `LoadComponent { name: Some(<digest>), export: Some(BUNDLE_NAMESPACE) }` to `aether.component`, so every root is a child of the component host named by its digest. Keeps `roots: HashMap<Digest, ErasedActorRef>` from each load reply's stamped sender. |
| Bundle root | `crates/aether-bloomery-bundle-derive/src/expand/root.rs` | One generated root per digest serves both roles (ADR-0225 decision 8): a `programs` field and a `reactors` field in one actor. |
| Program role | `crates/aether-bloomery-program/src/root.rs`, `expand/programs.rs` | The role's only state is `live: BTreeMap<u64, Live<H>>`, keyed by `Invoke.seq`. Each invocation is an inline child named `Subname::Named(seq)`, which the host registers as `<root>/aether.embedded:<seq>`. A second `Invoke` with a live seq is refused `"seq already live"`. A fetch-on-miss already relays invocation → root → the `Invoke`'s sender. |
| Program APIs | `expand/programs.rs` `expand_send_pending` | The invocation declares `depends(api_target::…)` and sends a captured call through `ctx.actor_ref::<T>()`, a root-singleton proof. |
| Reactor role | `crates/aether-bloomery-reactor/src/root.rs` | One `Owner` with a cursor. `Warm` and `Event` refuse anything but `cursor + 1` with `OutOfSequence`, and a fold failure poisons the role. The loaded role is a fold of one journal's log. |
| Workspace | `crates/aether-workspace/src/config.rs` | `WorkspaceConfig` is the actor's `Config`: the host's `cpuset`, `budget_memory_bytes`, and the per-run defaults. Each actor that resolves it claims the whole host. |
| Module reuse | `crates/aether-component/src/component/runtime/module_cache.rs` | One slot keyed by sha256 (`ModuleCache { cached: Option<CachedModule> }`, line 25). Back-to-back loads of one digest compile once; any other load between them evicts the slot and the next load recompiles. |
| Trampoline namespace | `crates/aether-component/src/trampoline/runtime/mod.rs` | Every loaded component is one native type, `WasmTrampoline`, whose `NAMESPACE` is the constant `EMBEDDED_SCOPE` (`"aether.embedded"`). `LoadComponent` chooses only the discriminator (`name`) and the exported actor (`export`). |
| Bootstrap | `crates/aether-bloomery-bootstrap` | Config carries two `ErasedActorPath`s (journal, driver), proven at `wire` with `resolve_path`, sent through unchecked `ErasedActorRef`s. `depends(WorkspaceCapability)` reaches the root singleton. |
| Identity halves | `aether-workspace` vs journal and driver crates | `aether-workspace` has an always-on `WorkspaceCapability` marker and a `runtime` feature (ADR-0122). The journal crate depends on `aether-substrate` and `rusqlite` unconditionally, and the driver on `aether-substrate`, so a guest cannot name `JournalActor` or `BundleDriver`. |

Who can place a child beneath an existing actor today:

| Spawner | Verb | Parent |
|---|---|---|
| The parent itself, from a handler | `NativeCtx::spawn_child::<C>` (staged; `C: ChildOf<A> + Instanced`) | the ctx's own actor, never a caller-supplied one |
| The component host, for a wasm trampoline | `NativeCtx::spawn_child_scoped::<C>(parent: ErasedActorRef, …)` (`#[doc(hidden)]`) | a proven foreign parent. Its only request path is `aether.component.load_under`, documented as test-harness only, which takes the parent as `ErasedActorPath` text |
| An embedder | `BuiltChassis::spawn_actor::<A>` | none: `A: Root` only |
| A guest | inline children and detached siblings | its own inline cluster |

No native ctx has a parent-reference verb (issue #6796 records this). An
embedder can look up a live child with `BuiltChassis::child::<P, C>` but
cannot spawn one.

`spawn_child_scoped::<C>` bounds `C: ChildOf<A>` against the calling actor
`A`, not against the parent it is handed
(`crates/aether-substrate/src/actor/native/ctx/spawn.rs:94-110`). Called from
the component host, the bound holds for every trampoline, so
`aether.component.load_under` can place a trampoline beneath any live actor.
Its callers are the harness (`HarnessOp::load_component_under`) and two test
files. Issue #6821 tracks the fix. This ADR does not use the verb.

### What collides when a second journal appears

| Shared today | Why a second journal breaks it |
|---|---|
| The bundle root of a digest | Its reactor role folds one log in order. A second driver's `Warm` / `Event` arrive out of sequence and poison it. Its program role keys children by `seq`, which is unique only within one journal. |
| The root's name | Two drivers loading one digest under the component host collide on `SubnameInUse`. |
| `aether.workspace` | One artifact store, one journal. A second journal's runs would write into the first. |
| Engine-wide budgets | A second workspace or journal owner resolving its own `Config` claims the whole host's cores, memory, or read cache again. |
| Addressing | Config, the bootstrap script, and `xtask import-commit` name the journal and driver by fixed paths. |

## Decision

### Terms

| Term | Meaning |
|---|---|
| **unit** | One log plus every actor whose state derives from that log. |
| **unit key** | The `UnitKey` that names a unit, unique per engine: a `LoadName` of at most 191 bytes (D3, D4). |
| **unit root** | The unit's log owner, the root of the unit's lineage: `JournalActor` at `aether.bloomery.journal:<key>` (D1). |
| **member** | An actor beneath a unit root: the driver. |
| **bundle root** | A trampoline beneath the engine's one `aether.component` that hosts one bundle's generated root for one unit, loaded under the name `<key>-<digest>` (D4). |
| **belongs to a unit** | A member belongs by lineage, beneath the unit root. A bundle root belongs by name, through the unit key its name is built from. |
| **engine-shared** | Held once per engine and keyed by content hash, or holding no log state. |

```text
aether.bloomery.journal:<key>                          unit root: the journal
└── aether.bloomery.driver:driver                      member; relays its programs' runs to the workspace

aether.component                                       engine-shared host
├── aether.embedded:<key>-<digest>                     bundle root of unit <key> (one per digest the unit loads)
│   └── aether.embedded:<seq>                          inline child per live invocation
└── aether.embedded:<other-key>-<digest>               the same digest's bundle root for another unit

aether.bloomery.workspace                              engine-shared; no store: reads and writes each request's source

engine-shared: aether.component (and its compiled-module map),
aether.bloomery.workspace, aether.http, the engine blob store, the RPC server,
the inventory
```

### Invariants

Each rule is followed by the mechanism that makes breaking it impossible or
refused, the concrete way it would be broken and why that way is closed, and
what it forces elsewhere.

**I-1. A unit is indivisible: everything that must share one total order
lives in one journal, and a journal is never split across units.**

- *Upheld by:* units exist only at boot, one per config entry (D2, D3).
  `JournalActor` is `#[actor(instanced, root)]`, and a root is spawned only
  through `BuiltChassis::spawn_actor`, which only the chassis mount holds. No
  ctx verb births a root: `NativeCtx::spawn_child` requires `C: ChildOf<A>`,
  and a guest spawns only inline children and siblings. So no bundle,
  program, or reactor can create a journal for part of a unit's work. Each
  unit opens exactly one journal root, and D3 refuses two entries with the
  same root, so no journal is shared by two units.
- *Would be violated by:* splitting one log's work across several journals,
  or two units opening one journal root. Closed by construction: no runtime
  door births a unit, and a repeated root refuses boot. Closed by
  semantics as well: I-3 says no order exists across units, so work split
  across two journals loses the order between its parts.
- *Implication:* a unit runs as one whole, with one journal and one driver.
  Its runs go through the engine's one workspace, whose source for them is
  the unit's journal, so their artifacts stay in that journal (D7). Work that must stay in order grows inside its unit's
  log, never by adding logs.

**I-2. Every actor whose state derives from a log belongs to that log's
unit: a member by lineage beneath the log's owner, a bundle root by a name
built from the unit's key.**

- *Upheld by:* lineage for members and naming for bundle roots. The driver
  is `#[actor(instanced, child_of(JournalActor))]` and is born beneath its
  journal (D2). The workspace holds no unit's data and derives no state
  from any log (I-7): it reads and writes a request's artifacts only
  through the request's source (D7). Every bundle root is loaded under the
  name `UnitBundle::name(key, digest)`, the only constructor of that name,
  from the unit key its driver was born with (D4). Unit keys are unique per
  engine (D3), so two units' roots of one digest are two actors under two
  names, and each name states the unit it folds for.
- *Would be violated by:* one bundle root of a digest serving two units,
  the shape on `main`, where a root's name is its digest alone. Closed: the
  name carries the key, and a driver holds proofs only for the roots its
  own loads returned. A name written by hand at a call site, or hashed
  from the key and digest, would hide which unit a root belongs to. Closed
  by review: the driver builds names only through `UnitBundle::name`. A
  root loaded under a unit's name by some other loader (MCP
  `load_component`) makes that unit's own load fail on `SubnameInUse`; the
  driver never binds to it, because it keeps only its own load reply's
  sender, as it does on `main`.
- *Implication:* one bundle root per (digest, unit); the compiled module is
  shared instead (D5). A unit's bundle roots are not a subtree, so
  removing a unit means dropping its `<key>-*` roots (D9).

**I-3. A unit's log is totally ordered and local: every entry's position is
its own journal's `seq`, and no entry is ordered against another unit's.**

- *Upheld by:* each unit opens its own journal root (a separate SQLite
  database and `blobs` directory under its own lock, ADR-0220); `seq` is
  that database's. No kind carries a position in another journal, and no
  actor appends to two journals: each driver holds exactly one
  `ActorRef<JournalActor>`, handed at birth and never replaced. The
  workspace holds no journal; a run's outputs are staged to its source,
  which the unit's driver sets to its own journal (D7).
- *Would be violated by:* a driver retargeted by mail, or a cross-unit
  `cause`. Closed: `ActorRef` has no codec, so no mail can carry a journal
  to write to; `AppendRecords` requires every cause in `1..=expected_seq`
  of the journal it is sent to.
- *Implication:* anything that needs an order between two entries puts both
  in one unit's journal (I-1).

**I-4. A driver drives exactly one journal, and a bundle root answers
exactly one driver.**

- *Upheld by:* `DriverParams { unit, journal }` is filled at birth from the
  unit's key and the journal's spawn result (D2). A root sends its replies
  to its `Invoke` / `Warm` / `Event` sender, and only the driver that loaded it holds a proof of it: the load
  reply goes to the requester alone (ADR-0230 §3), and the driver keeps
  that reply's stamped sender. The root's parent is `aether.component`,
  which sends it nothing after the load.
- *Would be violated by:* a second driver sending `Invoke` to a root of
  another unit. Closed in the type path (it holds no proof); not closed
  against a hand-written path, because ADR-0230's 2026-09-25 amendment lets
  any loaded component prove any `Live` path. That is the known
  unauthenticated-writes gap of ADR-0226, unchanged here.
- *Implication:* `Invoke.seq` is unique among the invocations a root
  holds, so the program role keeps `live` keyed by `seq` with no change.

**I-5. An invocation reaches providers only through its own unit's driver.**

- *Upheld by:* D6 and D7. The generated invocation holds one proof, its
  parent root, and declares no dependency. A program API call goes
  invocation → root → the `Invoke`'s sender (the driver) → the provider the
  driver declares. The driver builds every `Run` it relays and sets its
  `source` to its own journal; the program-side call carries every field
  but `source`, so a program cannot choose where its run reads and writes
  (D7).
- *Would be violated by:* an invocation sending `Run` to the workspace
  itself (ADR-0229 today, through `A::NAMESPACE`) with another unit's
  journal as `source`. Closed: the invocation declares no dependency (D6),
  so it holds no proof of the workspace to send through. A program naming
  a source in its call. Closed: the call has no such field. A shared
  program root relaying one unit's call through another unit's driver.
  Closed by I-2: roots are per unit.
- *Implication:* ADR-0229's binding moves from a declared dependency on the
  invocation to a relay through the invoker (D6).

**I-6. An engine-wide budget is handed out once: to one actor that serves
every unit, or as per-unit shares, never as copies.**

- *Upheld by:* the host's cores and memory go to the one workspace, a
  `#[actor(singleton, root)]` the chassis composes once, so
  `WorkspaceConfig` resolves once (D7). The journal read-cache budget is
  divided: each journal gets an equal share of `read_cache_bytes` (D3).
- *Would be violated by:* a workspace per unit, each resolving
  `WorkspaceConfig` off argv and env. Closed: the workspace is a singleton,
  and no unit spawns one (D2).
- *Implication:* every unit's runs wait in one FIFO queue against one
  budget (ADR-0237 decision 9). Cores free anywhere serve the next run
  from any unit, and a unit with many runs queued can delay another's.

**I-7. What is engine-shared is keyed by content hash or holds no log
state.**

- *Upheld by:* the compiled-module map is keyed by sha256 (D5); the engine
  blob store dedups by hash (ADR-0238); `aether.component`, `aether.http`,
  the RPC server, and the inventory keep no fold of any journal. The
  workspace keeps no store and no fold: its only state is the budget, the
  queue, and run-key estimates, which are executor-local and keyed by the
  digest of what a run does (ADR-0237 decision 9). The bundle
  roots beneath `aether.component` do hold folds, and each belongs to one
  unit by name (I-2); the host keeps only their load bookkeeping.
- *Would be violated by:* an engine-wide actor that caches a view of one
  unit's log. Closed only by review: the chassis composes no such actor,
  and a new composed actor that reads a journal is a change to this ADR.
  One bundle root per digest holding every unit's views, keyed by journal,
  is such an actor; it is rejected below.
- *Implication:* sharing a bundle across units costs one instance per unit
  and one compilation per engine.

**I-8. A unit is named by its key, a member by its type beneath the unit,
and a bundle root by its unit's key and its digest; no position crosses a
boundary.**

- *Upheld by:* D8 and D4. Config carries unit keys (`UnitKey`, validated on
  decode). The bootstrap writes each unit's paths from the actor types and
  the key: the journal's is `ActorPath::<JournalActor>::root(&key)`, and a
  member's is written beneath it with the member's fixed key,
  `.child::<C>(&C::key())`. It proves each through the guest verb
  `WasmCtx::resolve`, which mints `ActorRef<R>`; native code holds its
  proofs from spawn results. A
  bundle root's name is `UnitBundle::name(key, digest)`, so anyone holding
  the key and the digest derives it, and its canonical path
  `aether.component/aether.embedded:<key>-<digest>` splits back into both
  because the digest is last and fixed-width. `ActorRef` has no codec, and
  no config, kind, or record carries a `MailboxId` (ADR-0230 §1).
- *Would be violated by:* a config slot holding the driver's path (the
  bootstrap today), a role or alias per unit, `send_to_named`, or a
  `MailboxId` in config. Closed: the bootstrap's config loses its path
  fields, `send_to_named` is deleted, and every path is written from a type
  and a key and carries no position. A bundle root name that cannot be split
  back into key and digest. Closed: `UnitBundle::name` is the only
  constructor, and `UnitKey` is short enough that the name always fits one
  segment.
- *Implication:* the journal and driver crates split identity from runtime
  (ADR-0122) so a guest can name their types and its sends are
  kind-checked. A bundle root's path reads as its unit and digest in logs,
  traces, and MCP tools.

### D1. Unit topology (serves I-1, I-2, I-3, I-7, I-8)

| Held | Scope | Why |
|---|---|---|
| Journal (log, artifact files, read cache) | per unit | I-3 |
| Driver (program queue, reactor following, fetch cache) | per unit | I-2, I-4 |
| Bundle roots (program role and reactor role) | per (digest, unit), beneath `aether.component`, named `<key>-<digest>` | I-2 |
| Workspace (the host budget, one admission queue, run-key estimates; no store) | per engine | I-5, I-6, I-7 |
| Component host, compiled modules by hash | per engine | I-7 |
| Engine blob store (in-memory bytes by hash) | per engine | I-7 |
| HTTP egress, RPC server, inventory | per engine | I-7 |
| Durable artifact files | per unit (its journal root's `blobs`) | I-3: each root is one lock and one writer (ADR-0237's 2026-09-25 amendment) |

A unit's root is its `JournalActor`; its member is `BundleDriver`; its
bundle roots belong to it by name. The unit root is
the journal because a dedicated unit actor (`aether.bloomery.unit:<key>`)
would have no handlers once the embedder can spawn children beneath a proof
(D2).

Bundles are engine-shared code: the bundle format, digest naming, the
`Warm` / `Event` / `Invoke` protocols, and each compiled module (D5) serve
every unit alike, and so does the one workspace (D7). Each unit owns its
log, its driver, and every artifact its runs read or write.

### D2. The chassis mounts each unit, then binds (serves I-1, I-2, I-4)

```rust
impl<C: Chassis> BuiltChassis<C> {
    /// Birth `Child` beneath `parent`, committed eagerly like `spawn_actor`.
    pub fn spawn_child<P: Addressable, Child: ChildOf<P> + Instanced + NativeActor>(
        &self,
        parent: ActorRef<P>,
        subname: Subname<'_>,
        config: Child::Config,
        params: Child::Params,
    ) -> SpawnBuilder<'_, Child>;
}
```

`mount` runs, for each unit in config order:

1. `spawn_actor::<JournalActor>(Named(key), cache_share, journal)` →
   `ActorRef<JournalActor>`;
2. `spawn_child::<JournalActor, BundleDriver>(journal, Named("driver"), limit,
   DriverParams { unit: key, journal })`.

The workspace is not mounted per unit: the chassis composes the one
`WorkspaceCapability` beside the component host, with no params, before
any unit is mounted (D7). Only after every unit is mounted does the RPC
bind gate open (issue #6399's order, now over every unit). `Mounted`
becomes one `MountedUnit { journal, driver }` per key.

The embedder verb is new. Its consumer is the mount. It reuses
`SpawnBuilder::new_child`, which the staged handler verb already builds
from a parent identity, and it is bounded by the same `ChildOf` placement
fact. The alternative, the journal spawning its own members from `wire`,
needs a native self-reference door to fill `DriverParams.journal` and a
readiness wait before bind; it is rejected below.

### D3. Chassis config is a list of units (serves I-1, I-3, I-8)

| Knob | Shape | Lowered to |
|---|---|---|
| `AETHER_BLOOMERY_UNITS` / `--bloomery-units` | comma-separated `key=root` entries, the precedent `--http-secrets` sets | `Vec<UnitSpec { key: UnitKey, root: PathBuf }>` |
| `AETHER_BLOOMERY_JOURNAL` | removed | — |
| `closure_limit_bytes` | unchanged, a per-read ceiling | one `ClosureLimit` for every driver |
| `read_cache_bytes` | now the engine's total | divided equally among units (I-6) |
| Workspace knobs (`WorkspaceConfig`) | unchanged, the engine's | the one workspace (D7); no per-unit workspace knob exists |

Lowering refuses boot, naming the key, for: an empty list, a key that is
not a `LoadName`, a key longer than 191 bytes, a repeated key, or two
entries whose roots are the same directory after canonicalization. The
191-byte limit is what `UnitKey::new` checks: a bundle root's name is the
key, a dash, and 64 hex characters, and one path segment holds at most 256
bytes (`crates/aether-data/src/reference/segment.rs`), so a longer key would
leave the unit unable to load any bundle. The root lock stays the guard
across processes. Each root is opened before wasmtime, as today, so a held
root costs no boot.

### D4. Bundle roots are named by unit key and digest (serves I-2, I-4, I-8)

Every bundle root stays a trampoline beneath the one engine-level
`aether.component`, as on `main`. Its load name is a readable fold of the
unit key and the bundle digest:

```rust
/// A `LoadName` of at most 191 bytes: the key of one unit (ADR-0240).
/// Fallible on construction and on decode.
pub struct UnitKey(LoadName);

/// The load name of a unit's bundle root.
pub struct UnitBundle;

impl UnitBundle {
    /// `<key>-<digest>`, the digest as 64 lowercase hex characters.
    ///
    /// Every bundle root is a child of the one engine-level
    /// `aether.component`, so this name is what keeps two units' roots of
    /// one digest apart and what says, in a log, a trace, or an MCP path,
    /// which unit a root folds for and which bundle it runs (ADR-0240 I-2,
    /// I-8). The digest comes last at a fixed width, so the name splits back
    /// into key and digest even when the key contains dashes; the key's
    /// 191-byte limit keeps the whole name inside one 256-byte segment.
    pub fn name(key: &UnitKey, digest: &Digest) -> LoadName;
}
```

Both live beside `Digest` in `aether-bloomery-kinds`, so the driver, the
chassis, and external tooling build the same name from the same parts.

- `UnitBundle::name` is the only way a bundle root's name is built. It is
  never written by hand at a call site, and never hashed (a
  `sha256(key || digest)` name would hide both parts and the reason the
  name exists).
- The driver is born with its unit's key (`DriverParams.unit`, D2) and
  loads each bundle with `LoadComponent { name:
  Some(UnitBundle::name(&unit, &digest)), export: Some(BUNDLE_NAMESPACE) }`
  sent to `aether.component`. `LoadComponent` is unchanged.
- The driver keeps each load reply's stamped sender in
  `roots: HashMap<Digest, ErasedActorRef>`, as it does on `main`, and sends
  to roots only through those proofs. The name is for placement and for
  readers; routing never parses it.

This amends ADR-0226:

- **Decision 1.** One driver per unit. Roots are shared by digest within one
  unit; `Invoke.seq` is unique within one journal, which is now one
  driver's.
- **Decision 2.** "Roots are named by digest" becomes "roots are named by
  (unit key, digest)": a root's name is `UnitBundle::name(key, digest)`,
  whose digest part is the same lowercase-hex `artifact_digest` ADR-0226
  uses. Roots are still never dropped by the driver, and the name is unique
  beneath `aether.component` because unit keys are unique per engine.

A root keeps the `aether.embedded` namespace every loaded component has.
A dedicated namespace (`aether.component/aether.bloomery.bundle:<key>-<digest>`)
would put the reason in the address itself, but a load cannot choose its
namespace:

- The trampoline is one native type whose `NAMESPACE` is the constant
  `EMBEDDED_SCOPE` (`crates/aether-component/src/trampoline/runtime/mod.rs:79`,
  `crates/aether-actor/src/model/mod.rs:102`). A staged birth names its node
  `ActorId::instanced(A::NAMESPACE, subname)`
  (`crates/aether-substrate/src/actor/native/spawn/staged.rs:121-122`), and
  `LoadComponent` carries only the subname and the export.
- One Rust type owns one namespace (`try_claim_namespace` in
  `crates/aether-substrate/src/actor/native/spawn/activation.rs:200`), so a
  second namespace needs a second trampoline type.
- The guest's `Embedded` resolver folds `instanced(EMBEDDED_SCOPE, …)`
  (`crates/aether-actor/src/model/mod.rs:119`); the host's inline-child and
  sibling spawns use `TRAMPOLINE_NAMESPACE`
  (`crates/aether-substrate/src/actor/wasm/host_fns.rs`); and the registry
  marks a mailbox a trampoline when its leaf segment starts with
  `aether.embedded:` (`crates/aether-substrate/src/mail/registry/names.rs:61`).
  A second trampoline type would have to be threaded through all three.

The dedicated namespace is deferred (D9).

### D5. Shared code, per-unit instances (serves I-2, I-4, I-7)

Code is shared through the compiled module: one per digest per engine.
`ModuleCache` holds one slot today
(`crates/aether-component/src/component/runtime/module_cache.rs:25`), so two
units loading different bundles alternately would recompile each. It becomes a
map keyed by sha256 content hash that keeps each compiled module while any
live trampoline of that hash exists. Bundle roots are never dropped (D4),
so a bundle compiles once per engine however many units load it and in
whatever order.

Each unit gets its own bundle root instance: its own linear memory, its own
reactor views, and its own program table. A unit's cost for a bundle it
shares with another unit is that one instance. One generated root serves
both the program role and the reactor role (ADR-0225 decision 8), and its
name carries the unit key (D4), so program roots are per unit exactly as
reactor roots are.

Code reuse and actor sharing are separate decisions. A compiled module is
immutable code with no log state, so sharing it is I-7. An actor holds a
fold of one log and one driver's `seq` space, so units do not share one:

- the reactor role folds one log in cursor order, and a second unit's
  `Warm` / `Event` poison it;
- the program role keys invocations by `seq`, which is unique only within
  one journal;
- one actor runs one handler at a time (ADR-0038, ADR-0087), so a shared
  root would serialize every unit's folds and runs through one mailbox;
- a removed unit's views would stay inside a live shared actor with no way
  to evict them.

### D6. Program API calls relay through the invoker (serves I-4, I-5)

| Hop | Holds | Pending reply |
|---|---|---|
| Invocation → its root | the parent proof | the invocation's `waiting`, by request id, as today |
| Root → the `Invoke`'s sender | `invokers`, as the fetch-on-miss relay already does | a deferred reply per relayed call, answered once |
| Driver → provider | its declared dependencies, `WorkspaceCapability` and `HttpCapability`; for a `Workspace` call it builds the `Run` with its journal as `source` (D7) | the driver's `callers`, as for a fetch |

This is the one path for every program API (`Http`, `Process`,
`Workspace`). The invocation declares no dependency. The closed API set
moves its check from the invocation's `depends(api_target::…)` to the
driver:

- Each program's record in the `aether.bloomery.programs` section gains the
  APIs its `run` binds. The driver already reads that record before any
  load (ADR-0226 decision 3); a program naming an API with no provider in
  this unit faults `BundleUnavailable` before `Invoke`. This keeps today's
  refusal-before-run, which the invocation's `depends` gave at bundle load.
  On the bloomery engine `Process` has no provider, since `aether.process`
  is not composed.
- The driver maps `Http` to `HttpCapability` and `Workspace` to
  `WorkspaceCapability`, and refuses anything else with `Refusal::Refused`,
  as the invocation does today.
- The driver declares `depends(ComponentHostCapability, HttpCapability,
  WorkspaceCapability)`, so a missing HTTP capability or workspace refuses
  the driver's birth.

This amends ADR-0229 decision 2 and its 2026-09-24 amendment: bindings no
longer resolve through `A::NAMESPACE`. The sealed `InjectedApi` set and the
SDK table stay; the table now names what the driver maps.

No hop drops or evicts a parked reply; each is answered exactly once or
abandoned when its actor closes, as the driver's `Drop` does today.

### D7. One Bloomery workspace per engine over a typed storage source (serves I-3, I-5, I-6, I-7)

The workspace is Bloomery's executor, and Bloomery runs one per engine: a
singleton service that every unit's runs go through.

| Piece | Decision |
|---|---|
| Crate and actor | The crate is `aether-bloomery-workspace` (today `aether-workspace`). `WorkspaceCapability`'s namespace is `aether.bloomery.workspace`, beside `aether.bloomery.journal` and `aether.bloomery.driver`. It is `#[actor(singleton, root)]`, composed once by the chassis (D2). |
| Kind names | Unchanged (`aether.workspace.run`, `aether.workspace.environment`, …). An artifact's digest is the digest of its kind-prefixed stored blob (`ReadArtifact`, `crates/aether-bloomery-kinds/src/journal/mod.rs`), so renaming `aether.workspace.environment` would change every stored environment's digest. |
| Budget | `WorkspaceConfig.cpuset` and `budget_memory_bytes` are the whole host's, resolved once. One FIFO queue admits every unit's runs (ADR-0237 decision 9). Run-key estimates are shared by every unit, since the key is what a run does. |
| Params | None. `WorkspaceParams { artifacts }` is removed, and the runtime half does not depend on `aether-bloomery-journal`. It keeps `aether-bloomery-kinds` (the tree kinds and the storage kinds below) and `aether-bloomery-tar`. |
| Storage | The request's. `Run` and `Import` carry `source: ProtocolPath<ArtifactStorage>`; the workspace reads inputs from it and stages outputs to it. It holds no artifact store. |

**The storage protocol.** The journal implements it:

```rust
#[protocol]
pub trait ArtifactStorage {
    /// Existing: `aether.bloomery.journal.read_artifact`, answered with the
    /// stored artifact as a `ClosureArtifact` (bytes as a `Blob`), `Missing`, or `Err`.
    fn read(mail: ReadArtifact) -> ReadArtifactResult;
    /// New, beside it in `aether-bloomery-kinds`.
    fn stage(mail: Stage) -> StageResult;
}

/// Store these artifacts: content-addressed, unfenced, no event, no head move.
/// Each carries its kind, its bytes as a `Blob`, and its citations.
#[aether_data::kind(name = "aether.bloomery.journal.stage", no_serde)]
pub struct Stage { artifacts: Vec<EncodedArtifact> }

#[aether_data::kind(name = "aether.bloomery.journal.stage_result", no_serde)]
pub enum StageResult { Staged, Err { message: String } }
```

`JournalActor` handles `ReadArtifact` today and gains `Stage`, so
`ArtifactStorage: CoveredBy<JournalActor>` holds and the journal stays the
only writer (ADR-0237 open question 1). `Stage` is the write the journal's
in-process `ArtifactStore` does for the workspace today, as mail. `Publish`
does not fit: it carries a whole-journal fence and head moves, which are the
journal's to order, not the executor's. Bytes cross as `Blob` values in both
directions; in-process mail shares a `Blob` through the engine blob store
rather than copying it (ADR-0238 decisions 3 and 10). `ReadArtifactResult`
already carries a `Blob`; `EncodedArtifact`'s bytes move from `Vec<u8>` to
`Blob` as `ClosureArtifact`'s did.

**The source is a checked path.** The request carries the storage path;
the workspace never asks who sent it. The ADR-0231 pieces it uses:

| Piece | Where | What it proves |
|---|---|---|
| `ActorPath::<JournalActor>::root(&key).narrow::<ArtifactStorage>()` → `ProtocolPath<ArtifactStorage>` | the unit's driver, once, from the key it was born with (`DriverParams.unit`) | compiles only if `ArtifactStorage: CoveredBy<JournalActor>`; the text `aether.bloomery.journal:<key>` is written from the type and the key, and the narrowing is type-level, with no registry lookup and no position (ADR-0230 §2, ADR-0231 §3) |
| `ctx.resolve(&run.source)` → `ProtocolRef<ArtifactStorage>` | the workspace, on receipt, before anything is queued | the path compiles to its position by the lineage fold, and one route-table lookup finds a `Live` route under that canonical name whose published rows still cover `ArtifactStorage` |

A source that does not resolve is refused at receipt:
`Refused(Refusal::SourceUnavailable)` for a run, `Failed { detail }` for an
import. Every read and stage of the request goes through the resolved
`ProtocolRef`; the reply goes to the caller, as today.

**Bloomery's sources.** The unit's driver writes its journal's source once
and sets it on every `Run` it relays for its programs (D6). The program-side
call carries every `Run` field but `source` (ADR-0237 decision 7), so a
program cannot choose where its run reads and writes (I-5). An `Import` goes
straight to the workspace, as today, from the bootstrap, which writes the
unit's journal as `source` the same way, or from an operator, whose MCP call
spells the path as text; it decodes as the same type and the workspace's
`resolve` proves it like any other. No driver is involved:
`Import` is operator mail (ADR-0237 decision 3), and the operator already
names the unit it imports into. So each unit's runs read and
write only its own journal, and no table maps anything to a unit.

**The seam.** The tar codec reads and writes trees only through `TreeSource`
and `TreeSink` (`crates/aether-bloomery-tar/src/store.rs`). The workspace's
one implementation of them, `JournalSource` / `JournalSink` over one
`ArtifactBatch` (`crates/aether-workspace/src/runtime/journal/mod.rs`), is
replaced by a second whose reads are `read` and whose writes are `stage`
through the source. The container logic keeps talking to the traits: the
environment image build (`run/environment.rs`), `write_tree` of the run tree
and each mount, and the output decode (`run/output.rs`). The three places
that use the batch directly go through the same source: the stdin attach
and the log capture in `run/step.rs` (`attach`, `store_output`), and the
import decode (`import/mod.rs`).

The sequence runs on the actor's worker thread (ADR-0093) and the codec
traits are synchronous. Each read or stage the worker makes is a request the
actor sends through the source for it, and the actor's reply handler hands
the answer back to the waiting worker. No dispatcher thread waits on the
journal.

**Checks before any container.** `run/resolve.rs`'s checks run over the
source, in today's order, before any container exists: `InputMissing` (a
`Missing` read), `ToolchainMismatch` (the tree's `rust-toolchain.toml`
against `Environment::provides`), and `UnknownTool` (the walk of the
environment root), then `PlatformMismatch` against the daemon. They stay in
the workspace because each compares a request with what the executor
provides.

**Outputs.** Step stdout and stderr, the output tree, and an import's tree
are staged in bounded batches as they are produced, each `Stage` answered
before the next is sent. The reply follows the last `Staged`. A `Stage`
answered `Err` ends a run or an import `Failed { detail }`. A run or import
that ends any way but `Ok` leaves what it staged cited by nothing:
content-addressed and inert, like an `import-commit` batch with no head move.
Staging as it goes bounds memory by the batch rather than by the largest
import.

The daemon's image store stays shared by every unit; it is a rebuildable
derivative labelled by environment digest (ADR-0237 decision 8), so two
units importing one environment converge on one image.

### D8. Addressing: units by key, members by type, bundle roots by key and digest (serves I-8)

| Piece | Change |
|---|---|
| Unit paths | written from the actor types and the unit key with ADR-0230 §2's `ActorPath<R>`: the journal is `ActorPath::<JournalActor>::root(&key)` (`aether.bloomery.journal:<key>`), and a member is written beneath it, `.child::<C>(&C::key())`. No registry lookup and no position: the text is each type's `NAMESPACE` and its key. `.narrow::<P>()` makes a `ProtocolPath<P>` where a holder needs only a protocol (D7). |
| `WasmCtx::resolve` | ADR-0230 §3's verb over an `ActorPath<R>`, the guest arm: the path compiles to its position by the lineage fold, and one route-table lookup checks the canonical name, `Live`, that the route's actor type is `R`, and `R`'s compiled rows against the published ones; it mints `ActorRef<R>`. The guest crosses one host import. It takes the name ADR-0230 reserved. It needs the route record to carry its actor type, which it does not on main (ADR-0230 §3). |
| `UnitMember` | a trait in the journal identity half: `ChildOf<JournalActor> + Instanced` with a fixed key, `C::key()` (`driver`). A member's path is its unit's path plus `.child::<C>(&C::key())`. The fixed key stands in for a one-per-parent child placement that the actor model does not have yet (ADR-0166 defers a keyless native-child resolver); #6822 designs that placement, and `UnitMember` is deleted when it lands. |
| `UnitKey`, `UnitBundle::name` | in `aether-bloomery-kinds` beside `Digest` (D4). The driver's only way to name a bundle root. |
| `aether-bloomery-journal`, `aether-bloomery-driver` | split per ADR-0122: an always-on, `no_std` identity (the marker, its handled kinds and contract rows, `UnitMember`) and a `runtime` feature carrying the actor, `aether-substrate`, and `rusqlite`. |
| Bootstrap config | `journal` and `driver` paths are replaced by `units: Vec<UnitKey>`. At `wire` it writes each unit's `ActorPath<JournalActor>` and `ActorPath<BundleDriver>` from the types and the key and resolves each with `WasmCtx::resolve` to an `ActorRef`; every send is `send_to(ActorRef<R>, &K)`, kind-checked. That includes `aether.bloomery.driver.call`, which the driver answers from a manual handler (`on_call`, `crates/aether-bloomery-driver/src/actor/mod.rs`); a protocol-typed link could not carry it, which is why the bootstrap links by actor type (ADR-0231 §3). Its journal sends, `ReadHead`, `Publish`, and `ReadArtifact`, land on single handlers (`crates/aether-bloomery-journal/src/actor.rs`). Its `Import`s name the unit's journal as `source`, `ActorPath::<JournalActor>::root(&key).narrow::<ArtifactStorage>()` (D7). |
| External callers (MCP, `xtask import-commit`) | name a unit's member by its canonical ADR-0166 path, `aether.bloomery.journal:<key>/aether.bloomery.driver:driver`; `import-commit` takes the unit key. An operator's `Import` names the unit's journal as its `source` (D7). A unit's bundle root is `aether.component/aether.embedded:<key>-<digest>`, or its short path `aether.component/:<key>-<digest>`. |

`resolve` is one verb, and each arm lands with a named production consumer:
the guest arm over an `ActorPath<R>` serves the bootstrap, and the native
arm over a `ProtocolPath<P>` serves the workspace's receipt of `Run.source`
and `Import.source` (D7). The native arm over an `ActorPath<R>` has no
caller, because the chassis mount holds its native proofs from spawn
results, so it waits for its first native caller, as does the guest arm
over a `ProtocolPath<P>`.

The bootstrap's `resolve_path` use goes away; the verb stays for its native
consumers and for guests that are handed text.

### D9. Deferred

- Adding or removing a unit while the engine runs; units exist from boot.
  With bundle roots named by key, removing a unit means dropping its
  `<key>-*` roots beneath `aether.component` along with its journal
  subtree; its roots are not a subtree of their own. ADR-0226 decision 2
  keeps the driver from dropping roots, so the dropping actor is the
  teardown's to name.
- Authenticating journal writes (ADR-0226), now per unit.
- A dedicated namespace for bundle roots,
  `aether.component/aether.bloomery.bundle:<key>-<digest>`, so the address
  itself shows why the root exists. A load cannot choose its namespace
  today (D4 lists the code); it needs a second trampoline type and the
  embedded resolver, inline-child spawns, and trampoline categorisation to
  accept it.
- A per-unit component host (rejected for now below). It is the route if
  dropping a unit by subtree ever matters.

**Revisit when** measurement shows a bottleneck in what D4 and D5 share or
place. The likely candidates are per-unit instance memory or instantiation
cost, pressure on the compiled-module map, and the one engine-level
component host as a mail hot spot. The first alternative to reexamine is
the per-unit component host.

### Compliance with the owner's design rules

| Rule | How this ADR complies |
|---|---|
| Addressing by type markers only; no roles, aliases, config slots; no `send_to_named`; ADR-0166 is the only grammar | Units by key, members by type (`UnitMember`), bundle roots by a name built from key and digest; no path fields in config; external text is canonical ADR-0166 paths; no new grammar. `key=root` entries follow `--http-secrets`. The workspace is reached by type and keys nothing by unit: its storage is the request's typed `source`, re-proven on receipt (D7). |
| No public `MailboxId` surface increase; stored state holds proofs; no serialized `MailboxId` | Every path is written from a type and a key and carries no position, and no config, kind, or record gains a `MailboxId` (ADR-0230 §1); `UnitBundle::name` returns a `LoadName`; the driver stores `ActorRef` / `ErasedActorRef`, and the workspace holds each request's source as a `ProtocolRef<ArtifactStorage>` resolved on receipt. `LoadComponent` is unchanged. The program role's existing `Live.child: MailboxId` is untouched. |
| Representations valid by construction | `UnitKey` is fallible on construction and decode and enforces the 191-byte limit that keeps `UnitBundle::name` infallible. |
| Unexportable invariants stay unexported | Config carries `UnitKey`s; `ActorRef` crosses nothing. A request carries its source as a `ProtocolPath`, an `ErasedActorPath` on the wire, and the workspace proves it again on receipt. |
| Static contract checks | A storage source is written from the journal's type and key and narrowed to `ArtifactStorage`, which compiles only if the journal covers it; the workspace's `resolve` on receipt re-proves liveness and the published rows (D7). `child_of(JournalActor)` places the driver; identity halves make bootstrap sends kind-checked. No runtime token or injection. |
| One valid way; every door needs a named production consumer | One relay path for every program API. `spawn_child` (embedder): the mount. `UnitBundle::name`: the driver. `UnitKey::new`: D3's lowering. `resolve`: the bootstrap for the guest arm over `ActorPath<R>`, the workspace for the native arm over `ProtocolPath<P>` (D7, D8); the other arms wait for a caller. `ActorPath::root` and `narrow`: the driver and the bootstrap; `ActorPath::child` and `UnitMember`: the bootstrap. `Stage`: the workspace's source-backed sink, answered by the journal owner. `ArtifactStorage`: `Run.source` and `Import.source`. |
| Pending replies never dropped | D6's hops park and answer once; nothing evicts. Each storage request is answered once, and a request whose source does not resolve is answered at receipt, never queued. |
| Fewer events, simpler architecture | No new actor, event, or record kind; one new mail kind pair (`Stage`). The workspace sheds its store, and isolation between units is the source each driver sets, with no table. The dedicated unit root is rejected for having no behaviour, and bundle roots keep the host they have. |
| No recursion on unbounded data | Mount iterates the unit list; relays are single hops. |
| No z-index; no drive or storage figures | None appear. |
| Bring-up is throwaway script components | The bootstrap stays a deletable component; the engine gains no bring-up mechanism. |

## Consequences

- One engine drives several journals, each with its own driver and bundle
  roots, and no member can fold or write another unit's log through a proof
  it was given. Every unit's runs go through the one workspace, whose source
  for them is the unit's journal.
- Every unit's runs share one host budget and one FIFO queue: free cores
  serve whichever unit's run is next, and a unit with many runs queued can
  delay another's (I-6).
- The workspace's storage seam is rewritten: a source-backed `TreeSource` /
  `TreeSink` with the worker-to-actor request path, `run/resolve.rs`'s checks
  moved onto it, and the stdin, log, and import paths moved off the batch.
  The container logic (image build, `write_tree`, output decode, step
  execution) is unchanged.
- Every artifact a run or import reads or writes is one mail to the
  journal and one reply. `Blob`s are shared in process, not copied,
  but a large tree costs one `ReadArtifact` per node and blob when the
  daemon lacks its image or a run writes it into a container.
- A run or import that does not end `Ok` leaves the artifacts it staged in
  the journal, cited by nothing.
- The workspace's storage and the bootstrap depend on the typed paths:
  `#[protocol]` and `CoveredBy<R>` (#6843) and published contract rows
  (#6844) are on main; ADR-0230's `ActorPath<R>` and the route's actor-type
  tag, ADR-0231's `ProtocolPath<P>`, and `ctx.resolve` over both are not.
- Memory grows per unit: a journal read cache share, a driver fetch cache,
  up to two closure walks in flight per journal, and one instance per
  (digest, unit) including dormant reactor roots. Compilation does not grow
  (D5).
- The ADR-0226 serialization point is per unit: units no longer wait on
  each other's slow reactors.
- Program API calls take two more in-engine hops. Workspace runs and HTTP
  fetches dominate them.
- Operators name units: every config, script, and external path gains a
  unit key, and a bundle root's path shows its unit and digest.
- Every unit's loads still pass through the one `aether.component` (D9's
  revisit trigger).

Follow-on issues, one concept each:

1. Embedder `BuiltChassis::spawn_child` and the multi-unit mount (D2, D3).
2. `UnitKey`, `UnitBundle::name`, and the driver naming its roots by unit
   key and digest (D4).
3. The compiled-module map keyed by hash (D5).
4. Program API relay through the invoker, the API list in each program's
   section record, and dropping the invocation's `depends` (D6).
5. One Bloomery workspace serves every unit (D7): `source` on `Run` and
   `Import`, resolved on receipt; the source-backed `TreeSource` /
   `TreeSink` replacing `JournalSource` / `JournalSink`; the worker-to-actor
   request path; `run/resolve.rs`'s checks over the source; step logs and
   import output staged to it; and `WorkspaceParams` and the journal
   dependency removed. A rewrite of the workspace's storage seam, not of its
   container logic. Depends on ADR-0230's `ActorPath<R>` and ADR-0231's
   `ProtocolPath` and its native `resolve` arm; the protocol and
   published-rows slices are on main (#6843, #6844).
6. Identity halves for the journal and driver crates (D8).
7. `UnitMember`, the route record's actor-type tag and the guest arm of
   `resolve` over an `ActorPath<R>` (ADR-0230 §3), and the bootstrap
   migration to paths written from unit keys (D8).
8. Rename `aether-workspace` to `aether-bloomery-workspace` and its actor's
   namespace to `aether.bloomery.workspace`; kind names stay (D7).
9. The journal as `ArtifactStorage` (D7): the `ArtifactStorage` protocol,
   the `Stage` kind pair (with `EncodedArtifact`'s bytes as a `Blob`) and
   the journal owner's handler, the driver writing its journal's path as
   each relayed `Run`'s `source`, the bootstrap naming its unit's journal on
   `Import`, and the read-cache split (I-6). Depends on ADR-0230's
   `ActorPath<R>` and ADR-0231's `ProtocolPath`; `#[protocol]` is on main
   (#6843).

Issue #6821 (`load_under` placement checked against the caller) is separate
and does not block these.

## Alternatives considered

- **Bundle roots loaded beneath their driver** (`Placement::Requester` on
  `LoadComponent`, this ADR's first draft). It gives each unit a subtree,
  but the host places a trampoline beneath a foreign parent only through
  `spawn_child_scoped`, whose `ChildOf` bound is checked against the host
  rather than the parent (#6821), so the placement would rest on that hole
  or need a `child_of(BundleDriver)` fact on the trampoline. The name in D4
  keeps units apart with no change to `LoadComponent`, and a subtree
  matters only for runtime teardown, which is deferred.
- **A per-unit component host.** `ComponentHostCapability` is
  `#[actor(singleton, root)]`, and an instanced variant under the same
  `aether.component` namespace would give that namespace two cardinalities;
  the address index marks it `Contradictory` and drops it, which breaks
  `aether.component/:NAME` short paths
  (`crates/aether-substrate/src/mail/registry/address.rs:293-316`). So it
  needs a new host type and namespace, and also:
  - an engine-wide compiler split: the `Engine`, `Linker`, module map, boot
    registry, and inventory egress the one host owns today;
  - a third `child_of` on the trampoline;
  - ownership checks on drop and replace, so one unit's host cannot touch
    another's roots;
  - MCP arguments to target a unit's host;
  - two or three more path levels against the depth cap of 8
    (`MAX_SCOPE_PATH_DEPTH`, `crates/aether-data/src/hash.rs:204`).

  Deferred (D9): it is the route if dropping a unit by subtree ever
  matters, and the first alternative the revisit reexamines.
- **Views keyed by journal on one shared root per digest.** One actor runs
  one handler at a time (ADR-0038, ADR-0087), so every unit's folds and
  program runs would serialize through it. It has no eviction for a
  removed unit's views, needs the program table keyed by (invoker, seq),
  and breaks I-2 (one actor folding several logs), I-4 (one root answering
  several drivers), and I-7 (a shared actor holding log state).
- **A dedicated unit root actor.** No handlers once the embedder can spawn
  beneath a proof; rejected under "why do we need this" (D1).
- **The journal spawns its own members from `wire`.** Needs a native
  self-reference door for `DriverParams.journal` and a readiness wait before
  the RPC bind; the embedder verb needs neither.
- **Shared program roots keyed by (invoker, seq).** One root serves both
  roles per digest (ADR-0225 decision 8), so sharing the program role
  splits one root per digest into two, or leaves a root whose reactor role
  belongs to one unit and program role to all. It also picks the relaying
  driver per call at run time (breaks I-5), and saves instance memory only,
  since D5 already compiles once.
- **Keep program APIs on declared dependencies and add a resolver that
  finds the nearest enclosing unit.** A new sealed `DependencyResolver`
  whose fold needs an arbitrary ancestor's lineage, where the relay reuses
  the fetch-on-miss path already built.
- **One workspace per unit, each with a disjoint share of the host
  budget** (an earlier draft of D7). It splits one
  resource manager into N. Each admits only against its own share, so one
  unit's cores sit idle while another's runs queue; handing each the whole
  host instead overbooks cores and memory by the unit count. It needs
  `HostBudget::split`, `UnitBudget`, two per-unit knobs, and the workspace
  as a unit member, all to divide what one queue admits whole.
- **Two-table funnel: one workspace holding every unit's store.** The
  chassis hands it each unit's store at composition, keyed by unit key, and
  each driver attaches under its key, so a run's store is looked up from
  its proven sender. Two mechanisms for one relationship (the store table
  and the attach table), and the workspace stays coupled to journal
  storage. A typed source on the request is one mechanism and no coupling.
- **The sender supplies storage** (the workspace casts `ctx.sender()` to
  `ProtocolRef<ArtifactStorage>` and reads and stages through it). The
  driver would relay every read and stage to its journal, which needs a
  relay from a handler row that declares its reply, and a static check
  needs a caller-protocol bound on the workspace's rows. The typed source
  address needs neither: the workspace talks to the journal directly.
- **One workspace that reads the requester's unit per request.** From the
  requester's lineage, it needs a fold of an arbitrary ancestor at run time;
  from a unit key on `Run`, it needs a key-to-store table (the two-table
  funnel).
- **The resolve checks moved to the source or the driver.** They compare a
  request with what the executor provides (its platform, its view of a
  toolchain and a tool table), which is executor knowledge copied into
  every storage provider.
- **Outputs held until the run ends, then handed back in one reply.**
  Keeps "nothing stored unless `Ok`", but sizes memory by the largest
  output or import rather than by a staging batch.
- **Several engines, one journal each.** Works today and needs no
  change, but pays a process, a substrate, and a compilation per journal,
  and is what an operator keeps for isolation across hosts.
- **Splitting one unit's work across several journals.** Loses the order
  between the parts, since no order exists across units (I-1, I-3).
- **A path field per actor in the bootstrap config** (the bootstrap on
  `main`). A config slot per actor is the shape the addressing rules
  reject, and a path read from config is untyped, so each send through it
  is unchecked by kind or pays a cast at receipt. A `UnitKey` and the actor
  types write the same paths every time, typed at compile time.
