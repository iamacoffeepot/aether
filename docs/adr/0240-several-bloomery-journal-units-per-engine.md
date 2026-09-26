# ADR-0240: Several Bloomery Journal Units Per Engine

- **Status:** Proposed
- **Date:** 2026-09-26

Amends [ADR-0226](0226-native-bundle-driver.md) decisions 1 and 2,
[ADR-0229](0229-program-cap-apis-are-extra-run-arguments.md) decision 2 and
its 2026-09-24 amendment, and [ADR-0237](0237-workspaces-run-steps-over-trees.md)
decisions 8 and 9. Resolves the "several journals per engine" deferral in
[ADR-0225](0225-reactor-bundles-load-by-digest.md) and ADR-0226. Gives
[ADR-0230](0230-proven-actor-references.md) §3's `Address<R>` row its first
consumer.

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
| Program role | `crates/aether-bloomery-program/src/root.rs`, `expand/programs.rs` | The role's only state is `live: BTreeMap<u64, Live<H>>`, keyed by `Invoke.seq`. Each invocation is an inline child named `Subname::Named(seq)`. A second `Invoke` with a live seq is refused `"seq already live"`. A fetch-on-miss already relays invocation → root → the `Invoke`'s sender. |
| Program APIs | `expand/programs.rs` `expand_send_pending` | The invocation declares `depends(api_target::…)` and sends a captured call through `ctx.actor_ref::<T>()`, a root-singleton proof. |
| Reactor role | `crates/aether-bloomery-reactor/src/root.rs` | One `Owner` with a cursor. `Warm` and `Event` refuse anything but `cursor + 1` with `OutOfSequence`, and a fold failure poisons the role. The loaded role is a fold of one journal's log. |
| Workspace | `crates/aether-workspace/src/config.rs` | `WorkspaceConfig` is the actor's `Config`: the host's `cpuset`, `budget_memory_bytes`, and the per-run defaults. Each actor that resolves it claims the whole host. |
| Module reuse | `crates/aether-component/src/component/runtime/module_cache.rs` | One slot keyed by sha256. Back-to-back loads of one digest compile once; any other load between them evicts the slot and the next load recompiles. |
| Bootstrap | `crates/aether-bloomery-bootstrap` | Config carries two `ActorPath`s (journal, driver), proven at `wire` with `resolve_path`, sent through unchecked `ErasedActorRef`s. `depends(WorkspaceCapability)` reaches the root singleton. |
| Identity halves | `aether-workspace` vs journal and driver crates | `aether-workspace` has an always-on `WorkspaceCapability` marker and a `runtime` feature (ADR-0122). The journal crate depends on `aether-substrate` and `rusqlite` unconditionally, and the driver on `aether-substrate`, so a guest cannot name `JournalActor` or `BundleDriver`. |

Who can place a child beneath an existing actor today:

| Spawner | Verb | Parent |
|---|---|---|
| The parent itself, from a handler | `NativeCtx::spawn_child::<C>` (staged; `C: ChildOf<A> + Instanced`) | the ctx's own actor, never a caller-supplied one |
| The component host, for a wasm trampoline | `NativeCtx::spawn_child_scoped::<C>(parent: ErasedActorRef, …)` (`#[doc(hidden)]`) | a proven foreign parent. Its only request path is `aether.component.load_under`, documented as test-harness only, which takes the parent as `ActorPath` text |
| An embedder | `BuiltChassis::spawn_actor::<A>` | none: `A: Root` only |
| A guest | inline children and detached siblings | its own inline cluster |

