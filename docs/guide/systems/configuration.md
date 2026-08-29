# Configuration

> **Governing ADR:** [ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md)
> (application configuration). The **model** — the layered source-stack, one
> typed struct per subsystem, validation, and discovery — is **stable** and
> mostly shipped; the rollout is still **settling** at the edges (a handful of
> chassis-wide knobs are still read inline). This page documents the contract
> and defers the rollout's internals to the ADR.

Configuration is how a knob's value gets decided before the engine runs — the
worker-pool size, an HTTP allowlist, a provider's API key, the tick rate. Values
come from a **layered stack** of sources (defaults, environment, command-line
arguments) with a defined precedence, declared once per knob and resolved the
same way everywhere. And it's **per-spawn**: two substrates launched from one
shell can be told apart — one with a capability enabled under key A, another with
it off — the axis the "substrate as a general application host" direction needs.

When you drive the engine over MCP, configuration is per engine: you hand
`spawn_substrate` the arguments for one substrate and give a loaded component its
own typed config, independent of any other engine. When you author a capability or
component, you declare each knob once on its config struct — that single
declaration parses the value, supplies its default, validates it, and lists it
for discovery.

## Why it exists

Aether is assembled from many subsystems — capabilities, the runtime, the chassis
— each with its own knobs, and all of them are configured together at startup. A
single configuration standard gives them one way to declare a knob and one way to
resolve it, and three properties follow from that: **validation** (a typo'd key,
or a known key set to garbage, is caught at boot rather than swallowed by a
default), **discovery** (one listing of every knob, its default, and what it
does), and no **duplicated** parsing across subsystems.

The second reason is per-spawn configuration. Configuration enters a process
once, at startup, and each channel addresses one audience (ADR-0162).
Environment variables configure the process a person launches directly — the
hub at a shell — and stop there: the hub and tunnel construct a forked child's
environment rather than handing down their own, clearing `AETHER_*` entirely
and copying only a shared allowlist of platform keys (locale, proxy, GPU and
audio driver families, `PATH` / `HOME`, and the like). Exporting an aether knob
on the hub therefore does not reach the substrates it forks. Per-spawn arguments
are what configure one engine differently from the rest — they ride the spawn
call to a single substrate as argv, the addressed machine channel — which is
what the "substrate as a general application host" direction needs.

## The model: layered sources, one struct per subsystem

A resolved value comes from a stack of sources, lowest precedence to highest:

```text
typed defaults   <   config file   <   environment   <   argv
```

Argv overrides the environment, the environment overrides a file, a file
overrides the declared default. The file is a sectioned TOML source supplied with
`--config <PATH>` or the `AETHER_CONFIG_FILE` fallback; if neither is set, an
engine boots exactly as env-then-argv configuration dictates.

Each subsystem owns its own config struct, in its own crate, declaring its own
knobs — there is no central registry that every subsystem has to register into.
A `#[derive(aether_substrate::Config)]` on that struct is what unifies them: from
the field annotations it generates the environment parsing, the argument
(`clap`) layer, and the layered resolution, *and* a machine-readable description
of every knob that the discovery dump walks. So the declaration stays next to the
field that owns it, and parse, validate, and discover all come from that one
declaration.

## Resolution, validation, and discovery

Resolution is strict. At boot the chassis **warns** on any `AETHER_*` variable
that no registered knob claims — catching a typo
without breaking on a stray CI variable — and **hard-errors** on a *known* key
that's set but fails to parse, rather than falling through to the default. A bad
value stops the boot with the key named, instead of a subsystem quietly running
on a default you didn't ask for.

Parsing failure is distinct from **post-parse semantic validation**. A known
field with malformed syntax hard-errors. A successfully parsed value can still
be rejected or normalized by a subsystem's explicit policy—for example a window
mode may warn and fall back when the requested monitor mode is unavailable.
Document that behavior with the owning subsystem rather than hiding it as a
parse default.

Discovery is the `--print-config` flag on any chassis binary: it walks the same
declarations and prints every knob — its environment key, the value it resolves
to and which source that value came from, its default, and its doc — then exits
without booting. That listing is generated from the field annotations, so it
can't drift from what the engine actually reads. It's the first place to look
when you're unsure what a build will do with a given variable.

## Configuring a running engine

Over MCP there are three ways to set configuration, from coarsest to finest:

- **The environment** configures the process you launch directly — the hub
  itself, or a chassis you run at a shell. Use `--print-config`, the owning
  config struct, and the active surface contract for exact knobs
  (`AETHER_TICK_HZ`, `AETHER_SAVE_DIR`, `AETHER_AUDIO_DISABLE`,
  `AETHER_ACTOR_TRACE_RING_SIZE`, and the rest). It's fixed at launch and
  addressed to that one process: `AETHER_*` is scrubbed from a spawned engine's
  environment at fork (ADR-0162), so a knob exported on the hub does not reach
  the substrates it forks — configure those through per-spawn arguments below.
