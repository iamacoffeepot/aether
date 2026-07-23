// Actor macro codegen builds deeply-nested `quote!` trees from `if let Some(...)`
// branches; `map_or_else` would obscure the control flow. Allow at the crate
// root because cargo doesn't permit `[lints.clippy]` overrides alongside
// `lints.workspace = true` in the manifest (iamacoffeepot/aether#854 Phase 1.a).
#![allow(clippy::option_if_let_else)]

//! Proc-macro home for the actor SDK attributes: `#[actor]`,
//! `#[runtime]`, `#[handler]`, `#[fallback]`, `#[capability]`, and
//! `#[local]`, plus the `export_asset!` asset embed (ADR-0163). The
//! data-layer `Kind` / `Schema` derives live in `aether-data-derive`
//! and are re-exported by `aether-data`.

mod asset;
mod diagnostics;
mod handler_parse;
mod manifest;
mod native_expand;
mod opts;
mod wasm_expand;

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::quote;
use syn::{Fields, ItemImpl, ItemStruct, parse_macro_input};

use native_expand::{NativeEmit, expand_native_actor_trait, expand_struct_hosted_actor};
use opts::{ActorOpts, parse_actor_opts};
use wasm_expand::expand_wasm_actor;

// ADR-0033 phase 3: `#[actor]` on an `impl Component for C` block
// is the one receive path for every component. The macro emits:
//
//   (a) An inherent method `__aether_dispatch(&mut self, ctx, mail)
//       -> u32` on `C` that `export!`'s `receive_p32` shim calls. The
//       body matches `mail.kind()` against each `<K as Kind>::ID`
//       const (ADR-0030 Phase 2) and dispatches to the user-written
//       inherent handler method; a `#[fallback]` catches unmatched
//       kinds; strict receivers (no fallback) return
//       `DISPATCH_UNKNOWN_KIND` so the substrate's scheduler logs the
//       miss (issue #142).
//
//   (b) A wrapper around the user's `init` that prepends
//       `ctx.subscribe_input::<K>()` for every `K::IS_INPUT` handler
//       kind. Replaces the ADR-0027 `KindList::resolve_all` walker.
//       Guarded by `if <K as Kind>::IS_INPUT` so non-input kinds
//       compile down to no-ops.
//
//   (c) Two associated consts on `C`'s inherent impl —
//       `__AETHER_INPUTS_MANIFEST_LEN: usize` and
//       `__AETHER_INPUTS_MANIFEST: [u8; …LEN]` — carrying the
//       concatenated `aether.kinds.inputs` record bytes (one record per
//       `#[handler]`, one per `#[fallback]` if present, one for the
//       component-level doc if present, each prefixed with the section
//       version byte). The `#[link_section]` static that pins these
//       bytes into the wasm custom section is emitted by
//       `aether_actor::export!()` in the cdylib root crate, NOT
//       here. Sections only land where `export!()` runs (the cdylib
//       root); transitive rlib pulls of a `#[actor]`-using crate
//       carry only the const data and contribute no section bytes —
//       which is what keeps duplicate Component records from stacking
//       when a cdylib deps on a sibling cdylib's rlib output.
//
// The user's handler methods ride as inherent methods on `C` (since
// `impl Trait for C` can't host non-trait items); helpers go the same
// way. The trait impl retains only `init` and lifecycle hooks.
//
// Rustdoc capture: `///` comments on the impl block (component-level),
// each `#[handler]`, and each `#[fallback]` become MCP-facing prose. If
// a `# Agent` section is present, only that section's body is sent;
// otherwise the full doc is sent. `cargo doc` still renders the whole
// comment — the `# Agent` heading sits alongside `# Safety`/`# Examples`
// as a conventional reader-specific section.