No native ctx has a parent-reference verb (issue #6796 records this). An
embedder can look up a live child with `BuiltChassis::child::<P, C>` but
cannot spawn one.

### What collides when a second journal appears

| Shared today | Why a second journal breaks it |
|---|---|
| The bundle root of a digest | Its reactor role folds one log in order. A second driver's `Warm` / `Event` arrive out of sequence and poison it. Its program role keys children by `seq`, which is unique only within one journal. |
| The root's name | Two drivers loading one digest under the component host collide on `SubnameInUse`. |
| `aether.workspace` | One artifact store, one journal. A second journal's runs would write into the first. |
| Engine-wide budgets | A second workspace or journal owner resolving its own `Config` claims the whole host's cores, memory, or read cache again. |
| Addressing | Config, the bootstrap script, and `xtask import-commit` name the journal and driver by fixed paths. |

### Constraints from the Ergo specification

Ergo is the language that compiles down to Bloomery reactors and programs.
Its specification fixes what a world's log must be:

- One log per world with one total order ≺ over every turn. The formalism's
  header states "heads are a fold over one log"; formalism §11 defines the
  tick as a fixed list of slots.
- Parallelism lives inside that one order: core-calculus §6.2 (commit order
  of conflicting pairs follows ≺, non-conflicting firings commit in any
  order, speculative work validated first) and §6.3 (the schedule
  description the platform receives).
- Instances are World state inside the one log: `Tile.inst: InstanceId =
  Main | Inst(a)` (systems, collision).
- The clock and the seed are facts of the log (core-calculus §2.1), and the
  seed is one per game (systems, rng §3).
- Grammar gap G-3: a run returns an ordered log append with causal
  addresses and FIFO order.
- core-calculus §6.4: "This file designs neither journals nor executors; the
  owner is rethinking the layer around them."

A journal per instance, region, or player would break teleports, hit
landing, drops, and dispatch, which all need ≺ across both sides.

### The fork seam

Bundles (reactors, pure programs, and their packaging) are Ergo's compile
target and Bloomery's build target alike. The log and the executor are where
the two may diverge:

| | Build unit (Bloomery today) | World unit (Ergo, future) |
|---|---|---|
| Log order | `seq` and `cause`, with head fences | one total order by (tick, slot, seq) |
| Executor | the driver, which serializes calls per root | parallel commit over a tick's turns (core-calculus §6.2) |
| Clock and seed | none | one Clock and one seed per world |
| Workspace | yes | no |

Bloomery may fork into its own system focused on building. This ADR does not
decide that. It fixes the part both sides share: a unit owns a log and
everything that folds it, and bundles are engine-wide.

## Decision

### Terms

| Term | Meaning |
|---|---|
| **unit** | One log plus every actor whose state derives from that log. |
| **unit key** | The `LoadName` that names a unit, unique per engine. |
| **unit root** | The unit's log owner, the root of the unit's lineage. For a build unit it is `JournalActor` at `aether.bloomery.journal:<key>`. |
| **member** | An actor beneath a unit root: the driver, the workspace, and the driver's bundle roots. |
| **build unit** | A unit whose log is a Bloomery journal and whose executor is the bundle driver. The only kind this ADR builds. |
| **world unit** | A unit whose log is one Ergo world. Its log and executor types are not decided here. |
| **engine-shared** | Held once per engine and keyed by content hash, or holding no log state. |

```text
aether.bloomery.journal:<key>                          unit root: the journal
├── aether.workspace:workspace                         member
└── aether.bloomery.driver:driver                      member
    ├── aether.embedded:<digest>                        bundle root (one per digest this unit loads)
    │   └── aether.bloomery.bundle.invocation:<seq>     inline child per live invocation
    └── aether.embedded:<digest>

engine-shared: aether.component (and its compiled-module cache), aether.http,
the engine blob store, the RPC server, the inventory
```

### Invariants

Each rule is followed by the mechanism that makes breaking it impossible or
refused, the concrete way it would be broken and why that way is closed, and
what it forces elsewhere.

**I-1. A world unit holds exactly one Ergo world; nothing smaller than a
world gets its own journal. A build unit holds no world.**

- *Upheld by:* units exist only at boot. `JournalActor` is `#[actor(instanced,
  root)]`, and a root is spawned only through `BuiltChassis::spawn_actor`,
  which only the chassis mount holds. No ctx verb births a root:
  `NativeCtx::spawn_child` requires `C: ChildOf<A>`, and a guest spawns only
  inline children and siblings. So no
  bundle, program, or reactor can create a journal for an instance, a
  region, or a player. Which world a unit holds is its config entry, read
  once at boot.
- *Would be violated by:* a journal per instance, region, or player. Closed
  by construction (no runtime door births a unit) and by semantics:
  teleports, hit landing, drops, and dispatch need ≺ across both sides, and
  I-3 says no order exists across units.
- *Implication:* instances are World state (`Tile.inst`); a world scales
  inside its log (core-calculus §6.2), never by adding logs.

**I-2. Every actor whose state derives from a log sits beneath that log's
owner.**

- *Upheld by:* lineage. The driver and the workspace are
  `#[actor(instanced, child_of(JournalActor))]` and are born beneath their
  journal (D2). Bundle roots are loaded beneath the driver that asked for
  them (D4), so the position a root holds names its unit.
- *Would be violated by:* one bundle root of a digest serving two units,
  the shape on `main`, where every root is a child of `aether.component`.
  Closed: roots are named by digest beneath their driver, so two units'
  roots of one digest are two actors at two positions, and a driver holds
  proofs only for the roots its own loads returned.
- *Implication:* one bundle root per (digest, unit); the compiled module is
  shared instead (D5).

**I-3. A unit's log is totally ordered and local: every entry's position is
its own journal's `seq`, and no entry is ordered against another unit's.**

- *Upheld by:* each unit opens its own journal root (a separate SQLite
  database and `blobs` directory under its own lock, ADR-0220); `seq` is
  that database's. No kind carries a position in another journal, and no
  actor appends to two journals: each driver holds exactly one
  `ActorRef<JournalActor>`, handed at birth and never replaced.
- *Would be violated by:* a driver retargeted by mail, or a cross-unit
  `cause`. Closed: `ActorRef` has no codec, so no mail can carry a journal
  to write to; `AppendRecords` requires every cause in `1..=expected_seq`
  of the journal it is sent to.
- *Implication:* anything that needs ≺ between two things puts both in one
  unit (I-1). A world unit's Clock and seed are facts in its own log, so
  each world has its own by construction (core-calculus §2.1).

**I-4. A driver drives exactly one journal, and a bundle root answers
exactly one driver.**

- *Upheld by:* `DriverParams { journal, workspace }` is filled at birth
  from the journal's and the workspace's spawn results (D2). A root sends
  its replies to its `Invoke` / `Warm` / `Event` sender, and only its
  parent driver holds a proof of it: the load reply goes to the requester
  alone (ADR-0230 §3).
- *Would be violated by:* a second driver sending `Invoke` to a root of
  another unit. Closed in the type path (it holds no proof); not closed
  against a hand-written path, because ADR-0230's 2026-09-25 amendment lets
  any loaded component prove any `Live` path. That is the known
  unauthenticated-writes gap of ADR-0226, unchanged here.
- *Implication:* `Invoke.seq` is unique among the invocations a root
  holds, so the program role keeps `live` keyed by `seq` with no change.

**I-5. An invocation reaches providers only through its own unit's driver.**

- *Upheld by:* D6. The generated invocation holds one proof, its parent
  root. A program API call goes invocation → root → the `Invoke`'s sender
  (the driver) → the provider proof the driver was born with. The
  workspace is an instanced child, so `#[actor(depends(WorkspaceCapability))]`
  no longer compiles: `DependsOn<R>` requires `R: Singleton` with a
  `DependencyResolver` of `One` or `Embedded`.
- *Would be violated by:* an invocation resolving the workspace through
  `A::NAMESPACE` as a root singleton (ADR-0229 today). Closed: the type is
  no longer a root singleton. A shared program root choosing a workspace
  per call at run time. Closed by I-2: roots are per unit.
- *Implication:* ADR-0229's binding moves from a declared dependency on the
  invocation to a relay through the invoker (D6).

**I-6. An engine-wide budget is handed out once: per-unit actors receive
shares of it, never copies.**

- *Upheld by:* `HostBudget::split` is the only constructor of `UnitBudget`,
  the workspace's `Config` (D7). It refuses overlapping core sets and
  memory shares that sum past the host's. The journal read-cache budget is
  divided the same way.
- *Would be violated by:* each workspace resolving `WorkspaceConfig` off
  argv and env, which is what composing N copies of today's actor would do.
  Closed: the workspace's `Config` is `UnitBudget`, which no config source
  can produce.
- *Implication:* an idle unit's cores are not lent to a busy one; a shared
  admission actor is deferred (D10).

**I-7. What is engine-shared is keyed by content hash or holds no log
state.**

- *Upheld by:* the compiled-module cache is keyed by sha256 (D5); the engine
  blob store dedups by hash (ADR-0238); `aether.component`, `aether.http`,
  the RPC server, and the inventory keep no fold of any journal.
- *Would be violated by:* an engine-wide actor that caches a view of one
  unit's log. Closed only by review: the chassis composes no such actor,
  and a new composed actor that reads a journal is a change to this ADR.
- *Implication:* sharing a bundle across units costs one instance per unit
  and one compilation per engine.

**I-8. A unit is named by its key, a member by its type beneath the unit,
and no position crosses a boundary.**

- *Upheld by:* D8. Config carries unit keys (`LoadName`, validated on
  decode). Code builds `Address::<JournalActor>::root_at(key)` and proves it
  through `ctx.resolve::<R>`, which mints `ActorRef<R>`; a member's address
  is `member_address::<C>(unit)`, derived from the unit's proof and `C`'s
  fixed key. `ActorRef` has no codec.
- *Would be violated by:* a config slot holding the driver's path (the
  bootstrap today), a role or alias per unit, `send_to_named`, or a
  `MailboxId` in config. Closed: the bootstrap's config loses its path
  fields, `send_to_named` is deleted, and the new `Root` address form
  carries a key and no position.
