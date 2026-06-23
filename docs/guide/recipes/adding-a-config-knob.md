# Adding a config knob

> **Prereq (recompile class):** you're editing aether's Rust and rebuilding, so
> you need `cargo` and the pre-flight loop (`scripts/preflight.sh`). The
> [Configuration](../systems/configuration.md) explainer states the model this
> recipe walks; [ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md)
> holds the design. Read the explainer first if "layered source-stack" and
> "discovery dump" aren't already familiar.

A knob is a field on a subsystem's resolved-config struct, declared once with a
`#[config(...)]` hint that supplies its default and its env/CLI names. That
single declaration generates the env layer, the `clap` argument
overlay, the layered resolver, and the `--config` discovery entry — so you never
write an `env::var(...).parse()` read. This recipe adds a knob end to end, with
the two gotchas (the `native` feature gate, the `*_defaults_match` test) inline at
the step where each bites.

## The exemplar to copy

Follow [`HttpConfig`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-capabilities/src/http/client.rs)
in `crates/aether-capabilities/src/http/client.rs`. It's the same struct the
[Configuration](../systems/configuration.md) explainer excerpts, it carries most
of the hints you'll reach for (`default`, `env`, `cli_long`, `csv_set`,
`ms_duration`, `layer_field`), and it's wired into both full-stack chassis. Open
it alongside this recipe and mirror the field you're closest to.

The steps below add a field to an **existing** config struct (`HttpConfig`),
which is the common case — the struct's layer is already registered for
discovery, so a new field joins the `--config` dump for free. Adding a
**brand-new** config struct takes two extra steps, called out at the end.

## Enable / disable flags

A capability that ships off (or on) by default exposes that switch as one
config-API `bool`, resolved through the same derive as every other knob —
not inferred from another field (a bound address, a configured path) and
not read out of `env::var` directly. Declare it with a `false` literal
default; a `bool` needs no parser:

```rust
#[cfg_attr(feature = "native", config(default = false))]
pub enabled: bool,
```

Name it for the intent: an opt-in cap that stays off until asked for calls
the field `enabled`, while an opt-out cap that runs until suppressed calls
it `disabled`. Both default to `false`, so the literal default reads as the
unsurprising state, and a chassis turns the behaviour on from one
documented `AETHER_…` key (or its CLI flag). At the composition site the
chassis maps the resolved flag to its structural choice —
`cfg.enabled.then_some(cfg)` for an opt-in cap — keeping the flag the
single source of the on/off decision. confique's native bool parsing
accepts `1` / `true` / `yes` / `0` / `false` / `no`, case-insensitive and
trimmed.

## Steps

### 1. Declare the field with a `#[config(...)]` hint

Add the field to the struct in its cap crate and annotate it. The derive reads
the hint to generate everything downstream:

```rust
#[cfg_attr(feature = "native", config(default = false))]
pub require_https: bool,
```

Most fields need no parser. A numeric, `Duration`, or `bool` field rides
confique's native env parsing: it trims the value, treats an empty one as unset
(falling back to the default), and hard-errors on a non-empty value that doesn't
parse — so a typo'd `AETHER_…` number stops the boot with the key named instead
of silently defaulting.

The hints you have:

- `default = <lit>` — the literal default the layer resolves to when no env or
  argv value is set.
- `env = "..."` / `cli_long = "..."` — pin the env key and `--flag` to an exact
  name when the field name doesn't match the historical wire shape. Absent these,
  the names come from the container's `env_prefix` / `cli_prefix` joined to the
  field name.
- `csv_set` — for a `HashSet<String>` field: the overlay accepts one
  `Option<String>`, and the env side auto-wires `parse_csv_set` (trim, split on
  commas, drop empties).
- `nonzero` — a resolved `0` coerces to the field default, for a knob where `0`
  is degenerate (a concurrency bound that would deadlock at zero). Requires a
  `default`.
- `ms_duration` + `layer_field = "..."` — the domain field is a `Duration` while
  the layer carries `<field>_ms: u32`; the derive bridges via
  `Duration::from_millis`.
- `parse = <fn_path>` — the escape hatch for a genuinely custom mapping, a
  `fn(&str) -> Result<T, impl Error>`. `fs`'s `parse_dir` (an empty override is
  unset; the default is computed at runtime from `dirs::data_dir()`) is the
  worked example. A plain numeric / `bool` / `Duration` / `String` field never
  needs it.

The container attribute on the struct sets the prefixes both names derive from:

```rust
#[cfg_attr(
    feature = "native",
    config(env_prefix = "AETHER_HTTP", cli_prefix = "http")
)]
```

### 2. Keep `Default` in sync — and let the test enforce it

`HttpConfig` declares `impl Default` separately from the derive's `default = ...`
literals (the derive feeds the layer; `Default` feeds direct construction in
tests and call sites). Add your field's default to **both**. The
`http_from_env_defaults_match` test in the `http/client.rs` test module is what
keeps them honest:

```rust
#[test]
fn http_from_env_defaults_match() {
    use super::super::HttpConfigLayer;
    use confique::Config as _;
    let layer = HttpConfigLayer::builder().load().expect("defaults load");
    let default = HttpConfig::default();
    // assert each layer default equals the matching HttpConfig::default() field
}
```

`HttpConfigLayer` is the derive-emitted layer type — you don't write it, but you
do reference it from the test. Add an assertion for your new field. It loads with
no `.env()` source, so it's env-free and CI-safe (issue 464).

> **Gotcha — the `native` feature gate.** Every `#[derive(...)]` and `#[config]`
> attribute is wrapped in `#[cfg_attr(feature = "native", ...)]`, including the
> struct-level derive. The capabilities crate also cross-compiles to wasm, where
> the config machinery isn't present, so the wasm build must carry only the plain
> struct. Clippy runs host-native and won't catch a missing gate — the wasm32
> cross-build step in `scripts/preflight.sh` (step 6) is what fails on it. Any
> `parse` helper you add is `#[cfg(feature = "native")]` too.

### 3. Wire the argv overlay into each chassis CLI

The derive emits `<Name>Overlay` (here `HttpOverlay`) with an `into_layer()`
method. For a field on an existing struct whose overlay is already flattened into
a chassis CLI, the new field rides the existing overlay automatically — confirm
your struct's overlay is reached. `HttpOverlay` is re-exported from
`crates/aether-substrate-bundle/src/cli.rs` and flattened into `CommonOverlay`:

```rust
#[command(flatten)]
pub http: HttpOverlay,
```

`CommonOverlay` is in turn flattened into `DesktopCli` and `HeadlessCli`, so both
full-stack chassis expose the flag. Each chassis resolves it in
`from_env_with_argv` (`crates/aether-substrate-bundle/src/{desktop,headless}/chassis.rs`):

```rust
let http = HttpConf::try_from_argv_then_env(http.into_layer())?;
```

`try_from_argv_then_env` is the fallible resolver — argv wins over env, env over
the literal default, and an unparseable *known* value `?`-propagates as a
`ConfigError` rather than falling through. (`from_argv_then_env` is the panicking
sibling; caps with total parsers like `NamespaceRoots` use it.) Absent flags
resolve `None` and fall through to env, so an empty argv boots byte-identically.

The flag name is mechanical: take the env key, drop the `AETHER_` prefix,
lowercase, hyphenate — `AETHER_HTTP_REQUIRE_HTTPS` becomes `--http-require-https`.
A bool flag accepts zero or one value (`--http-disable` ⇒ `true`,
`--http-disable=false` ⇒ `false`, absent ⇒ `None`).

### 4. Confirm the knob in the `--config` dump

Build and run any full-stack chassis with `--config` — it walks the same
declarations and prints every knob's env key, resolved value, source, default,
and doc, then exits before boot:

```sh
cargo run -p aether-substrate-bundle --bin aether-substrate-headless -- --config
```

Your new field appears with its default. The dump is rendered by
`chassis_config_dump()` in `crates/aether-substrate-bundle/src/chassis_common.rs`,
which walks `chassis_registry()`. That registry lists `&HttpConfigLayer::META`, so
a field on an existing struct shows up with no extra wiring — the META walk is the
discovery source of truth. If your knob is missing from the dump, the field isn't
reaching the layer (re-check the `#[config]` hint and the `native` gate).

### 5. Run the pre-flight

```sh
scripts/preflight.sh
```

This is the CI-equivalent loop: fmt, clippy, doc, nextest (which runs
`http_from_env_defaults_match`), and the wasm32 component cross-build that catches
a missing `native` gate. Fix anything it flags before you push.

## Adding a brand-new config struct

If the knob doesn't belong on any existing struct, you're declaring a new
`#[derive(aether_substrate::Config)]` struct. Two steps beyond the above:

- **Register its layer META for discovery.** Add `&YourConfigLayer::META` to the
  `METAS` slice in `chassis_registry()`
  (`crates/aether-substrate-bundle/src/chassis_common.rs`) so the `--config` dump
  and the unknown-key sweep (`chassis_known_keys`) both see its knobs.
- **Flatten its overlay into a chassis CLI.** Re-export `YourOverlay` in
  `crates/aether-substrate-bundle/src/cli.rs` and `#[command(flatten)]` it into
  `CommonOverlay` (or a per-chassis root), then resolve it in each chassis's
  `from_env_with_argv` the way `HttpConf` is resolved.

## Verify against current code

This recipe names files, symbols, and methods that move. Before following it,
confirm `HttpConfig`, `HttpConfigLayer`, `HttpOverlay`,
`try_from_argv_then_env`, `into_layer`, `chassis_registry`, and
`chassis_config_dump` still exist where named — grep the crates, and if a name has
drifted, fix the recipe as part of your work.