/// Outer attribute on an `impl WasmActor for X` (or `impl Component for X`)
/// block. Reads the `#[handler]` / `#[fallback]` methods inside, then emits:
///
/// - One `impl HandlesKind<K> for X` per handler kind (gates type-driven
///   sender bounds — ADR-0075).
/// - The dispatch table inherent method `__aether_dispatch` that the
///   `export!` shim's `receive_p32` calls.
/// - The `aether.kinds.inputs` manifest consts (substrate reads them via
///   the wasm custom section the cdylib's `export!` pins in).
/// - The `Addressable`-trait const re-routing (`NAMESPACE` flows from the impl
///   block into a sibling `impl Addressable`).
///
/// Renamed from `#[actor]` in PR A of issue 533. Same behavior; the
/// new name reads as "decorate this actor's impl" — natural now that the
/// macro applies to any actor (and will extend to native chassis caps in
/// a follow-up).
#[proc_macro_attribute]
pub fn actor(attr: TokenStream, item: TokenStream) -> TokenStream {
    let opts = match parse_actor_opts(attr.into()) {
        Ok(opts) => opts,
        Err(e) => return e.to_compile_error().into(),
    };
    // ADR-0123: `#[actor]` may now sit on the capability *struct* (the
    // struct-hosted identity form) as well as on an `impl WasmActor /
    // NativeActor for X` block (the impl-hosted form). A struct input takes
    // the disk-read identity path — it parses the sibling runtime module file,
    // lifts the `NAMESPACE` const + `#[handler]` kinds out of the
    // `impl NativeActor` there, and emits the always-on identity markers
    // against the struct. Any other input is the existing impl-hosted path.
    let item2: TokenStream2 = item.clone().into();
    if let Ok(item_struct) = syn::parse2::<ItemStruct>(item2) {
        return match expand_struct_hosted_actor(&item_struct, &opts) {
            Ok(ts) => ts.into(),
            Err(e) => e.to_compile_error().into(),
        };
    }
    let item = parse_macro_input!(item as ItemImpl);
    match expand_handlers(item, &opts) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// `#[runtime]` (ADR-0123): sits on the `impl NativeActor for Cap` block in a
/// capability's `mod runtime;` file. It emits only the gated runtime surface —
/// `Lifecycle`, `Dispatch`, the `NativeActor` composition pinning `type State`,
/// and the handler bodies as an inherent impl — and consumes the `NAMESPACE`
/// const (lifted into `Addressable` by the struct-side `#[actor]`). It emits no
/// addressing markers; those come from the struct. The runtime impls ride the
/// `#[cfg]` on the `mod runtime;` line, so this attribute adds no gate of its
/// own.
#[proc_macro_attribute]
pub fn runtime(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "#[runtime] takes no arguments")
            .to_compile_error()
            .into();
    }
    let item = parse_macro_input!(item as ItemImpl);
    let is_native_actor =
        item.trait_.as_ref().and_then(|(_, p, _)| p.segments.last()).is_some_and(|s| s.ident == "NativeActor");
    if !is_native_actor {
        return syn::Error::new_spanned(
            &item.self_ty,
            "#[runtime] expects `impl NativeActor for X` — it emits the gated native \
             runtime surface for a struct-hosted (`#[actor]`-on-the-struct) split capability",
        )
        .to_compile_error()
        .into();
    }
    match expand_native_actor_trait(item, &ActorOpts::default(), NativeEmit::RuntimeOnly) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// Parsed `#[actor(...)]` attribute arguments. `singleton` / `instanced`
/// (ADR-0119) declare cardinality, mapped to the `Addressable::Resolver` per
/// transport.
#[proc_macro_attribute]
pub fn handler(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    // Real logic runs inside `#[actor]` (the enclosing impl-block
    // attribute scans for #[handler] markers). This standalone shim
    // only exists so rustc accepts `#[handler]` syntactically outside
    // macro expansion and so rust-analyzer doesn't redline it.
    syn::Error::new(proc_macro2::Span::call_site(), "#[handler] may only appear inside a `#[actor]` impl block")
        .to_compile_error()
        .into()
}