- *Implication:* the journal and driver crates split identity from runtime
  (ADR-0122) so a guest can name their types and its sends are
  kind-checked.

### Open questions for the owner

Each has a recommendation. None is decided silently.

1. **Unit content** (changes I-1). Is a unit at most one whole Ergo world,
   or non-game Bloomery work, with a written ban on sub-world journals?
   *Recommendation: yes, as I-1 states.*
2. **Where the Ergo executor lives** (changes I-2, I-4, and the fork seam).
   The unit's driver, a sibling member beneath the world unit, or
   undecided. *Recommendation: a member beneath the world unit, of its own
   type, never the `BundleDriver`: a driver that serializes calls per root
   is the wrong shape for parallel commit over a tick's turns. Its log
   order, scheduling, and whether it owns the log are left to the executor
   ADR. This is the fork seam.*
3. **Does this ADR settle the core-calculus §6.4 rethink?** (changes I-1,
   I-3). *Recommendation: it settles ownership and topology only (a unit
   owns one log and everything that folds it; bundles are engine-wide) and
   leaves executor semantics, the world log's order, and whether Bloomery
   forks to that rethink.*
4. **Unit root: the journal, or a dedicated unit actor?** (changes I-2,
   I-8). *Recommendation: the journal.* A dedicated
   `aether.bloomery.unit:<key>` would be an actor with no handlers once the
   embedder can spawn children beneath a proof (D2), which fails "why do we
   need this". It becomes worth having only if a unit kind needs behaviour
   of its own beside its log, such as a world unit coordinating its log and
   executor; that unit kind can choose it then.
