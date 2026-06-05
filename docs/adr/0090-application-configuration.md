# ADR-0090: Application Configuration

- **Status:** Proposed
- **Date:** 2026-05-27

## Context

The application is configured by ~50 `AETHER_*` environment variables (plus a
few un-prefixed ones like `GEMINI_API_KEY`, `PERF_K`), each parsed ad-hoc at
its point of use:

```rust
env::var("AETHER_HTTP_MAX_BODY_BYTES").ok().and_then(|s| s.parse().ok()).unwrap_or(DEFAULT)
```

There is no central declaration of the knob set. Four concrete pains follow:

- **No validation** — a typo'd `AETHER_PREF_TIER` silently no-ops; nothing
  catches a known key set to garbage either (the chain just falls through to the
  default).
- **No discovery** — nothing lists every knob, its default, and what it does.
- **Inconsistent naming** — `AETHER_WORKERS` (runtime pool) vs
  `AETHER_PERF_WORKERS` (perf sweep); the `AETHER_LATENCY_*` / `AETHER_PERF_*`
  split; `PERF_K` / `PERF_K_TREND` don't carry the `AETHER_` prefix at all.
- **Copy-pasted parsers** — `parse_env_u64` is duplicated
  (`dag/validator.rs:552`, `dag/executor.rs:1226`); `parse_env_usize`,
  `env_or_default` are near-identical one-offs. A Qodana `DuplicatedCode` magnet
  (the iamacoffeepot/aether#1055 smell).

This ADR **supersedes iamacoffeepot/aether#1055** — its shared-parser dedup
becomes the typed accessors on this layer.

### What already exists (the patterns to build on)

The repo already converged on a *good* shape, just without a backbone:

- Per-subsystem **config structs with a resolver method**: `HttpConfig`,
  `GeminiConfig`, `AnthropicConfig`, `AudioConfig` (`from_env()`),
  `NamespaceRoots::from_env()`, `PersistConfig::from_env()`, the DAG validator
  `Caps::from_env()`. Each has a typed `Default` and a non-env constructor for
  tests.
- These compose into one per-chassis env struct — `DesktopEnv` / `HeadlessEnv` /
  `HubEnv`, each with a single `from_env()` that calls the sub-configs'
  resolvers. The chassis mains do nothing but `XEnv::from_env()` then `build()`.
- Native actors **already receive typed config at construction**:
  `NativeActor { type Config: Send + 'static; fn init(config: Self::Config,
  ctx: &mut NativeInitCtx) -> Result<Self, BootError> }`. The chassis builds the
  `Config` (from env today) and hands it in. The empty case is `type Config = ()`.

So the missing pieces are a *backbone* under the structs (typed resolution +
validation + discovery), and a *second source* above env (arguments).

### The forcing signal: configuration is frozen and fleet-global

Config enters the process tree once, at `exec`, and cannot change in place:

- the chassis resolves config only from env (`XEnv::from_env()`); the bin mains
  never read `argv`,
- a hub-spawned substrate inherits the hub's full environment (no `env_clear`),
  the hub inherits the tunnel's, the tunnel the launching shell's,
- a process's environment is frozen at `exec`.

Two consequences. First, correcting one key (e.g. the dev shell exported
`GEMINI_API_KEY` but the cap read `GOOGLE_API_KEY`) meant relaunching the whole
tunnel, dropping the MCP session — `POST /admin/restart-hub` can't help because
it re-forks the hub from the same frozen env. Second, and structurally:
configuration is **global to the entire fleet**. There is no way to spawn two
substrates with different config — one with `aether.gemini` under key A, another
with it disabled — because they all inherit one environment from the root of the
tree. That is exactly the axis the "substrate as a general application host"
direction needs.

The per-spawn transport already exists and is unused: `spawn_substrate`
forwards per-spawn `args` end-to-end (`SpawnEngine { binary_path, args }` →
`Command::args(&mail.args)` at `engine/server.rs:146`, fed from `aether-mcp`
`tools.rs`), but the chassis mains never parse them — they only call
`XEnv::from_env()`. So per-application configuration is mostly **argument
parsing + a precedence model**, not new plumbing.

## Decision

Five decisions, taken together.

### 1. Distributed typed structs over a shared resolution backbone — not a central registry

Keep the per-subsystem config struct as the unit (the pattern already in the
wild here): each subsystem owns its `Config` struct, in its own crate, declaring
its own knobs. Replace the hand-written `from_env()` bodies and the ad-hoc
`env::var(...).parse()` chains with a **derive macro that resolves the struct
from a layered source-stack**. A central *registry table* (one declarative list
of every knob) is rejected (see Alternatives) — it would rewrite every existing
struct and centralize ownership away from the subsystem that knows the knob.

Discovery and validation, which a central table would have given for free, come
instead from the derive's compile-time metadata: the macro emits a description
of each field (name, env key, type, default, doc) that a `--config` dump walks.
This is "distributed structs, *unified* parse + validate + discover."

**Adopt an external crate rather than hand-roll the macro.** None of
`confique` / `figment` / `twelf` / `conf` is currently in the tree (`clap` and a
config crate are both absent; `serde 1.0.228` is present). Lead recommendation:
**`confique`** —

- `#[derive(Config)]` per struct, `#[config(env = "AETHER_X")]`,
  `#[config(default = ...)]`, `#[config(nested)]`, `#[config(validate = ...)]` —
  a near-exact map onto the existing structs (per-field env key + typed default +
  composed sub-configs + validation), so the migration is mechanical;
- a `Meta` / `Config::META` introspection surface plus template generators
  (`confique::toml::template`, …) — **this is the discovery story** the issue
  asks for, generated from the same declaration, no second source of truth to
  drift;
- a layered model (`Layer` partials combined via `with_fallback`,
  `default_values` last) that makes adding an `argv` layer above `env` a
  one-line change — load-bearing for decision 3's staging.

`confique` has no native `argv` source (env + file + any serde `Deserializer`);
arguments arrive as a `clap`-built partial layered highest, when step 2 lands.
The alternatives (`twelf` with native `Layer::Clap`; `conf`; `figment`'s
provider-merge) are weighed in Alternatives — the crate pick is the main item
for review on this ADR.

**Why `confique` over the merge-into-serde family (`config-rs`, `figment`).**
The choice turns on one axis: a *declared, introspectable, compile-time* schema
(confique) versus *runtime source-merging into a plain serde struct* (`config-rs`
0.15, `figment` 0.10). The latter pair is one family — a builder/provider chain
(`ConfigBuilder::add_source` / `Figment::merge`) that deserializes whatever keys
exist into a `#[derive(Deserialize)]` struct. Both miss the two things this ADR
buys:

- **Discovery (the issue's #2 pain).** Neither can enumerate the keys the app
  accepts. config-rs has no introspection at all; figment's `Metadata` is value
  *provenance* (which source a value came from, for error messages), not a
  schema — so neither generates the `--config` listing without a hand-maintained
  second list, the exact drift we are removing. confique's `Config::META` +
  `template()` *is* that listing, from the same declaration.
- **Co-located declaration (decision 1's "distributed structs").** Both split a
  knob's definition out of the field into a central builder — env keys, defaults
  via `set_default` / `#[serde(default)]` / a defaults-provider. confique keeps
  env-key + default + doc + validate *on the field*, in the owning crate.

Their env model is also a global prefix + separator (`AETHER_DB__URL` →
`db.url`) that fights our flat, inconsistent, partly un-prefixed names
(`AETHER_PEER_STEAL`, bare `PERF_K`, `GEMINI_API_KEY`); confique's per-field key
maps them 1:1, which is what makes the behaviour-identical step-1 env map a pure
annotation.

Between the two, **`figment` is the better member** — provenance-grade error
messages (a partial answer to the validate pain), first-class profiles, and
Rocket-grade maturity — so it, not config-rs, is the runner-up to swap to if
confique's youth (v0.4, ~half-documented) is a concern. figment's profiles do
not map to an aether need: per-application config is served by per-spawn `argv`
(decision 3), not named profiles. Net ranking for these requirements:
**confique > figment > config-rs** — confique wins on discovery + co-located
declaration, the two requirements that motivated the ADR. The cost it carries is
the maturity gap (younger, smaller surface) and errors less source-rich than
figment's.

### 2. Where the layer lives — derive in place, no new `aether-config` crate (yet)

Because the structs stay in their owning crates and derive the chosen macro
directly, **there is no central crate for the config structs**. The shared
surface is just the external crate as a workspace dependency. The aether-specific
glue — the unknown-`AETHER_*` warning sweep, the `--config` discovery command,
and the component-config delivery of decision 5 — is small; it lands as a thin
module (initially in `aether-substrate`, promoted to a leaf `aether-config`
crate only if the out-of-process MCP/tunnel/perf bins need it without a substrate
dep, the original fork-2 concern). This reverses iamacoffeepot/aether#1055's
assumption that config folds into `aether-substrate` as bespoke code: the
external crate *is* the shared layer.

### 3. Sources layer; migration is staged (structs map env now, argv-over-env later)

The end-state source-stack, lowest to highest precedence:

```
typed defaults  <  config file  <  env  <  argv
```

`argv` overrides `env` (the explicit flip), `env` overrides a file, a file
overrides the typed default. The rollout is staged so no step is a flag day:

- **Step 1 (this effort): structs map env.** Every knob becomes a derived field
  with its `#[config(env = ...)]` and typed default. Resolution is `defaults <
  env` — **identical observable behavior to today** — but now typed, validated,
  discoverable, and free of the duplicated parsers. Naming normalization is
  resolved here via *back-compat aliases*: a field accepts both the new name and
  the legacy one (`AETHER_PERF_WORKERS`, bare `PERF_K`), warning on the legacy
  name. (confique maps one env key per field; multi-name acceptance is a small
  custom layer — flagged as the one place the macro needs help.)
- **Step 2: argv above env.** The chassis mains parse `argv` (via `clap`) into
  the highest-priority partial layer, and `spawn_substrate`'s already-plumbed
  `args` finally land. This is what makes per-spawn / per-application
  configuration real: two substrates spawned with different `args` get different
  config from one shared environment.
- **Step 3 (optional, later): config file + deprecate env.** A `--config
  file.toml` source slots between defaults and env; the env-var surface enters a
  deprecation window and is eventually removed. The layered model means this is
  additive, not a rewrite.

This honors "replace the env vars" as the *destination* while keeping
env-driven CI workflows, `.mcp.json`, and `ensure-tunnel.sh` working through the
transition.

### 4. Validation: warn on unknown, hard-error on known-but-unparseable

At boot the chassis logs a warning for any `AETHER_*` env var not claimed by a
registered field (catches typos without breaking on a stray CI variable —
strict-reject-unknown is too brittle). A *known* key that is set but fails to
parse is a **hard boot error**, not a silent fall-through to default. Discovery
is a `--config` flag that walks the derive metadata and prints every knob, its
source-resolved value, default, and doc.

### 5. Component configuration is delivered at init — symmetric with native

A component declares a **config kind** and receives it **at `init`**, exactly as
a native actor receives `NativeActor::Config`. Concretely: give the guest
`FfiActor` (currently `fn init<C: Resolver>(ctx: &mut C)`, no config) a
`type Config` with `init(config: Self::Config, ctx: …)`, mirroring
`NativeActor { type Config; fn init(config, ctx) }`. The default is
`type Config = ()` for components that need none.

Because a wasm guest's config must cross the FFI as bytes, `type Config` for a
guest is a **`Kind`** (schema-bearing). That is what "expose a configuration kind"
means: the config *type* is a kind whose schema rides the `aether.kinds` custom
section, so `describe_component` surfaces the config shape just like a handler
kind. The config bytes ride the load/spawn call (`load_component` /
`SpawnEngine`) — passed by the hub **upward** through the substrate — and the
chassis decodes them into `Self::Config` and hands them to the guest's `init`.
This unifies the model:

| | declares | delivered |
|---|---|---|
| native actor | `type Config = HttpConfig` (plain struct) | `init(config, ctx)` |
| wasm component | `type Config = MyConfig` (a `Kind`) | `init(config, ctx)`, bytes decoded by chassis |

Init-time delivery (rather than a post-init mail) is the right call because
`init` is where load-bearing state is built — config must be present there, and
native already does exactly this. This does **not** contradict the established
"chassis-pushed state rides mail" pattern (ADR-0060's log-drain delivery): that
is for *runtime* state pushed *after* construction. The two are complementary —
**boot config at `init`, runtime *reconfiguration* as mail** (a component that
wants live reconfig adds a `#[handler]` for its config kind; the same kind serves
both paths). Init-time config is a chassis *push* into `init`, not a guest
host-fn *pull*, so it stays consistent with ADR-0030's "guests don't host-fn-pull"
stance.

#### Config delivery routes by size, reusing the shared guest-heap reserve (#1390)

The wire-encoded config bytes are written into the guest's linear memory before
`init_with_config_p32(mailbox_id, ptr, len)`. The substrate routes that write by
size, mirroring the mail path (#1337): a config at or below the fixed
`CONFIG_OFFSET` window's `MAX_CONFIG_PAYLOAD_BYTES` cap lands inline at
`CONFIG_OFFSET`; a larger config rides the same reusable guest-heap reserve buffer
the large-mail path uses (`mail_scratch::reserve`), so a config that would overrun
the low shadow-stack window is delivered heap-backed instead of clobbering the
stack and trapping init. The reserve is a module-level guest allocator, ready
right after instantiation and independent of the actor's `init`, and config use is
temporally disjoint from mail use, so sharing the one buffer across the two paths
is safe. A config past the absolute deliverable ceiling, or destined for a raw-FFI
guest with no reserve export, is a **clean boot error** (`LoadResult::Err`) with a
structured log — never a write or a trap.

## Consequences

**Positive**

- One declaration per knob drives parse, validate, and discover; the duplicated
  `parse_env_*` helpers are deleted.
- Typos and bad values surface at boot (warn / hard-error) instead of silently
  defaulting.
- `--config` gives the listing that does not exist today.
- Per-spawn `argv` makes per-application configuration real: differently-configured
  substrates from one environment, the "general application host" axis. The
  `spawn_substrate` args path stops being dead plumbing.
- Native and wasm actors get config the same way (`type Config` at `init`); the
  config shape is introspectable via `describe_component`.
- Migration is incremental and reversible per-subsystem — step 1 changes no
  observable behavior.

**Negative / costs**

- A new third-party dependency (`confique`, and `clap` when step 2 lands).
  Weigh against aether's lean-deps habit — justified here because config parsing,
  layering, and template generation are exactly the battle-tested-crate case, and
  the issue explicitly asked to evaluate external solutions.
- `FfiActor` gains a `type Config` — a trait change touching the `#[actor]` /
  `export!` macros and every guest's `init` signature (default `= ()` keeps the
  blast radius to the trait + macro, not every component).
- Back-compat aliases mean a window where both old and new env names work — extra
  surface until the deprecation completes.
- confique's one-env-per-field needs a small custom layer for multi-name aliases.

**Neutral / follow-on**

- The bespoke parsers stay bespoke: `AETHER_WINDOW_MODE`'s `windowed:WxH`
  grammar, the allowlist splitters — these are field *parsers*, fed by the
  source-stack, not replaced by it.
- Naming normalization (`AETHER_WORKERS` vs `AETHER_PERF_WORKERS`, `PERF_K`
  under-prefix) is resolved as fields are migrated, behind aliases.
- Implementation scopes as incremental PRs off this ADR: (a) adopt the crate +
  migrate one subsystem as the pattern, (b) sweep remaining subsystems, (c) the
  `FfiActor::Config` + delivery glue, (d) `argv` layer + `spawn_substrate`
  wiring, (e) `--config` discovery, (f optional) config file + env deprecation.

## Alternatives considered

- **Central registry table** (the issue's first framing) — one declarative list
  of every knob as the single source of truth, structs generated from it.
  Rejected: rewrites every existing `*Config` struct, centralizes ownership away
  from the subsystem, and a derive macro already yields the discovery/validation
  metadata a table would have provided.
- **Hand-rolled macro / keep `from_env`** — dedup the parsers into one helper and
  stop. Rejected: solves the parser dup but not validation, discovery, or the
  argv/per-spawn axis; re-implements what `confique`/`figment` already ship.
- **`twelf`** instead of `confique` — native `Layer::Clap` makes argv a
  first-class layer *now*, not a `clap`-glued partial. Weaker on the things step 1
  needs most: defaults via `#[serde(default)]` (no per-field doc/default surface),
  thinner discovery/template generation. Strong fallback if argv-now is
  prioritized over discovery-now; reconsider at step 2.
- **`conf`** — single derive over CLI + env + file. Viable; less mature
  discovery/template story than confique and a smaller ecosystem footprint.
- **`config` (config-rs) / `figment`** — the merge-into-serde family (a runtime
  provider/builder chain deserializing into a plain struct). Rejected as the lead
  for the reasons spelled out in Decision §1: no schema introspection (so no
  native `--config` discovery) and the knob definition splits out of the field.
  `figment` is the stronger member — provenance errors, profiles, maturity — and
  the runner-up to swap in if confique's youth is a concern; `config-rs` trails it
  (no profiles, weaker errors).
- **New `aether-config` crate up front** (fork 2) — rejected for now: with
  derive-in-place there is nothing central to house; promote a leaf crate only if
  the out-of-process bins need the glue without a substrate dep.
- **Hard env→argv replacement now** — rejected: breaks every env-driven CI
  workflow, `.mcp.json`, `ensure-tunnel.sh`, and the `AETHER_RPC_PORT` injection
  in one cut. Staged migration reaches the same destination without a flag day.
- **Component config as post-init mail only** (the first framing in chat) —
  rejected as the *primary* path: config is needed at `init` where load-bearing
  state is built, and native already delivers at `init`. Mail is retained for
  *runtime reconfiguration*, not initial config.
