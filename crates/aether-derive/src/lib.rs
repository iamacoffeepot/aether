//! Engine-wide derive macros (ADR-0090 unit g, iamacoffeepot/aether#1264).
//!
//! The `Config` derive collapses a cap's domain struct + confique
//! `*Layer` + clap `*Overlay` + `from_layer` mapping into one
//! `#[derive(aether_substrate::Config)]` annotation. Re-exported from
//! `aether-substrate::Config`; downstream callers write the
//! `aether_substrate::Config` path rather than reaching in here.
//!
//! Crate skeleton — the actual emission lives in subsequent commits.

use proc_macro::TokenStream;

mod config;

/// Derive macro that turns a cap's resolved-config struct into the full
/// ADR-0090 quartet (Layer / Overlay / `FromArgvThenEnv` /
/// inherent `from_env` + `from_argv_then_env`).
///
/// ## Container attribute (required)
///
/// ```ignore
/// #[config(env_prefix = "AETHER_HTTP", cli_prefix = "http")]
/// ```
///
/// `env_prefix` joins with the upper-snake field name to form the env
/// key (`AETHER_HTTP_TIMEOUT_MS`). Override per-field via `env = "..."`
/// for unprefixed keys (`GEMINI_API_KEY`). `cli_prefix` joins with the
/// hyphen-lower field name to form the long flag (`--http-timeout-ms`).
///
/// ## Field attributes
///
/// - `#[config(default = <lit>)]` — confique default literal.
/// - `#[config(parse = <fn_path>)]` — confique `parse_env` function;
///   turbofish-bearing paths are supported (`parse_u32_ms_or::<DEFAULT_TIMEOUT_MS>`).
/// - `#[config(ms_duration)]` — domain field is `Duration`; Layer
///   carries `<field>_ms: u32`; `from_layer` does the millis → Duration
///   map.
/// - `#[config(csv_set)]` — domain + Layer share `HashSet<String>`;
///   overlay carries `Option<String>` and splits CSV inline.
/// - `#[config(env = "...")]` — per-field env-name override (used for
///   un-prefixed keys like `GEMINI_API_KEY`).
///
/// ## Type-driven emission (no explicit hint)
///
/// - `Option<String>` — `from_layer` always applies `.filter(|s| !s.is_empty())`
///   (empty env ≡ unset).
/// - `Option<<numeric>>` — Layer holds `Option<String>`; `from_layer`
///   does a soft `.parse().ok()` to preserve the prior
///   indistinguishable-from-unset semantics for unparseable values.
///
/// ## Cfg gating
///
/// The macro emits Layer + Overlay + `FromArgvThenEnv` impl + inherent
/// shims **unconditionally**. Cap authors who want the emission to ride
/// a `native` feature (so wasm builds skip confique + clap entirely)
/// wrap the derive in `#[cfg_attr(feature = "native", derive(...))]`
/// instead of `#[derive(...)]`. The domain struct itself stays
/// unconditional.
///
/// ## Escape hatch
///
/// `NamespaceRoots` (`aether-capabilities::fs`) carries runtime-computed
/// defaults (`dirs::data_dir()` / `current_exe()`) that confique cannot
/// express as literals. The cap declares `#[config(skip_from_layer)]`
/// at the container level — the derive emits the Layer + Overlay +
/// inherent shims, but skips the `FromArgvThenEnv` impl so the cap can
/// hand-write the `from_layer` body with the runtime fallbacks. This
/// is an exception, not a general feature — every other cap uses the
/// auto-emitted `from_layer`.
#[proc_macro_derive(Config, attributes(config))]
pub fn derive_config(input: TokenStream) -> TokenStream {
    config::derive(input)
}