5. **Program roots: per unit, or shared per engine?** (changes I-2, I-4,
   I-5). The direction handed to this ADR shared program roots once their
   live table and child names were keyed by (invoker, seq). *Recommendation:
   per unit.* One root serves both roles per digest (ADR-0225 decision 8),
   so sharing the program role means two roots per digest or a root whose
   reactor role belongs to one unit and program role to all. A shared root
   would also pick each invocation's workspace at run time (breaks I-5).
   The saving is instance memory only, since D5 already compiles once.
6. **The native twin of `resolve`** (changes I-8). Issue #6796 says the
   `Address<R>` door lands as one verb on each ctx in the same change, and
   also that no ctx verb lands without a named production consumer. The
   guest verb's consumer is the bootstrap script. The native verb has none
   in this ADR: the chassis mount holds its proofs from spawn results.
   *Recommendation: land the twins together, as #6796 decides; both run the
   same registry read (`Registry::live_child`), so the native verb adds no
   new mechanism, and the pair is one door with one consumer.*

### D1. Unit topology (serves I-1, I-2, I-3, I-7)

| Held | Scope | Why |
|---|---|---|
| Journal (log, artifact files, read cache) | per unit | I-3 |
| Driver (program queue, reactor following, fetch cache) | per unit | I-2, I-4 |
| Bundle roots (program role and reactor role) | per (digest, unit) | I-2 |
| Workspace (artifact store handle, run-key estimates, share of the host) | per unit | I-5, I-6 |
| Component host, compiled modules by hash | per engine | I-7 |
| Engine blob store (in-memory bytes by hash) | per engine | I-7 |
| HTTP egress, RPC server, inventory | per engine | I-7 |
| Durable artifact files | per unit (its journal root's `blobs`) | I-3: each root is one lock and one writer (ADR-0237's 2026-09-25 amendment) |

A build unit's members are `JournalActor`, `WorkspaceCapability`, and
`BundleDriver`. A world unit follows the same rule (its log owner is its
root, and everything that folds its log is beneath it); its types wait for
open question 2.

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
2. `spawn_child::<JournalActor, WorkspaceCapability>(journal, Named("workspace"),
   unit_budget, WorkspaceParams { artifacts })`;
