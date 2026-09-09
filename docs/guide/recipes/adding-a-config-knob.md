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

A config struct declares `impl Default` separately from the derive's
`default = ...` literals (the derive feeds the layer; `Default` feeds direct
construction in tests and call sites). Add your field's default to **both**. A
`*_defaults_match` test is what keeps them honest — the HTTP server's
`config_layer_defaults_match_the_named_consts`, in
`crates/aether-http/src/server/runtime/unit_tests.rs`, is the shape to copy:

```rust
#[test]
fn config_layer_defaults_match_the_named_consts() {
    use super::super::{DEFAULT_BIND_ADDR, HttpServerConfig, HttpServerConfigLayer};
    use confique::Config as _;
    let layer = HttpServerConfigLayer::builder().load().expect("defaults load");
    let default = HttpServerConfig::default();
    assert_eq!(layer.bind_addr, DEFAULT_BIND_ADDR);
    assert_eq!(layer.bind_addr, default.bind_addr);
    // …one pair per field
}
```

`<Name>ConfigLayer` is the derive-emitted layer type — you don't write it, but you
do reference it from the test. Add an assertion for your new field. It loads with
no `.env()` source, so it's env-free and CI-safe (issue 464). Where the default
is also a named `const`, assert against the const on both sides so the literal,
the `Default`, and the const cannot drift apart in pairs.

> **Gotcha — the `runtime` feature gate.** Every `#[derive(...)]` and `#[config]`
> attribute is wrapped in `#[cfg_attr(feature = "runtime", ...)]`, including the
> struct-level derive. A capability crate also cross-compiles to wasm, where
> the config machinery isn't present, so the wasm build must carry only the plain
> struct. Clippy runs host-native and won't catch a missing gate — the wasm32
> cross-build in CI is what fails on it. Any `parse` helper you add is
> `#[cfg(feature = "runtime")]` too.

### 3. Wire the argv overlay into the chassis CLI

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
full-stack chassis expose the flag. `ChassisCli::into_sources` then assembles the
whole stack once — it loads the `--config` file into a `ConfigSources` and stages
every flattened overlay's argv layer onto it through the derived `StageArgv` impl
— and hands that stack to the chassis builder
(`crates/aether-chassis/src/boot.rs`):

```rust
let mut sources = cli.into_sources()?;

// A value the chassis itself needs before composing.
let namespace_roots = sources.resolve::<NamespaceRoots>()?;

// Everything else: the builder resolves each cap's Config off the stack.
builder.with_config_sources(sources).with_actor::<HttpCapability>(())
```

`resolve` reads the member's `[section]` out of the loaded file itself, so the
section name is declared once on the config type (`#[config(section = "…")]`,
defaulting to its `cli_prefix`) rather than repeated at each chassis call site.
Reach for a chassis-side `sources.resolve::<C>()` only when the chassis needs the
value before composition (a driver's cadence, roots it must also pass
programmatically); an ordinary cap config is builder-resolved. For this config, a
file override is written under `[http]`:

```toml
[http]
require_https = true
```

`load_chassis_config` selects `--config PATH` first and falls back to
`AETHER_CONFIG_FILE`; either explicitly selected file is a hard boot error when it
cannot be read or parsed. Resolution extracts the named section with
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
cargo run -p aether-chassis-headless --bin aether-headless -- --print-config
```

Your new field appears with its default. This command is the discovery surface:
the binaries exit before loading a selected TOML file, so use it to confirm that
the declaration is registered, not to test a `[http]` override. The dump is
`ConfigManifest::dump`, and the manifest is *composition-derived* (ADR-0156):
`with_actor::<HttpCapability>(…)` accumulates that cap's `ConfigMember` record —
its section plus its layer `META` — so a field on an existing struct shows up
with no extra wiring, and a chassis lists exactly the knobs it composes. If your
knob is missing from the dump, the field isn't reaching the layer (re-check the
`#[config]` hint and the `runtime` gate); if the whole struct is missing, its cap
isn't composed on that chassis.

### 5. Run the deterministic local tier

```sh
cargo fmt -- --check
cargo clippy --workspace --all-targets -- -D warnings
```

Fix either failure locally, then push the implementation branch. CI owns the
full docs/test/wasm/package matrix and catches feature combinations the native
lint pass cannot.

## Adding a brand-new config struct

If the knob doesn't belong on any existing struct, you're declaring a new
`#[derive(aether_substrate::Config)]` struct. Three steps beyond the above:

- **Choose a stable TOML section.** The derive takes it from the struct's
  `cli_prefix`; pin a different one with `#[config(section = "…")]` when the
  historical file API and the flag prefix diverge. That string is the
  operator-facing file API, so it is chosen once and does not move.
- **Own it from a composed cap.** Discovery is composition-derived: the derive
  emits the `ConfigMember` impl, and `with_actor::<YourCapability>(…)` is what
  puts it in the chassis aggregate the `--print-config` dump and the unknown-key
  sweep walk. A config struct nothing composes appears nowhere. A non-cap
  chassis member is declared on the builder the same way.
- **Flatten its overlay into a chassis CLI.** Import `YourOverlay` into
  `crates/aether-chassis/src/cli.rs` and `#[command(flatten)]` it into
  `CommonOverlay`, or into a per-chassis root in its own chassis crate
  (`crates/aether-chassis-{desktop,headless,hub}/src/cli.rs`). The derived
  `StageArgv` impl on that root is what stages the overlay onto the source stack.

## Verify against current code

This recipe names files, symbols, and methods that move. Before following it,
confirm `HttpConfig`, `HttpConfigLayer`, `HttpOverlay`,
`load_chassis_config`, `file_section`, `ConfigSources::resolve`, `StageArgv`,
`ConfigMember`, and `ConfigManifest` still exist where named — grep the
crates, and if a name has drifted, fix the recipe as part of your work.
