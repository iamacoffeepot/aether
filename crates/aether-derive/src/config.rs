//! `#[derive(aether_substrate::Config)]` — full ADR-0090 quartet
//! generation (ADR-0090 unit g, iamacoffeepot/aether#1264).
//!
//! Architecture: parse the container + per-field attrs into a
//! `ConfigInput` IR, then quote out four pieces gated on
//! `#[cfg(feature = "native")]`:
//!
//!   1. A `<Name>Layer` struct (`#[derive(confique::Config)]`) carrying
//!      the wire-shape primitive per field.
//!   2. An `impl aether_substrate::FromArgvThenEnv for <Name>` whose
//!      `from_layer` body is assembled from per-field hints.
//!   3. Inherent `pub fn from_env()` + `pub fn from_argv_then_env(argv)`
//!      delegating to the trait so call sites needn't import it.
//!   4. A `<Name>Overlay` struct (`#[derive(clap::Args, ...)]`) with
//!      `Option<T>` per field + a `pub fn into_layer(self)`.
//!
//! The attribute parser is hand-rolled (`Attribute::parse_nested_meta`)
//! — same pattern as `aether-actor-derive` and `aether-data-derive`,
//! deliberately no `darling` dep.

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote};
use syn::{
    Attribute, Data, DataStruct, DeriveInput, Expr, Field, Fields, GenericArgument, Ident, LitStr,
    Path, PathArguments, Type, TypePath, parse_macro_input, spanned::Spanned,
};

/// Container-level `#[config(env_prefix = "...", cli_prefix = "...")]`.
struct ContainerAttr {
    env_prefix: String,
    cli_prefix: String,
    /// `#[config(skip_from_layer)]` — opt the cap out of the
    /// auto-emitted `FromArgvThenEnv` impl when its `from_layer` body
    /// can't be assembled mechanically (`NamespaceRoots`'s
    /// runtime-computed defaults). The Layer + Overlay + inherent
    /// shims still ride the derive; the cap hand-writes the impl.
    skip_from_layer: bool,
}

/// Per-field `#[config(...)]` parsing result.
#[derive(Default)]
struct FieldAttr {
    /// `env = "..."` — overrides the prefix-joined env key.
    env: Option<String>,
    /// `cli_long = "..."` — overrides the prefix-joined `--cli-flag`
    /// (the long form). Used when the domain field name doesn't match
    /// the historical flag (e.g. domain `default_timeout` but the
    /// chassis flag has shipped as `--http-timeout-ms`). `cli_id` is
    /// derived from `cli_long` by `s/-/_/g`.
    cli_long: Option<String>,
    /// `default = <lit>` — confique default literal expression.
    default: Option<Expr>,
    /// `parse = <fn_path>` — confique `parse_env`. Stored as `Path` so
    /// turbofish (`parse_u32_ms_or::<DEFAULT_TIMEOUT_MS>`) round-trips.
    parse: Option<Path>,
    /// `ms_duration` hint — domain field is `Duration`, Layer carries
    /// `<field>_ms: u32`.
    ms_duration: bool,
    /// `csv_set` hint — overlay accepts `Option<String>` and splits CSV.
    csv_set: bool,
    /// `layer_field = "..."` — overrides the Layer-side field
    /// identifier (and the env key derivation if `env` isn't also set).
    /// Used when the domain name differs from the historical Layer
    /// shape (e.g. `default_timeout` → Layer field `timeout_ms` not
    /// `default_timeout_ms`).
    layer_field: Option<String>,
}

/// One field's resolved shape after attribute + type-driven inference.
struct FieldInfo {
    /// Original field identifier (domain + most Layer field names).
    ident: Ident,
    /// Layer field type (often the same as the domain; differs under
    /// `ms_duration`, `Option<numeric>`).
    layer_ty: Type,
    /// Overlay field type (typically `Option<Layer field's inner>`).
    overlay_ty: Type,
    /// Layer ident — usually the same as the domain ident; under
    /// `ms_duration` it's `<field>_ms`.
    layer_ident: Ident,
    /// `from_layer` body fragment that constructs this domain field.
    /// `field_name: <expr from layer>`.
    from_layer_expr: TokenStream2,
    /// Attribute fragments for the Layer field (`#[config(env = …, parse_env = …, default = …)]`).
    layer_attrs: TokenStream2,
    /// Attribute fragments for the Overlay field (`#[arg(id = …, long = …, …)]`).
    overlay_attrs: TokenStream2,
    /// `into_layer` body fragment that pushes this overlay field into
    /// the partial layer (`if let Some(v) = self.foo { layer.bar = Some(v); }`).
    into_layer_stmt: TokenStream2,
}