3. `spawn_child::<JournalActor, BundleDriver>(journal, Named("driver"), limit,
   DriverParams { journal, workspace })`.

Only after every unit is mounted does the RPC bind gate open (issue #6399's
order, now over every unit). `Mounted` becomes one `MountedUnit { journal,
workspace, driver }` per key.

The embedder verb is new. Its consumer is the mount. It reuses
`SpawnBuilder::new_child`, which the staged handler verb already builds
from a parent identity, and it is bounded by the same `ChildOf` placement
fact. The alternative, the journal spawning its own members from `wire`,
needs a native self-reference door to fill `DriverParams.journal` and a
readiness wait before bind; it is rejected below.

### D3. Chassis config is a list of units (serves I-1, I-3, I-8)

| Knob | Shape | Lowered to |
|---|---|---|
| `AETHER_BLOOMERY_UNITS` / `--bloomery-units` | comma-separated `key=root` entries, the precedent `--http-secrets` sets | `Vec<UnitSpec { key: LoadName, root: PathBuf }>` |
| `AETHER_BLOOMERY_JOURNAL` | removed | — |
| `closure_limit_bytes` | unchanged, a per-read ceiling | one `ClosureLimit` for every driver |
| `read_cache_bytes` | now the engine's total | divided equally among units (D7) |

Lowering refuses boot, naming the key, for: an empty list, a key that is
not a `LoadName`, a repeated key, or two entries whose roots are the same
directory after canonicalization. The root lock stays the guard across
processes. Each root is opened before wasmtime, as today, so a held root
costs no boot.

### D4. Bundle roots load beneath the driver (serves I-2, I-4)

`LoadComponent` gains a placement:

```rust
pub enum Placement {
    /// Beneath `aether.component`, as every load is today.
    Host,
    /// Beneath the requester: the component host places the trampoline
    /// beneath the envelope sender it stamped.
    Requester,
}
```

The driver loads every bundle with `Placement::Requester`, so a root sits
at `<driver>/aether.embedded:<digest>`. The parent is the stamped sender, a
proof the host already holds, so no text or position is carried.
`aether.component.load_under` stays the harness's text seam.

This amends ADR-0226:

- **Decision 1.** One driver per unit. It is its roots'
  parent. Roots are shared by digest within one unit; `Invoke.seq` is
  unique within one journal, which is now one driver's.
- **Decision 2.** A root's name is still the lowercase-hex digest and it is
  still never dropped; the name is unique beneath its driver.

### D5. One compiled module per digest per engine (serves I-7)

`ModuleCache` keeps each compiled module by content hash while any live
trampoline of that hash exists, replacing the one slot. Bundle roots are
never dropped (D4), so a bundle compiles once per engine however many units
load it and in whatever order. A unit's cost for a bundle it shares is one
instance: its linear memory and its root's fold.

### D6. Program API calls relay through the invoker (serves I-4, I-5)

