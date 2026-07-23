# Adding a config knob

> **Prereq (recompile class):** you're editing aether's Rust and rebuilding, so
> you need `cargo`; CI runs the full check set on every push. The
> [Configuration](../systems/configuration.md) explainer states the model this
> recipe walks; [ADR-0090](https://github.com/iamacoffeepot/aether/blob/main/docs/adr/0090-application-configuration.md)
> holds the design. Read the explainer first if "layered source-stack" and
> "discovery dump" aren't already familiar.

A knob is a field on a subsystem's resolved-config struct, declared once with a
`#[config(...)]` hint that supplies its default and its env/CLI names. That
single declaration generates the partial layer used by the config file and env,
the `clap` argument overlay, the layered resolver, and the `--print-config`
discovery entry — so you never write an `env::var(...).parse()` read. This recipe
adds a knob end to end, with
the two gotchas (the `runtime` feature gate, the `*_defaults_match` test) inline at
the step where each bites.

## The exemplar to copy

Follow [`HttpConfig`](https://github.com/iamacoffeepot/aether/blob/main/crates/aether-http/src/client/config.rs)
in `crates/aether-http/src/client/config.rs`. It's the same struct the
[Configuration](../systems/configuration.md) explainer excerpts, it carries most
of the hints you'll reach for (`default`, `env`, `cli_long`, `csv_set`,
`ms_duration`, `layer_field`), and it's wired into both full-stack chassis. Open
it alongside this recipe and mirror the field you're closest to.

The steps below add a field to an **existing** config struct (`HttpConfig`),
which is the common case — the struct's layer is already registered for
discovery, so a new field joins the `--print-config` dump for free. Adding a
**brand-new** config struct takes three extra steps, called out at the end.

## Enable / disable flags

A capability that ships off (or on) by default exposes that switch as one
config-API `bool`, resolved through the same derive as every other knob —
not inferred from another field (a bound address, a configured path) and
not read out of `env::var` directly. Declare it with a `false` literal
default; a `bool` needs no parser:

```rust
#[cfg_attr(feature = "runtime", config(default = false))]
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
#[cfg_attr(feature = "runtime", config(default = false))]
pub require_https: bool,
```

Most fields need no parser. A numeric, `Duration`, or `bool` field rides
confique's native env parsing: it trims the value, treats an empty one as unset
(falling back to the default), and hard-errors on a non-empty value that doesn't
parse — so a typo'd `AETHER_…` number stops the boot with the key named instead
of silently defaulting.

The hints you have:

- `default = <lit>` — the literal default the layer resolves to when no config-file,
  env, or argv value is set.
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
    feature = "runtime",
    config(env_prefix = "AETHER_HTTP", cli_prefix = "http")
)]
```

#### Write the field's first sentence as its `--help` summary

The derive lifts the first sentence of the field's rustdoc into that flag's
`--help` description and appends the env key and resolved default. So the
opening sentence is the copy an operator reads: a plain-language summary of
what the knob does, with units spelled out, no internal type names, no issue
or ADR references, and no leading `AETHER_…` env incantation. Everything a
maintainer wants — the adapter the flag swaps, the error it returns, the
wire-shape pins — goes below a blank doc line, where it stays in the source
and out of `--help`.

```rust
/// Reject plaintext HTTP URLs and allow only HTTPS.
///
/// An `http://` URL is rejected with `HttpError::InvalidUrl`.
#[cfg_attr(feature = "runtime", config(default = false))]
pub require_https: bool,
```

The first sentence there is the entire flag description a `--help` reader
gets; the detail paragraph names `HttpError::InvalidUrl` for whoever opens the
source, and never surfaces in the dump. A summary that opens with a type name
or an `AETHER_…` key renders that text verbatim, so the flag reads as
maintainer prose to the operator who runs `--help`.

### 2. Keep `Default` in sync — and let the test enforce it

`HttpConfig` declares `impl Default` separately from the derive's `default = ...`
literals (the derive feeds the layer; `Default` feeds direct construction in
tests and call sites). Add your field's default to **both**. The
`http_from_env_defaults_match` test in the `http/client/mod.rs` test module is what
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

> **Gotcha — the `runtime` feature gate.** Every `#[derive(...)]` and `#[config]`
> attribute is wrapped in `#[cfg_attr(feature = "runtime", ...)]`, including the
> struct-level derive. The capabilities crate also cross-compiles to wasm, where
> the config machinery isn't present, so the wasm build must carry only the plain
> struct. Clippy runs host-native and won't catch a missing gate — the wasm32
> cross-build in CI is what fails on it. Any `parse` helper you add is
> `#[cfg(feature = "runtime")]` too.