- **A chassis config file** is the persistent per-deployment layer. Pass
  `--config path/to/chassis.toml` on the chassis command line, or set
  `AETHER_CONFIG_FILE` as a fallback for the file path. The file is sectioned by
  subsystem, for example `[http]`, `[http-server]`, `[fs]`, `[anthropic]`,
  `[actor]`, `[scheduler]`, `[settlement]`,
  `[chassis]`, plus chassis-specific sections such as `[window]`, `[tick]`, and
  hub `[engine]`. Environment variables still override file values.
- **Per-spawn arguments** are how a spawned engine is configured. `spawn_substrate`
  forwards its `args` to the substrate as command-line arguments — the addressed
  machine channel (ADR-0162) — so you can spawn one engine with `--tick-hz …`
  or `--http-disable` and leave the next one alone. Argv is where each engine's
  knobs live, since the hub's environment does not cross into its children. Flag
  names are mechanical: take the environment key, drop the `AETHER_` prefix,
  lowercase, and hyphenate (`AETHER_HTTP_TIMEOUT_MS` → `--http-timeout-ms`).
- **Component config** is finer still: a component declares a typed `Config` and
  receives it at `init`. Because a guest's config crosses the wasm boundary as
  bytes, that type is a **kind** (schema-bearing): `describe_component`
  identifies the config kind and `describe_kinds` surfaces its shape. Over MCP,
  pass either `load_component.config` as inline structured JSON or `config_path`
  as a path to a JSON file; the harness schema-encodes that JSON to wire bytes
  before the byte-transparent chassis hands the decoded value to the guest's
  `init`. The two inputs are mutually exclusive, and `config_path` is JSON
  rather than pre-encoded wire bytes. This mirrors a native actor exactly: both
  declare `type Config` and receive it at construction
  ([ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md)
  §5). Boot config arrives at `init`; *runtime* reconfiguration, if a component
  wants it, is ordinary mail — the same kind can serve both.

## Adding a knob

Author-side, a new knob is a field on the subsystem's resolved-config struct, not
a fresh `env::var` read. Derive `Config` on the struct and annotate the field:

```rust
#[derive(Clone, Debug)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(feature = "runtime", config(env_prefix = "AETHER_HTTP", cli_prefix = "http"))]
pub struct HttpConfig {
    #[cfg_attr(feature = "runtime", config(default = false))]
    pub disabled: bool,
    #[cfg_attr(feature = "runtime", config(default = [], csv_set))]
    pub allowlist: HashSet<String>,
}
```

The derive emits the environment-shaped layer, the `clap` argument overlay, and
the `from_env` / `from_argv_then_env` resolvers. A numeric, `Duration`, or `bool`
field carries only its `default` — confique's native parsing trims the value,
treats an empty one as unset (falling back to the default), accepts the usual
bool spellings (`1` / `true` / `yes` / `0` / `false` / `no`), and hard-errors on
a non-empty garbage value. The remaining field hints (`env`, `cli_long`,
`ms_duration`, `csv_set`, `nonzero`) carry the rest of the per-knob shape;
`parse` names a custom parser for the rare field that needs one. Two things to
know going in:

- **Gate it on the `runtime` feature**, as above. A capability crate also
  cross-compiles to wasm, where the config machinery isn't available; the
  `#[cfg_attr(feature = "runtime", …)]` keeps the wasm build carrying only the
  plain struct. Clippy runs host-native and won't catch a missing gate — the
  wasm32 cross-build in CI will.
- **Wire the argument overlay into the chassis CLI** so the per-spawn layer
  reaches your knob, and add a `*_defaults_match` test (the derive's literal
  default and your struct's `Default` are declared separately and a test keeps
  them honest).

The full walkthrough is the [*Adding a config knob*](../recipes/adding-a-config-knob.md)
recipe; the rule to carry is
that a knob is declared once and resolved by the layer, never read ad-hoc.

A `clippy.toml` `disallowed-methods` entry bans `std::env::var` / `std::env::var_os`
workspace-wide to keep that rule mechanical: a capability that reads the
environment directly fails `cargo clippy -- -D warnings` (the CI gate). A
legitimately external read — the config machinery
itself, a process-level tuning knob, a standard `HOME` / `XDG` lookup, a build
script, or test code — carries an `#[allow(clippy::disallowed_methods)]` with a
one-line reason stating why it is not cap config.

`HttpConfig` above is the live example: each field is a knob the deployer sets at
boot, and what those knobs gate — the deny-by-default egress allowlist, the body
cap, the per-request timeout — is the subject of [HTTP egress](http.md).

## Where to read more

- The rollout's design, the source-stack rationale, and the crate choice —
  [ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md).
- The `spawn_substrate` arguments and `load_component` config inputs in their tool
  context — [The MCP harness](../mcp-harness.md).
- How a component declares and receives `type Config` —
  [Components & lifecycle](components.md).
- The exact resolved knob inventory — the target chassis's `--print-config`
  output and its current config structs.