| Hop | Holds | Pending reply |
|---|---|---|
| Invocation → its root | the parent proof | the invocation's `waiting`, by request id, as today |
| Root → the `Invoke`'s sender | `invokers`, as the fetch-on-miss relay already does | a deferred reply per relayed call, answered once |
| Driver → provider | the proofs it was born with: `workspace` (its unit's), `HttpCapability` (declared dependency) | the driver's `callers`, as for a fetch |

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
- The driver maps `Http` to `HttpCapability` and `Workspace` to its unit's
  workspace, and refuses anything else with `Refusal::Refused`, as the
  invocation does today.
- The driver declares `depends(ComponentHostCapability, HttpCapability)`, so
  a missing HTTP capability refuses the driver's birth.

This amends ADR-0229 decision 2 and its 2026-09-24 amendment: bindings no
longer resolve through `A::NAMESPACE`. The sealed `InjectedApi` set and the
SDK table stay; the table now names what the driver maps.

No hop drops or evicts a parked reply; each is answered exactly once or
abandoned when its actor closes, as the driver's `Drop` does today.

### D7. The workspace is a unit member with a share of the host (serves I-5, I-6)

```rust
pub struct HostBudget { /* cpuset, budget_memory_bytes, from WorkspaceConfig */ }
pub struct UnitBudget { /* a disjoint core set and a memory share; no public constructor */ }

impl HostBudget {
    pub fn split(&self, shares: &[(LoadName, Share)]) -> Result<Vec<(LoadName, UnitBudget)>, SplitError>;
}
```

| Setting | Scope | Source |
|---|---|---|
| Endpoint, TLS files, import and output bounds, per-run defaults, headroom, `pids_limit` | engine | `WorkspaceConfig`, resolved once and declared as a config member |
| Host core set and memory budget | engine | `WorkspaceConfig.cpuset`, `budget_memory_bytes` |
| A unit's cores and memory | unit | `AETHER_WORKSPACE_UNIT_CPUSETS` (`key=0-3,…`) and `AETHER_WORKSPACE_UNIT_MEMORY_BYTES` (`key=N,…`) |
| Run-key estimates | unit | the actor's own state, rebuildable, never journaled (ADR-0237 decision 9) |

With one unit and neither per-unit knob set, the unit gets the whole host.
With more than one unit, every unit names both, and `split` refuses boot for
overlapping core sets, a core outside the host's set, or memory shares that
sum past the host's budget.

`WorkspaceCapability` becomes `#[actor(instanced, child_of(JournalActor))]`
with `Config = UnitBudget` and `Params = WorkspaceParams { artifacts }`, its
own unit's store. The daemon's image store stays shared by every unit; it is
a rebuildable derivative labelled by environment digest (ADR-0237 decision
8), so two units importing one environment converge on one image.

The journal's read cache is divided the same way: `read_cache_bytes` is the
engine total and each journal gets an equal share.

This amends ADR-0237 decision 8 (one workspace actor per unit) and decision 9 (the host budget is the engine's, handed out as
per-unit shares; admission stays FIFO within a unit's share).

### D8. Addressing: units by key, members by type (serves I-8)

| Piece | Change |
|---|---|
| `AddressForm` | gains `Root { key: Option<LoadName> }`: the root instance of `R` keyed `key`. It carries no position. `Address::<R>::root_at(key)` is bounded `R: Root + Instanced`. |
| `NativeCtx::resolve::<R>` / `WasmCtx::resolve::<R>` | `(&Address<R>) -> Result<ActorRef<R>, ResolveError>`, one verb on each ctx in one change. Folds the address with `R`'s resolver and proves the route with the published-route read `Registry::live_child` already makes; only `Live` mints. An `Exact` address mints only when the route's actor type is `R`. The guest crosses one host import. Takes the name ADR-0230 reserved for this door. |
| `UnitMember` | a trait in the journal identity half: `ChildOf<JournalActor> + Instanced` with a fixed key (`driver`, `workspace`). `member_address::<C: UnitMember>(unit: ActorRef<JournalActor>) -> Address<C>` is `child_address::<JournalActor, C>(unit, C::key())`. |
| `aether-bloomery-journal`, `aether-bloomery-driver` | split per ADR-0122: an always-on, `no_std` identity (the marker, its handled kinds, `UnitMember`) and a `runtime` feature carrying the actor, `aether-substrate`, and `rusqlite`. |
| Bootstrap config | `journal` and `driver` paths are replaced by `units: Vec<LoadName>`. At `wire` it resolves each unit, then its driver and workspace by type; every send is `send_to(ActorRef<R>, &K)`, kind-checked. |
| External callers (MCP, `xtask import-commit`) | name a unit's member by its canonical ADR-0166 path, `aether.bloomery.journal:<key>/aether.bloomery.driver:driver`; `import-commit` takes the unit key. |

The bootstrap's `resolve_path` use goes away; the verb stays for its native
consumers and for guests that are handed text.

### D9. The fork seam (serves I-1, I-3)

| Common to every unit kind | Per unit kind |
|---|---|
| Bundle format, digest naming, `Warm` / `Event` / `Invoke` protocols, compiled modules (D5) | the log owner's type and order |
| One log per unit, members beneath it (I-2) | the executor's type |
| Budgets handed out as shares (I-6) | whether it has a workspace, a Clock, a seed |

A world unit reuses bundles unchanged and supplies its own log and executor.
Whether those live in this repository or a forked one does not change a
bundle.

### D10. Deferred

- A world unit's log, executor, Clock, and seed (open questions 2 and 3).
- Lending idle cores between units through a shared admission actor (I-6).
- Adding or removing a unit while the engine runs; units exist from boot.
- Authenticating journal writes (ADR-0226), now per unit.

### Compliance with the owner's design rules

| Rule | How this ADR complies |
|---|---|
| Addressing by type markers only; no roles, aliases, config slots; no `send_to_named`; ADR-0166 is the only grammar | Units by key, members by type (`UnitMember`); no path fields in config; external text is canonical ADR-0166 paths; no new grammar. `key=root` entries follow `--http-secrets`. |
| No public `MailboxId` surface increase; stored state holds proofs | The new address form carries a key only; the driver stores `ActorRef` / `ErasedActorRef`; `Placement::Requester` carries no id. The program role's existing `Live.child: MailboxId` is untouched. |
| Unexportable invariants stay unexported | Config carries `LoadName` keys; `ActorRef` crosses nothing. |
| Static contract checks | `child_of(JournalActor)` and `DependsOn` make a root-singleton workspace binding a compile error; identity halves make bootstrap sends kind-checked. No runtime token or injection. |
| One valid way; every door needs a named production consumer | One relay path for every program API. `spawn_child` (embedder): the mount. `Placement::Requester`: the driver. `resolve` twins: the bootstrap (open question 6). `Root` form: the bootstrap. `HostBudget::split`: the mount. |
| Pending replies never dropped | D6's hops park and answer once; nothing evicts. |
| Fewer events, simpler architecture | No new actor, event, or record kind. The dedicated unit root is rejected for having no behaviour. |
| No recursion on unbounded data | Mount iterates the unit list; relays are single hops. |
| No z-index; no drive or storage figures | None appear. |
| Bring-up is throwaway script components | The bootstrap stays a deletable component; the engine gains no bring-up mechanism. |

## Consequences

- One engine drives several journals, each with its own driver, workspace,
  and bundle roots, and no member can fold or write another unit's log
  through a proof it was given.
- Memory grows per unit: a journal read cache share, a driver fetch cache,
  up to two closure walks in flight per journal, and one instance per
  (digest, unit) including dormant reactor roots. Compilation does not grow
  (D5).
- The ADR-0226 serialization point is per unit: units no longer wait on
  each other's slow reactors.
- Program API calls take two more in-engine hops. Workspace runs and HTTP
  fetches dominate them.
- Operators name units: every config, script, and external path gains a
  unit key.
- A world unit, when it comes, has a place: beneath its own root, sharing
  bundles, owning its log and executor.

Follow-on issues, one concept each:

1. Embedder `BuiltChassis::spawn_child` and the multi-unit mount (D2, D3).
2. `Placement::Requester` on `LoadComponent`, and the driver loading
   beneath itself (D4).
3. The compiled-module map keyed by hash (D5).
4. Program API relay through the invoker, the API list in each program's
   section record, and dropping the invocation's `depends` (D6).
5. `HostBudget::split`, the workspace as a unit member, the read-cache split
   (D7).
6. Identity halves for the journal and driver crates (D8).
7. `AddressForm::Root`, the `resolve` twins, `UnitMember`, and the bootstrap
   migration (D8), which also amends ADR-0230 §3's `Address<R>` row.

## Alternatives considered

- **A dedicated unit root actor.** No handlers once the embedder can spawn
  beneath a proof; rejected under "why do we need this" (open question 4).
- **The journal spawns its own members from `wire`.** Needs a native
  self-reference door for `DriverParams.journal` and a readiness wait before
  the RPC bind; the embedder verb needs neither.
- **Shared program roots keyed by (invoker, seq).** Splits one root per
  digest into two, and picks a workspace per call at run time (open
  question 5).
- **Keep program APIs on declared dependencies and add a resolver that
  finds the nearest enclosing unit.** A new sealed `DependencyResolver`
  whose fold needs an arbitrary ancestor's lineage, where the relay reuses
  the fetch-on-miss path already built.
- **One engine-wide workspace that picks a unit's store per request.** It
  would have to read the requester's unit from its lineage at run time.
- **Replicated budgets.** Each workspace claiming the whole host overbooks
  cores and memory by the unit count.
- **Several engines, one journal each.** Works today and needs no
  change, but pays a process, a substrate, and a compilation per journal,
  and is what an operator keeps for isolation across hosts.
- **Journals per instance, region, or player.** Breaks ≺ across teleports,
  hits, drops, and dispatch (I-1).
- **Carry `Address<JournalActor>` in the bootstrap config.** Its decode
  accepts `Beneath` and `Exact` forms, which carry positions from another
  session; a `LoadName` key carries none.