#[proc_macro_attribute]
pub fn fallback(_attr: TokenStream, _item: TokenStream) -> TokenStream {
    // Same story as `#[handler]` — marker attribute consumed by the
    // enclosing `#[actor]` scan. Standalone invocation is a
    // compile-time error.
    syn::Error::new(proc_macro2::Span::call_site(), "#[fallback] may only appear inside a `#[actor]` impl block")
        .to_compile_error()
        .into()
}

/// `#[local]` — attribute macro that declares a struct as
/// per-actor scratch storage (issue 582). Passes the struct
/// through unchanged and emits `impl ::aether_actor::Local for T
/// {}` underneath.
///
/// The trait requires `Default + Send + 'static` (native) /
/// `Default + 'static` (wasm) — the user supplies `Default` either
/// via `#[derive(Default)]` or a hand-rolled impl, depending on
/// whether the struct's fields default trivially. The macro
/// deliberately does *not* auto-derive `Default` so types that
/// need a custom default (e.g. a counter that starts at 1, a Vec
/// with reserved capacity) aren't fighting the derive.
///
/// ```ignore
/// #[derive(Default)]
/// #[local]
/// struct LogBuffer(Vec<LogEvent>);
///
/// #[derive(Default)]
/// #[local]
/// struct AppState {
///     pending: u32,
///     events: Vec<Event>,
/// }
///
/// // Custom Default:
/// #[local]
/// struct Retries { count: u32 }
/// impl Default for Retries {
///     fn default() -> Self { Self { count: 3 } }
/// }
/// ```
///
/// Generics are forwarded — `#[local] struct Foo<T>(T);` emits
/// `impl<T: Default + Send + 'static> Local for Foo<T>`. In
/// practice Local types are concrete; the generics support is
/// mostly for completeness.
#[proc_macro_attribute]
pub fn local(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let input = parse_macro_input!(item as ItemStruct);
    let name = &input.ident;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();
    quote! {
        #input
        impl #impl_generics ::aether_actor::Local for #name #ty_generics #where_clause {}
    }
    .into()
}

// ADR-0119: `#[derive(Singleton)]` / `#[derive(Instanced)]` /
// `#[derive(Embeddable)]` are retired. Cardinality is now the
// `Addressable::Resolver` (`One` / `Many` / `Embedded` / `EmbeddedMany`); the
// `Singleton` / `Instanced` markers derive from it by blanket impl, so a
// hand-emitted marker would conflict. `#[actor]` emits the resolver from its
// `singleton` / `instanced` arg; a hand-written actor sets `type Resolver`
// directly.