pub fn derive(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    match expand(&input) {
        Ok(ts) => ts.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let container = parse_container_attr(&input.attrs)?;
    let domain_ident = &input.ident;
    let layer_ident = format_ident!("{}Layer", domain_ident);
    let overlay_ident = format_ident!(
        "{}Overlay",
        // Strip the trailing `Config` so `HttpConfig` → `HttpOverlay`.
        // `NamespaceRoots` (no `Config` suffix) stays `NamespaceRootsOverlay`.
        domain_ident
            .to_string()
            .strip_suffix("Config")
            .unwrap_or(&domain_ident.to_string())
    );
    let vis = &input.vis;

    let fields = collect_fields(input, &container)?;

    let layer_struct = emit_layer_struct(&layer_ident, vis, &fields);
    let trait_impl = if container.skip_from_layer {
        TokenStream2::new()
    } else {
        emit_trait_impl(domain_ident, &layer_ident, &fields)
    };
    let inherent_impl = emit_inherent_impl(domain_ident, &layer_ident);
    let overlay_struct = emit_overlay_struct(&overlay_ident, &layer_ident, vis, &fields);

    Ok(quote! {
        #layer_struct
        #trait_impl
        #inherent_impl
        #overlay_struct
    })
}

fn parse_container_attr(attrs: &[Attribute]) -> syn::Result<ContainerAttr> {
    let mut env_prefix: Option<String> = None;
    let mut cli_prefix: Option<String> = None;
    let mut skip_from_layer = false;

    for attr in attrs {
        if !attr.path().is_ident("config") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("env_prefix") {
                env_prefix = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("cli_prefix") {
                cli_prefix = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("skip_from_layer") {
                skip_from_layer = true;
                Ok(())
            } else {
                Err(meta.error(
                    "unknown container attribute; expected one of \
                     `env_prefix = \"...\"`, `cli_prefix = \"...\"`, \
                     `skip_from_layer`",
                ))
            }
        })?;
    }

    let env_prefix = env_prefix.ok_or_else(|| {
        let span = attrs.first().map_or_else(Span::call_site, Spanned::span);
        syn::Error::new(
            span,
            "missing `#[config(env_prefix = \"...\")]` container attribute",
        )
    })?;
    let cli_prefix = cli_prefix.ok_or_else(|| {
        let span = attrs.first().map_or_else(Span::call_site, Spanned::span);
        syn::Error::new(
            span,
            "missing `#[config(cli_prefix = \"...\")]` container attribute",
        )
    })?;

    Ok(ContainerAttr {
        env_prefix,
        cli_prefix,
        skip_from_layer,
    })
}

fn parse_field_attr(attrs: &[Attribute]) -> syn::Result<FieldAttr> {
    let mut out = FieldAttr::default();
    for attr in attrs {
        if !attr.path().is_ident("config") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("env") {
                out.env = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("cli_long") {
                out.cli_long = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else if meta.path.is_ident("default") {
                out.default = Some(meta.value()?.parse::<Expr>()?);
                Ok(())
            } else if meta.path.is_ident("parse") {
                out.parse = Some(meta.value()?.parse::<Path>()?);
                Ok(())
            } else if meta.path.is_ident("ms_duration") {
                out.ms_duration = true;
                Ok(())
            } else if meta.path.is_ident("csv_set") {
                out.csv_set = true;
                Ok(())
            } else if meta.path.is_ident("layer_field") {
                out.layer_field = Some(meta.value()?.parse::<LitStr>()?.value());
                Ok(())
            } else {
                Err(meta.error(
                    "unknown field attribute; expected one of \
                     `env = \"...\"`, `cli_long = \"...\"`, `default = <lit>`, \
                     `parse = <fn_path>`, `ms_duration`, `csv_set`, \
                     `layer_field = \"...\"`",
                ))
            }
        })?;
    }
    Ok(out)
}

fn collect_fields(input: &DeriveInput, container: &ContainerAttr) -> syn::Result<Vec<FieldInfo>> {
    let Data::Struct(DataStruct { fields, .. }) = &input.data else {
        return Err(syn::Error::new_spanned(
            &input.ident,
            "`#[derive(Config)]` only supports structs with named fields",
        ));
    };
    let Fields::Named(named) = fields else {
        return Err(syn::Error::new_spanned(
            fields,
            "`#[derive(Config)]` only supports structs with named fields",
        ));
    };

    named
        .named
        .iter()
        .map(|f| field_info(f, container))
        .collect()
}

fn field_info(field: &Field, container: &ContainerAttr) -> syn::Result<FieldInfo> {
    let ident = field
        .ident
        .clone()
        .expect("checked by `collect_fields` — named struct");
    let attr = parse_field_attr(&field.attrs)?;
    let domain_ty = field.ty.clone();
    let span = field.span();

    if attr.ms_duration && !is_duration_type(&domain_ty) {
        return Err(syn::Error::new(
            span,
            "`ms_duration` hint requires field type `std::time::Duration`",
        ));
    }

    let is_bool = is_bool_type(&domain_ty);
    let inner_option_ty = unwrap_option(&domain_ty);
    // Whether this is `Option<numeric>` whose Layer rep is `Option<String>`.
    let is_option_numeric = inner_option_ty.as_ref().is_some_and(is_numeric_type);
    let is_option_string = inner_option_ty
        .as_ref()
        .is_some_and(|t| matches!(t, Type::Path(tp) if path_is(&tp.path, "String")));

    // Layer ident:
    //   - `layer_field = "..."` override always wins.
    //   - else `ms_duration` renames to `<field>_ms`.
    //   - else same as domain ident.
    let layer_ident = if let Some(name) = &attr.layer_field {
        Ident::new(name, span)
    } else if attr.ms_duration {
        format_ident!("{}_ms", ident)
    } else {
        ident.clone()
    };

    // Layer field type:
    // - `ms_duration` → `u32`
    // - `Option<numeric>` → `Option<String>` (preserves soft-parse on bad input)
    // - everything else → domain type
    //
    // The `Option<...>` shape is the bare identifier (not the
    // ::core::option::Option absolute path) — `clap`'s `Args` derive
    // detects optionality by matching the literal `Option` segment in
    // the field type. An absolute path defeats that match and clap
    // tries to value-parse the whole `Option<T>` (which never works).
    let layer_ty: Type = if attr.ms_duration {
        syn::parse_quote!(u32)
    } else if is_option_numeric {
        syn::parse_quote!(Option<String>)
    } else {
        domain_ty.clone()
    };

    // Overlay field type — `Option<inner of layer or domain>`:
    // - `ms_duration` → `Option<u32>`
    // - `Option<T>` (already optional) → `Option<T>` keeps T
    // - `csv_set` → `Option<String>`
    // - bool → `Option<bool>`
    // - other → `Option<T>`
    let overlay_ty: Type = if attr.ms_duration {
        syn::parse_quote!(Option<u32>)
    } else if attr.csv_set {
        syn::parse_quote!(Option<String>)
    } else if inner_option_ty.is_some() {
        // domain is already `Option<X>` — the overlay slot is the same
        // `Option<X>` (the env-side soft `Option<String>` is internal
        // to the Layer; on the argv side the value is already typed).
        domain_ty.clone()
    } else {
        let inner = &domain_ty;
        syn::parse_quote!(Option<#inner>)
    };

    // Env key: explicit `env` override > layer-ident-derived
    //   `<PREFIX>_<LAYER_IDENT_UPPER>` > domain-ident-derived
    //   `<PREFIX>_<DOMAIN_IDENT_UPPER>`. The layer-ident path covers
    //   the common `ms_duration` shape so the env key follows the
    //   stored-as-ms convention without an explicit override.
    let env_key = attr
        .env
        .clone()
        .unwrap_or_else(|| format!("{}_{}", container.env_prefix, layer_ident).to_uppercase());
    // CLI flag: explicit `cli_long` override > prefix-joined
    //   layer-ident. Using the layer ident (rather than the domain
    //   ident) keeps the flag honest about the wire shape — the user
    //   typing `--http-timeout-ms 5000` is setting the millisecond
    //   knob, not the `Duration`.
    let cli_long = attr.cli_long.clone().unwrap_or_else(|| {
        format!(
            "{}-{}",
            container.cli_prefix,
            layer_ident.to_string().replace('_', "-")
        )
    });
    let cli_id = cli_long.replace('-', "_");

    let layer_attrs = build_layer_attrs(&env_key, attr.default.as_ref(), attr.parse.as_ref());
    let overlay_attrs = build_overlay_attrs(&cli_id, &cli_long, is_bool);

    let from_layer_expr = build_from_layer_expr(
        &ident,
        &layer_ident,
        &domain_ty,
        attr.ms_duration,
        is_option_string,
        is_option_numeric,
    );
    let into_layer_stmt =
        build_into_layer_stmt(&ident, &layer_ident, attr.csv_set, is_option_numeric);

    Ok(FieldInfo {
        ident,
        layer_ty,
        overlay_ty,
        layer_ident,
        from_layer_expr,
        layer_attrs,
        overlay_attrs,
        into_layer_stmt,
    })
}

fn build_layer_attrs(env_key: &str, default: Option<&Expr>, parse: Option<&Path>) -> TokenStream2 {
    let mut inner = TokenStream2::new();
    inner.extend(quote! { env = #env_key });
    if let Some(parse) = parse {
        inner.extend(quote! { , parse_env = #parse });
    }
    if let Some(default) = default {
        inner.extend(quote! { , default = #default });
    }
    quote! { #[config(#inner)] }
}

fn build_overlay_attrs(cli_id: &str, cli_long: &str, is_bool: bool) -> TokenStream2 {
    if is_bool {
        quote! {
            #[arg(
                id = #cli_id,
                long = #cli_long,
                num_args = 0..=1,
                default_missing_value = "true"
            )]
        }
    } else {
        quote! {
            #[arg(id = #cli_id, long = #cli_long)]
        }
    }
}

fn build_from_layer_expr(
    domain_ident: &Ident,
    layer_ident: &Ident,
    domain_ty: &Type,
    ms_duration: bool,
    is_option_string: bool,
    is_option_numeric: bool,
) -> TokenStream2 {
    if ms_duration {
        // Domain stays the domain ident; layer is `<ident>_ms`.
        quote! {
            #domain_ident: ::std::time::Duration::from_millis(::core::convert::From::from(layer.#layer_ident))
        }
    } else if is_option_string {
        // `Option<String>` — empty string ≡ unset.
        quote! {
            #domain_ident: layer.#layer_ident.filter(|s| !s.is_empty())
        }
    } else if is_option_numeric {
        // Layer holds `Option<String>`; parse softly here.
        let inner = unwrap_option(domain_ty).expect("checked is_option_numeric");
        quote! {
            #domain_ident: layer.#layer_ident.and_then(|s| s.parse::<#inner>().ok())
        }
    } else {
        quote! {
            #domain_ident: layer.#layer_ident
        }
    }
}

fn build_into_layer_stmt(
    domain_ident: &Ident,
    layer_ident: &Ident,
    csv_set: bool,
    is_option_numeric: bool,
) -> TokenStream2 {
    if csv_set {
        // Overlay value is `Option<String>` — split CSV into a `HashSet`.
        quote! {
            if let ::core::option::Option::Some(s) = self.#domain_ident {
                let set: ::std::collections::HashSet<::std::string::String> = s
                    .split(',')
                    .map(::core::primitive::str::trim)
                    .filter(|h| !h.is_empty())
                    .map(::core::primitive::str::to_string)
                    .collect();
                layer.#layer_ident = ::core::option::Option::Some(set);
            }
        }
    } else if is_option_numeric {
        // Layer slot is `Option<String>` — stringify the typed argv.
        quote! {
            if let ::core::option::Option::Some(v) = self.#domain_ident {
                layer.#layer_ident = ::core::option::Option::Some(::std::string::ToString::to_string(&v));
            }
        }
    } else {
        quote! {
            if let ::core::option::Option::Some(v) = self.#domain_ident {
                layer.#layer_ident = ::core::option::Option::Some(v);
            }
        }
    }
}

fn emit_layer_struct(
    layer_ident: &Ident,
    vis: &syn::Visibility,
    fields: &[FieldInfo],
) -> TokenStream2 {
    let field_decls = fields.iter().map(|f| {
        let attrs = &f.layer_attrs;
        let ident = &f.layer_ident;
        let ty = &f.layer_ty;
        quote! {
            #attrs
            #vis #ident: #ty,
        }
    });
    quote! {
        #[derive(::confique::Config)]
        #vis struct #layer_ident {
            #(#field_decls)*
        }
    }
}

fn emit_trait_impl(
    domain_ident: &Ident,
    layer_ident: &Ident,
    fields: &[FieldInfo],
) -> TokenStream2 {
    let body = fields.iter().map(|f| &f.from_layer_expr);
    quote! {
        impl ::aether_substrate::FromArgvThenEnv for #domain_ident {
            type Layer = #layer_ident;

            fn from_layer(layer: #layer_ident) -> Self {
                Self {
                    #( #body, )*
                }
            }
        }
    }
}

fn emit_inherent_impl(domain_ident: &Ident, layer_ident: &Ident) -> TokenStream2 {
    quote! {
        impl #domain_ident {
            /// Resolve every field from `AETHER_*` (or per-field override)
            /// env vars. Chassis-main edge only — substrate-core never
            /// reads process env (issue 464).
            ///
            /// # Panics
            ///
            /// Panics only if the layer's literal defaults are themselves
            /// malformed — a programmer error caught by the cap's
            /// `*_defaults_match` test, never a runtime config fault
            /// (env values flow through total parsers).
            #[must_use]
            pub fn from_env() -> Self {
                use ::aether_substrate::FromArgvThenEnv as _;
                use ::confique::Config as _;

                let layer = #layer_ident::builder()
                    .env()
                    .load()
                    .expect(concat!(stringify!(#layer_ident), " defaults are well-formed"));
                <Self as ::aether_substrate::FromArgvThenEnv>::from_layer(layer)
            }

            /// Resolve with an argv-derived partial layer shadowing
            /// `AETHER_*` env (ADR-0090 unit d). Argv-set fields win;
            /// unset (`None`) fall through to env, then literal defaults.
            ///
            /// # Panics
            ///
            /// Same condition as [`Self::from_env`].
            #[must_use]
            pub fn from_argv_then_env(
                argv: <#layer_ident as ::confique::Config>::Layer,
            ) -> Self {
                <Self as ::aether_substrate::FromArgvThenEnv>::from_argv_then_env(argv)
            }
        }
    }
}

fn emit_overlay_struct(
    overlay_ident: &Ident,
    layer_ident: &Ident,
    vis: &syn::Visibility,
    fields: &[FieldInfo],
) -> TokenStream2 {
    let field_decls = fields.iter().map(|f| {
        let attrs = &f.overlay_attrs;
        let ident = &f.ident;
        let ty = &f.overlay_ty;
        quote! {
            #attrs
            #vis #ident: #ty,
        }
    });
    let into_layer_stmts = fields.iter().map(|f| &f.into_layer_stmt);
    quote! {
        #[derive(::clap::Args, ::core::fmt::Debug, ::core::default::Default, ::core::clone::Clone)]
        #vis struct #overlay_ident {
            #(#field_decls)*
        }

        impl #overlay_ident {
            /// Write argv-set fields into a fresh partial layer. `None`
            /// → partial `None` (env / default takes over); `Some(v)` →
            /// partial `Some(v)`.
            #[must_use]
            pub fn into_layer(self) -> <#layer_ident as ::confique::Config>::Layer {
                use ::confique::Layer as _;
                let mut layer = <<#layer_ident as ::confique::Config>::Layer as ::confique::Layer>::empty();
                #( #into_layer_stmts )*
                layer
            }
        }
    }
}

fn is_duration_type(ty: &Type) -> bool {
    if let Type::Path(TypePath { path, .. }) = ty {
        return path.segments.last().is_some_and(|s| s.ident == "Duration");
    }
    false
}

fn is_bool_type(ty: &Type) -> bool {
    if let Type::Path(TypePath { path, .. }) = ty {
        return path_is(path, "bool");
    }
    false
}

fn is_numeric_type(ty: &Type) -> bool {
    if let Type::Path(TypePath { path, .. }) = ty
        && let Some(seg) = path.segments.last()
    {
        let name = seg.ident.to_string();
        return matches!(
            name.as_str(),
            "u8" | "u16"
                | "u32"
                | "u64"
                | "u128"
                | "usize"
                | "i8"
                | "i16"
                | "i32"
                | "i64"
                | "i128"
                | "isize"
                | "f32"
                | "f64"
        );
    }
    false
}

fn unwrap_option(ty: &Type) -> Option<Type> {
    if let Type::Path(TypePath { path, .. }) = ty
        && let Some(seg) = path.segments.last()
        && seg.ident == "Option"
        && let PathArguments::AngleBracketed(args) = &seg.arguments
        && let Some(GenericArgument::Type(inner)) = args.args.first()
    {
        return Some(inner.clone());
    }
    None
}

fn path_is(path: &Path, name: &str) -> bool {
    path.segments.last().is_some_and(|s| s.ident == name)
}
