# Configuration

> **Governing ADR:** [ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md)
> (application configuration). The **model** — the layered source-stack, one
> typed struct per subsystem, validation, and discovery — is **stable** and
> mostly shipped; the rollout is still **settling** at the edges (a config-*file*
> layer is planned but not yet in, and a handful of chassis-wide knobs are still
> read inline). This page documents the contract and defers the rollout's
> internals to the ADR.

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
once, at startup, and a substrate inherits its environment from the hub that
forked it — the hub from the tunnel, the tunnel from the launching shell.
Environment values are therefore **fleet-global**: every engine the hub forks
sees the same ones, with no way to enable a capability on one substrate and
disable it on another. Per-spawn arguments are what let one engine differ from
the rest — they ride the spawn call to a single substrate and layer above the
shared environment — which is what the "substrate as a general application host"
direction needs.

## The model: layered sources, one struct per subsystem

A resolved value comes from a stack of sources, lowest precedence to highest:

```text
typed defaults   <   config file   <   environment   <   argv
```

Argv overrides the environment, the environment overrides a file, a file
overrides the declared default. (The file layer is the one piece still to land —
today the live stack is `defaults < environment < argv`, and a missing argument
simply falls through to env-then-default, so an engine launched with no arguments
boots exactly as the environment alone dictates.)

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

Discovery is the `--config` flag on any chassis binary: it walks the same
declarations and prints every knob — its environment key, the value it resolves
to and which source that value came from, its default, and its doc — then exits
without booting. That listing is generated from the field annotations, so it
can't drift from what the engine actually reads. It's the first place to look
when you're unsure what a build will do with a given variable.

## Configuring a running engine

Over MCP there are three ways to set configuration, from coarsest to finest:

- **The environment** is the workhorse, and it's what `CLAUDE.md` documents
  knob by knob (`AETHER_TICK_HZ`, `AETHER_SAVE_DIR`, `AETHER_AUDIO_DISABLE`, and
  the rest). It's fleet-wide and fixed at launch: set it before bringing the
  tunnel up, and every engine the hub forks inherits it.
- **Per-spawn arguments** are the per-engine override. `spawn_substrate` forwards
  its `args` to the substrate as command-line arguments, layered *above* the
  inherited environment — so you can spawn one engine with `--gemini-api-key …`
  or `--http-disable` and leave the next one alone. This is what makes two
  differently-configured substrates from one environment possible. Flag names are
  mechanical: take the environment key, drop the `AETHER_` prefix, lowercase, and
  hyphenate (`AETHER_HTTP_TIMEOUT_MS` → `--http-timeout-ms`).
- **Component config** is finer still: a component declares a typed `Config` and
  receives it at `init`. Because a guest's config crosses the wasm boundary as
  bytes, that type is a **kind** (schema-bearing), so `describe_component`
  surfaces the config shape the way it surfaces a handler kind. You deliver the
  bytes through `load_component`'s `config_path` — the chassis decodes them and
  hands the value to the guest's `init`. This mirrors a native actor exactly:
  both declare `type Config` and receive it at construction
  ([ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md)
  §5). Boot config arrives at `init`; *runtime* reconfiguration, if a component
  wants it, is ordinary mail — the same kind can serve both.

## Adding a knob

Author-side, a new knob is a field on the subsystem's resolved-config struct, not
a fresh `env::var` read. Derive `Config` on the struct and annotate the field:

```rust
#[derive(Clone, Debug)]
#[cfg_attr(feature = "native", derive(aether_substrate::Config))]
#[cfg_attr(feature = "native", config(env_prefix = "AETHER_HTTP", cli_prefix = "http"))]
pub struct HttpConfig {
    #[cfg_attr(feature = "native", config(default = false, parse = parse_flag))]
    pub disabled: bool,
    #[cfg_attr(feature = "native", config(default = [], parse = parse_allowlist, csv_set))]
    pub allowlist: HashSet<String>,
}
```

The derive emits the environment-shaped layer, the `clap` argument overlay, and
the `from_env` / `from_argv_then_env` resolvers; the field hints (`default`,
`parse`, `env`, `cli_long`, `ms_duration`, `csv_set`) carry the per-knob shape.
Two things to know going in:

- **Gate it on the `native` feature**, as above. The capabilities crate also
  cross-compiles to wasm, where the config machinery isn't available; the
  `#[cfg_attr(feature = "native", …)]` keeps the wasm build carrying only the
  plain struct. Clippy runs host-native and won't catch a missing gate — the
  wasm32 step in `scripts/preflight.sh` will.
- **Wire the argument overlay into the chassis CLI** so the per-spawn layer
  reaches your knob, and add a `*_defaults_match` test (the derive's literal
  default and your struct's `Default` are declared separately and a test keeps
  them honest).

The full walkthrough is the [*Adding a config knob*](../recipes/adding-a-config-knob.md)
recipe; the rule to carry is
that a knob is declared once and resolved by the layer, never read ad-hoc.

`HttpConfig` above is the live example: each field is a knob the deployer sets at
boot, and what those knobs gate — the deny-by-default egress allowlist, the body
cap, the per-request timeout — is the subject of [HTTP egress](http.md).

## Where to read more

- The rollout's design, the source-stack rationale, and the crate choice —
  [ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md).
- The `spawn_substrate` arguments and `load_component` config path in their tool
  context — [The MCP harness](../mcp-harness.md).
- How a component declares and receives `type Config` —
  [Components & lifecycle](components.md).
- The operational knob-by-knob reference for the `AETHER_*` environment surface —
  `CLAUDE.md`.