/// `#[capability]` — attribute macro for native chassis capability
/// structs. Cfg-gates every field with `#[cfg(feature = "runtime")]`
/// so the cap's runtime fields disappear from non-runtime builds (wasm
/// guests linking the cap's depable rlib for type/marker visibility
/// don't pay for `cpal::Stream`, etc.).
///
/// Issue 552 stage 0 ships the macro as a thin shim — fields get the
/// blanket `#[cfg(feature = "runtime")]` gate and the struct itself
/// passes through unchanged. Stage 1 may extend the macro to gate
/// trait impls, derive `Default`, or pre-emit the empty
/// stage-0-required `Singleton` marker; this skeleton lands now so
/// capability authors can adopt the new shape without waiting on
/// stage 1 details.
///
/// ```ignore
/// #[capability]
/// pub struct AudioCapability {
///     // Both fields gain `#[cfg(feature = "runtime")]` automatically.
///     audio_sender: Option<AudioEventSender>,
///     audio_thread: Option<JoinHandle<()>>,
/// }
/// ```
#[proc_macro_attribute]
pub fn capability(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(proc_macro2::Span::call_site(), "#[capability] takes no arguments")
            .to_compile_error()
            .into();
    }
    let mut item = parse_macro_input!(item as ItemStruct);
    // Issue 552 stage 4: gate fields on `not(target_family = "wasm")`
    // to match the macro-emitted `NativeActor` / `NativeDispatch`
    // impls. Wasm builds see the cap struct with no fields (a pure
    // marker), which is what typed `ctx.actor::<R>().send(...)` needs;
    // host builds see the full struct.
    match &mut item.fields {
        Fields::Named(fields) => {
            for field in &mut fields.named {
                let already_cfg = field.attrs.iter().any(|a| a.path().is_ident("cfg"));
                if !already_cfg {
                    field.attrs.push(syn::parse_quote!(#[cfg(not(target_family = "wasm"))]));
                }
            }
        }
        Fields::Unnamed(fields) => {
            for field in &mut fields.unnamed {
                let already_cfg = field.attrs.iter().any(|a| a.path().is_ident("cfg"));
                if !already_cfg {
                    field.attrs.push(syn::parse_quote!(#[cfg(not(target_family = "wasm"))]));
                }
            }
        }
        Fields::Unit => {
            // Marker structs: nothing to gate.
        }
    }
    quote! { #item }.into()
}

fn expand_handlers(item: ItemImpl, opts: &ActorOpts) -> syn::Result<TokenStream2> {
    if let Some((_, trait_path, _)) = item.trait_.as_ref() {
        // Pattern-match the trait path's last identifier so the macro
        // works regardless of the user's import style — bare
        // `WasmActor` / `NativeActor`, `aether_actor::WasmActor`,
        // `aether_substrate::NativeActor`, etc. all resolve here.
        let last = trait_path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
        match last.as_str() {
            "NativeActor" => expand_native_actor_trait(item, opts, NativeEmit::Full),
            // `WasmActor` is the post-552 trait name; `Component` is
            // the back-compat alias retained until stage 4.
            "WasmActor" | "Component" => expand_wasm_actor(item, opts),
            other => Err(syn::Error::new_spanned(
                trait_path,
                format!(
                    "#[actor] expects `impl WasmActor for X`, `impl NativeActor for X`, or                      `impl Component for X` (back-compat alias) — got `{other}`",
                ),
            )),
        }
    } else {
        // Inherent `impl X { … }` is rejected — every native chassis cap
        // now goes through `#[actor] impl NativeActor for X`. Pre-issue-688
        // this arm emitted `impl Dispatch for X` for the legacy
        // `Builder::with(cap)` facade path; that path retired alongside
        // the `Dispatch` trait itself.
        Err(syn::Error::new_spanned(
            &item.self_ty,
            "#[actor] expects `impl WasmActor for X`, `impl NativeActor for X`, or              `impl Component for X` (back-compat alias) — inherent `impl X { … }`              is no longer supported",
        ))
    }
}

/// `export_asset!("path/to/file")` — embed an asset file in the
/// component's wasm as a custom section named `aether.asset.<path>`
/// (ADR-0163 §2).
///
/// The path is resolved relative to the invoking source file, exactly
/// like `include_bytes!`, and the path string as written is the
/// asset's name — the key the ADR-0163 load window
/// (`AssetWindow::asset`) and catalog report.
///
/// ```ignore
/// aether_actor::export_asset!("sprites/slime.png");
/// ```
///
/// The bytes ride a custom section keyed by the asset path — the
/// section the host-side load window and catalog index — and are not
/// addressable from guest code. The static that carries them stays out
/// of linear memory: rustc leaves an unreferenced `#[link_section]`
/// static as a dead internal global, and this expansion withholds the
/// `#[used]` pin, so wasm-ld's default `--gc-sections` collects it before
/// the shipped wasm (ADR-0163 §2, "never instantiated into linear
/// memory"); the custom section itself is emitted unconditionally. See
/// `asset.rs`. Asset names must be unique across the final component:
/// a second `export_asset!` of the same path — anywhere in the crate
/// graph — fails at link time with a duplicate-symbol error rather than
/// silently concatenating sections. On non-wasm targets no section is
/// emitted, but the file is still `include_bytes!`-checked so a bad
/// path fails native builds too.
#[proc_macro]
pub fn export_asset(input: TokenStream) -> TokenStream {
    let path_lit = parse_macro_input!(input as syn::LitStr);
    match asset::expand_export_asset(&path_lit) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}