### 3. Wire the argv overlay and config-file section into each chassis

The derive emits `<Name>Overlay` (here `HttpOverlay`) with an `into_layer()`
method. For a field on an existing struct whose overlay is already flattened into
a chassis CLI, the new field rides the existing overlay automatically — confirm
your struct's overlay is reached. `HttpOverlay` is imported into
`crates/aether-chassis/src/cli.rs` and flattened into `CommonOverlay`:

```rust
#[command(flatten)]
pub http: HttpOverlay,
```

`CommonOverlay` is in turn flattened into `DesktopCli` and `HeadlessCli`, so both
full-stack chassis expose the flag. Each chassis loads its sectioned TOML file once,
then resolves the overlay against the subsystem's explicit section in
`resolve` (`crates/aether-chassis-{desktop,headless}/src/chassis.rs`):

```rust
let config_file = load_chassis_config(config)?;
let config_file = config_file.as_ref();

let http = resolve_with_file::<HttpConfig>(
    http.into_layer(),
    config_file,
    "http",
)?;
```

The section name is part of the chassis composition contract. For this config, a
file override is written under `[http]`:

```toml
[http]
require_https = true
```

`load_chassis_config` selects `--config PATH` first and falls back to
`AETHER_CONFIG_FILE`; either explicitly selected file is a hard boot error when it
cannot be read or parsed. `resolve_with_file` extracts the named section with
`file_section` and preserves field precedence **argv > env > config file > literal
default**. A missing `[http]` section simply contributes no layer, while a present
non-table or malformed section is a hard `ConfigError`. Absent flags resolve
`None` and fall through, so adding argv support does not shadow env or file values.

The flag name is mechanical: take the env key, drop the `AETHER_` prefix,
lowercase, hyphenate — `AETHER_HTTP_REQUIRE_HTTPS` becomes `--http-require-https`.
A bool flag accepts zero or one value (`--http-disable` ⇒ `true`,
`--http-disable=false` ⇒ `false`, absent ⇒ `None`).

### 4. Confirm the knob in the `--print-config` dump

Build and run any full-stack chassis with `--print-config` — it walks the same
declarations and prints every knob's env key, resolved value, source, default,
and doc, then exits before boot:

```sh
cargo run -p aether-chassis-headless --bin aether-substrate-headless -- --print-config
```

Your new field appears with its default. This command is the discovery surface:
the binaries exit before loading a selected TOML file, so use it to confirm that
the declaration is registered, not to test a `[http]` override. The dump is rendered by
`chassis_config_dump()` in `crates/aether-chassis/src/boot.rs`,
which walks `chassis_registry()`. That registry lists `&HttpConfigLayer::META`, so
a field on an existing struct shows up with no extra wiring — the META walk is the
discovery source of truth. If your knob is missing from the dump, the field isn't
  reaching the layer (re-check the `#[config]` hint and the `runtime` gate).

### 5. Run the deterministic local tier

```sh
cargo fmt -- --check
cargo clippy --all-targets -- -D warnings
```

Fix either failure locally, then push the implementation branch. CI owns the
full docs/test/wasm/package matrix and catches feature combinations the native
lint pass cannot.

## Adding a brand-new config struct

If the knob doesn't belong on any existing struct, you're declaring a new
`#[derive(aether_substrate::Config)]` struct. Three steps beyond the above:

- **Register its layer META for discovery.** Add `&YourConfigLayer::META` to the
  `METAS` slice in `chassis_registry()`
  (`crates/aether-chassis/src/boot.rs`) so the `--print-config` dump
  and the unknown-key sweep (`chassis_known_keys`) both see its knobs.
- **Flatten its overlay into a chassis CLI.** Import `YourOverlay` into
  `crates/aether-chassis/src/cli.rs` and `#[command(flatten)]` it into
  `CommonOverlay`, or into a per-chassis root in its own chassis crate
  (`crates/aether-chassis-{desktop,headless,hub}/src/cli.rs`).
- **Choose and wire a stable TOML section.** Pick the explicit section name that
  belongs to the subsystem, then call
  `resolve_with_file::<YourConfig>(overlay.into_layer(), config_file, "your-section")`
  in every chassis that carries it. Keep that string in lockstep across chassis;
  it is the operator-facing file API, and `file_section` validates a present
  section instead of silently skipping malformed input.

## Verify against current code

This recipe names files, symbols, and methods that move. Before following it,
confirm `HttpConfig`, `HttpConfigLayer`, `HttpOverlay`,
`load_chassis_config`, `file_section`, `resolve_with_file`, `into_layer`,
`chassis_registry`, and `chassis_config_dump` still exist where named — grep the
crates, and if a name has drifted, fix the recipe as part of your work.
