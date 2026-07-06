//! Proc macros for the typed route-authoring surface over the
//! `aether.http.server` capability (ADR-0131). Two attribute macros,
//! re-exported through `aether_capabilities::http` so consumers write
//! `#[http::router]` / `#[http::route]` next to the `http::FromRequest`
//! / `http::Ctx` runtime types the parent crate owns.
//!
//! `#[http::router]` sits on an actor's `impl` block, *above* `#[actor]`
//! (or `#[runtime]`). Attribute macros expand outer-first, so `router`
//! runs first: it consumes the `#[http::route(<Method|any>, "<prefix>")]`
//! attributes on methods, mints a hidden request-shaped route kind per
//! route, rewrites each routed method into a `#[handler]` glue method
//! driven by `FromRequest` extraction, injects the `RegisterRouteSelf`
//! registration into `wire`, and hands `#[actor]` an ordinary impl block.
//! Bare `#[http::router]` registers every route exclusively (today's
//! default); `#[http::router(shared)]` registers them all `shared: true`
//! instead (ADR-0136) — the impl-level opt-in for a component built to
//! run as N interchangeable instances of one round-robin member set.
//!
//! The macros only emit token paths at the runtime vocabulary
//! (`::aether_capabilities::http::…`, `::aether_data::…`, `::serde::…`);
//! they name none of those types directly, so this crate depends on
//! nothing but `syn` / `quote` / `proc-macro2`.

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Attribute, Expr, ExprLit, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, ItemImpl, Lit,
    LitStr, Pat, PatType, PathArguments, ReturnType, Type, TypePath, parse_macro_input,
    parse_quote,
};

/// `#[http::route(<Method|any>, "<prefix>")]` — a marker attribute
/// consumed by `#[http::router]` on the enclosing impl. Reaching this
/// expansion means the impl is missing `#[http::router]`; the emitted
/// `compile_error!` says so. Under correct usage `router` strips the
/// attribute before the compiler resolves it, so this body never runs.
#[proc_macro_attribute]
pub fn route(_args: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    quote_spanned! { item.span() =>
        ::core::compile_error!(
            "#[http::route] requires #[http::router] on the enclosing impl block (written above #[actor])"
        );
        #item
    }
    .into()
}

/// `#[http::router]` — the impl-block attribute that expands the typed
/// route-authoring surface (ADR-0131). Written above `#[actor]`. Takes no
/// arguments (today's exclusive registration) or the bare ident `shared`
/// (ADR-0136 joint opt-in — every route on the impl registers
/// `shared: true`, so N instances of the component join one round-robin
/// member set).
#[proc_macro_attribute]
pub fn router(args: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemImpl);
    let shared = match parse_router_args(TokenStream2::from(args)) {
        Ok(shared) => shared,
        Err(err) => return err.into_compile_error().into(),
    };
    match expand_router(item, shared) {
        Ok(tokens) => tokens.into(),
        Err(err) => err.into_compile_error().into(),
    }
}

/// Parse `#[http::router(...)]`'s optional argument: absent (`shared =
/// false`, today's exclusive semantics) or the bare ident `shared`
/// (`shared = true`, ADR-0136 joint opt-in). Anything else — a different
/// ident, a value, more than one token — is a spanned `compile_error!`
/// naming the two accepted forms.
fn parse_router_args(args: TokenStream2) -> syn::Result<bool> {
    if args.is_empty() {
        return Ok(false);
    }
    match syn::parse2::<Ident>(args.clone()) {
        Ok(ident) if ident == "shared" => Ok(true),
        _ => Err(syn::Error::new_spanned(
            args,
            "#[http::router] accepts no arguments, or the bare ident `shared` \
             (ADR-0136 joint opt-in)",
        )),
    }
}

/// Parsed `#[http::route(Get, "/api/users")]` arguments: an HTTP-method
/// identifier (or the bare `any`) and a path-prefix string literal.
struct RouteArgs {
    method: Ident,
    prefix: LitStr,
}

impl Parse for RouteArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let method: Ident = input.parse()?;
        let _comma: syn::Token![,] = input.parse()?;
        let prefix: LitStr = input.parse()?;
        Ok(Self { method, prefix })
    }
}

/// Everything the emitter needs about one routed method, gathered as
/// `#[http::router]` walks the impl block.
struct Routed {
    /// The retained user method's name (also the glue's call target).
    fn_name: Ident,
    /// `Some(HttpMethod::…)` / `None` token for the method filter.
    method_expr: TokenStream2,
    /// The route's path prefix.
    prefix: LitStr,
    /// Minted route-kind struct identifier (a sibling item).
    kind_struct: Ident,
    /// Minted route-kind wire name (`"{NAMESPACE}.route.{fn_name}"`).
    kind_name: LitStr,
    /// The method's first parameter (receiver or `state: &mut Self::State`),
    /// copied verbatim onto the glue handler.
    first_arg: FnArg,
    /// How the glue calls back into the retained method.
    call_style: CallStyle,
    /// The transport ctx type `C` from `http::Ctx<'_, C>`.
    ctx_c: Type,
    /// The `(ident, type)` of each `FromRequest` extractor parameter.
    extractors: Vec<(Ident, Type)>,
    /// `#[doc]` attributes carried onto the glue handler for
    /// `describe_component` prose.
    docs: Vec<Attribute>,
}

/// How a glue handler dispatches back into the retained user method:
/// a `self`-receiver method call, or an associated call threading a
/// fresh split-cap state binding of the carried state type. (The glue
/// mints its own state binding rather than reusing the user's — which
/// is typically `_state` — so it never *uses* an underscore binding.)
enum CallStyle {
    SelfReceiver,
    State(Box<Type>),
}

fn expand_router(mut item: ItemImpl, shared: bool) -> syn::Result<TokenStream2> {
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new(
            item.generics.span(),
            "#[http::router] does not support generic impl blocks",
        ));
    }

    let namespace = read_namespace(&item)?;
    let self_ident = self_type_ident(&item.self_ty)?;

    // Collect routed methods, stripping the `#[http::route]` marker so
    // each survives as a plain helper `#[actor]` re-emits verbatim.
    let mut routed = Vec::new();
    for impl_item in &mut item.items {
        let ImplItem::Fn(method) = impl_item else {
            continue;
        };
        if let Some(desc) = take_routed(method, &namespace, &self_ident)? {
            routed.push(desc);
        }
    }

    if routed.is_empty() {
        return Err(syn::Error::new(
            item.span(),
            "#[http::router] found no #[http::route(...)] methods on the impl block",
        ));
    }

    let minted = routed.iter().map(emit_minted_kind).collect::<Vec<_>>();
    let glue = routed.iter().map(emit_glue_handler).collect::<Vec<_>>();
    for handler in glue {
        item.items.push(parse_quote!(#handler));
    }

    inject_registration(&mut item, &routed, shared)?;

    Ok(quote! {
        #(#minted)*
        #item
    })
}

/// Read the required `const NAMESPACE: &'static str = "…"` literal.
fn read_namespace(item: &ItemImpl) -> syn::Result<LitStr> {
    for impl_item in &item.items {
        let ImplItem::Const(konst) = impl_item else {
            continue;
        };
        if konst.ident != "NAMESPACE" {
            continue;
        }
        // A `macro_rules` `:literal` metavariable reaches a proc macro
        // wrapped in an invisible `Expr::Group`; peel it before matching.
        let Expr::Lit(ExprLit {
            lit: Lit::Str(value),
            ..
        }) = peel_group(&konst.expr)
        else {
            return Err(syn::Error::new(
                konst.expr.span(),
                "#[http::router] needs `const NAMESPACE` to be a string literal",
            ));
        };
        return Ok(value.clone());
    }
    Err(syn::Error::new(
        item.span(),
        "#[http::router] requires `const NAMESPACE: &'static str = \"…\"` on the impl block",
    ))
}

/// The last path segment of the impl's self type, used to name the
/// minted sibling structs uniquely per actor.
fn self_type_ident(ty: &Type) -> syn::Result<Ident> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return Err(syn::Error::new(
            ty.span(),
            "#[http::router] expects a named self type (e.g. `impl WasmActor for MyActor`)",
        ));
    };
    path.segments
        .last()
        .map(|seg| seg.ident.clone())
        .ok_or_else(|| syn::Error::new(ty.span(), "#[http::router] could not name the self type"))
}

/// If `method` carries `#[http::route(...)]`, strip it and build the
/// routed-method descriptor; otherwise return `None`.
fn take_routed(
    method: &mut ImplItemFn,
    namespace: &LitStr,
    self_ident: &Ident,
) -> syn::Result<Option<Routed>> {
    let route_positions: Vec<usize> = method
        .attrs
        .iter()
        .enumerate()
        .filter(|(_, attr)| attr_is_route(attr))
        .map(|(index, _)| index)
        .collect();
    let Some(&index) = route_positions.first() else {
        return Ok(None);
    };
    if route_positions.len() > 1 {
        return Err(syn::Error::new(
            method.attrs[route_positions[1]].span(),
            "a routed method takes exactly one #[http::route(...)] attribute",
        ));
    }

    let route_attr = method.attrs.remove(index);
    let args: RouteArgs = route_attr.parse_args()?;
    let method_expr = method_filter_token(&args.method)?;

    let fn_name = method.sig.ident.clone();
    let kind_struct = format_ident!("{}Route{}", self_ident, to_camel(&fn_name.to_string()));
    let kind_name = LitStr::new(
        &format!("{}.route.{fn_name}", namespace.value()),
        fn_name.span(),
    );

    let (first_arg, call_style) = parse_receiver(method)?;
    let ctx_c = parse_ctx_type(method)?;
    let extractors = parse_extractors(method)?;
    ensure_response_return(&method.sig.output)?;

    let docs = method
        .attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .cloned()
        .collect();

    Ok(Some(Routed {
        fn_name,
        method_expr,
        prefix: args.prefix,
        kind_struct,
        kind_name,
        first_arg,
        call_style,
        ctx_c,
        extractors,
        docs,
    }))
}

/// True for `#[http::route]` / `#[route]` (matched on the last path
/// segment, so it works through any import style).
fn attr_is_route(attr: &Attribute) -> bool {
    attr.path()
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "route")
}

/// Map a `#[http::route]` method identifier to its
/// `Option<HttpMethod>` filter token: `any` → `None`, a variant name →
/// `Some(HttpMethod::Variant)`.
fn method_filter_token(method: &Ident) -> syn::Result<TokenStream2> {
    if method == "any" {
        return Ok(quote! { ::core::option::Option::None });
    }
    let known = ["Get", "Post", "Put", "Delete", "Patch", "Head", "Options"];
    if known.iter().any(|name| method == name) {
        return Ok(quote! {
            ::core::option::Option::Some(::aether_capabilities::http::kinds::HttpMethod::#method)
        });
    }
    Err(syn::Error::new(
        method.span(),
        "#[http::route] method must be one of Get/Post/Put/Delete/Patch/Head/Options or `any`",
    ))
}

/// Read the method's first parameter — a `self` receiver (self-hosted
/// actor) or `state: &mut Self::State` (split native cap) — and derive
/// the glue's call style.
fn parse_receiver(method: &ImplItemFn) -> syn::Result<(FnArg, CallStyle)> {
    let first = method.sig.inputs.first().ok_or_else(|| {
        syn::Error::new(
            method.sig.span(),
            "a routed method needs a `self` receiver or a `state: &mut Self::State` parameter",
        )
    })?;
    match first {
        FnArg::Receiver(_) => Ok((first.clone(), CallStyle::SelfReceiver)),
        FnArg::Typed(PatType { pat, ty, .. }) => {
            if !matches!(pat.as_ref(), Pat::Ident(_)) {
                return Err(syn::Error::new(
                    pat.span(),
                    "the state parameter of a routed method must be a plain identifier",
                ));
            }
            Ok((first.clone(), CallStyle::State(Box::new((**ty).clone()))))
        }
    }
}

/// Extract the transport ctx type `C` from the method's second
/// parameter, which must be `http::Ctx<'_, C>`.
fn parse_ctx_type(method: &ImplItemFn) -> syn::Result<Type> {
    let ctx_arg = method.sig.inputs.iter().nth(1).ok_or_else(|| {
        syn::Error::new(
            method.sig.span(),
            "a routed method's second parameter must be `ctx: http::Ctx<'_, C>`",
        )
    })?;
    let FnArg::Typed(PatType { ty, .. }) = ctx_arg else {
        return Err(syn::Error::new(
            ctx_arg.span(),
            "a routed method's second parameter must be `ctx: http::Ctx<'_, C>`",
        ));
    };
    let Type::Path(TypePath { path, .. }) = ty.as_ref() else {
        return Err(syn::Error::new(
            ty.span(),
            "a routed method's ctx parameter must be `http::Ctx<'_, C>`",
        ));
    };
    let seg = path.segments.last().ok_or_else(|| {
        syn::Error::new(
            ty.span(),
            "a routed method's ctx parameter must be `http::Ctx<'_, C>`",
        )
    })?;
    if seg.ident != "Ctx" {
        return Err(syn::Error::new(
            ty.span(),
            "a routed method's ctx parameter must be `http::Ctx<'_, C>`",
        ));
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return Err(syn::Error::new(
            ty.span(),
            "http::Ctx needs a transport ctx type argument: `http::Ctx<'_, C>`",
        ));
    };
    args.args
        .iter()
        .rev()
        .find_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .ok_or_else(|| {
            syn::Error::new(
                ty.span(),
                "http::Ctx needs a transport ctx type argument: `http::Ctx<'_, C>`",
            )
        })
}

/// Collect the `(ident, type)` of every extractor parameter (those
/// after the receiver and ctx). Each must be a plainly-named parameter.
fn parse_extractors(method: &ImplItemFn) -> syn::Result<Vec<(Ident, Type)>> {
    let mut extractors = Vec::new();
    for arg in method.sig.inputs.iter().skip(2) {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(
                arg.span(),
                "a routed method cannot take a second `self` receiver",
            ));
        };
        let Pat::Ident(ident) = pat.as_ref() else {
            return Err(syn::Error::new(
                pat.span(),
                "an extractor parameter must be a plain identifier (`name: Extractor`)",
            ));
        };
        extractors.push((ident.ident.clone(), ty.as_ref().clone()));
    }
    Ok(extractors)
}

/// Require the routed method to return `HttpServerResponse` — the v1
/// buffered-reply contract. Streaming routes keep the raw `#[handler]`
/// surface, so a non-`HttpServerResponse` return is an error here.
fn ensure_response_return(output: &ReturnType) -> syn::Result<()> {
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new(
            output.span(),
            "a routed method must return HttpServerResponse",
        ));
    };
    let Type::Path(TypePath { path, .. }) = ty.as_ref() else {
        return Err(syn::Error::new(
            ty.span(),
            "a routed method must return HttpServerResponse",
        ));
    };
    let is_response = path
        .segments
        .last()
        .is_some_and(|seg| seg.ident == "HttpServerResponse");
    if is_response {
        Ok(())
    } else {
        Err(syn::Error::new(
            ty.span(),
            "a routed method must return HttpServerResponse (a streaming route keeps the raw #[handler] surface)",
        ))
    }
}

/// The `#[doc(hidden)]` single-field wrapper kind for one route. Its
/// wire encoding is field-concatenation, so it decodes an
/// `HttpServerRequest` payload byte-for-byte however that type grows
/// (ADR-0131) — while the ordinary derives keep ID derivation, the
/// `aether.kinds` link-sections, and the descriptor inventory.
fn emit_minted_kind(routed: &Routed) -> TokenStream2 {
    let Routed {
        kind_struct,
        kind_name,
        ..
    } = routed;
    quote! {
        #[doc(hidden)]
        #[derive(
            ::aether_data::Kind,
            ::aether_data::Schema,
            ::serde::Serialize,
            ::serde::Deserialize,
        )]
        #[kind(name = #kind_name)]
        pub struct #kind_struct {
            pub request: ::aether_capabilities::http::kinds::HttpServerRequest,
        }
    }
}

/// The `#[handler]` glue for one route: decode the minted kind, run the
/// extractors in declaration order (first `Err` becomes the reply),
/// build `http::Ctx`, and call the retained user method.
fn emit_glue_handler(routed: &Routed) -> TokenStream2 {
    let Routed {
        fn_name,
        method_expr,
        prefix,
        kind_struct,
        first_arg,
        call_style,
        ctx_c,
        extractors,
        docs,
        ..
    } = routed;

    let glue_name = format_ident!("__aether_route_{}", fn_name);
    let ext_idents = extractors
        .iter()
        .map(|(ident, _)| ident)
        .collect::<Vec<_>>();
    let ext_types = extractors.iter().map(|(_, ty)| ty);

    // The glue mints its own `__aether_state` binding for the split-cap
    // call rather than reusing the user's first parameter (typically
    // `_state`), so it never uses an underscore-prefixed binding.
    let (glue_first, call) = match call_style {
        CallStyle::SelfReceiver => (
            quote! { #first_arg },
            quote! { self.#fn_name(__aether_http_ctx #(, #ext_idents)*) },
        ),
        CallStyle::State(state_ty) => (
            quote! { __aether_state: #state_ty },
            quote! { Self::#fn_name(__aether_state, __aether_http_ctx #(, #ext_idents)*) },
        ),
    };

    quote! {
        #(#docs)*
        #[handler]
        fn #glue_name(
            #glue_first,
            __aether_ctx: &mut #ctx_c,
            __aether_mail: #kind_struct,
        ) -> ::aether_capabilities::http::kinds::HttpServerResponse {
            let __aether_request = __aether_mail.request;
            #(
                let #ext_idents = match <#ext_types as ::aether_capabilities::http::FromRequest>::from_request(
                    &__aether_request,
                ) {
                    ::core::result::Result::Ok(__aether_value) => __aether_value,
                    ::core::result::Result::Err(__aether_response) => return __aether_response,
                };
            )*
            let __aether_http_ctx = ::aether_capabilities::http::Ctx::new(
                __aether_ctx,
                __aether_request,
                ::aether_capabilities::http::Route {
                    prefix: #prefix,
                    method: #method_expr,
                },
            );
            #call
        }
    }
}

/// Build the `RegisterRouteSelf` send for one route, addressed with the
/// given `wire` ctx binding. `shared` carries the impl-level
/// `#[http::router(shared)]` opt-in (ADR-0136) straight into the wire
/// field — every route on a `shared` impl registers `shared: true`.
fn registration_send(routed: &Routed, ctx: &Ident, shared: bool) -> TokenStream2 {
    let Routed {
        method_expr,
        prefix,
        kind_struct,
        ..
    } = routed;
    quote! {
        #ctx.actor::<::aether_capabilities::http::HttpServerCapability>()
            .send(&::aether_capabilities::http::kinds::RegisterRouteSelf {
                prefix: #prefix.to_string(),
                method: #method_expr,
                kind: <#kind_struct as ::aether_data::Kind>::ID,
                shared: #shared,
            });
    }
}

/// Inject the per-route `RegisterRouteSelf` registrations into `wire` —
/// appended to an author-written `wire` body, or synthesized as a new
/// `wire` when the impl has none. Receiver and ctx shapes are copied
/// from the routed methods, so one rewrite serves both transports.
/// `shared` is the impl-level `#[http::router(shared)]` flag (ADR-0136),
/// applied uniformly to every route registration this impl emits.
fn inject_registration(item: &mut ItemImpl, routed: &[Routed], shared: bool) -> syn::Result<()> {
    let existing = item.items.iter_mut().find_map(|impl_item| match impl_item {
        ImplItem::Fn(method) if method.sig.ident == "wire" => Some(method),
        _ => None,
    });

    if let Some(wire) = existing {
        let ctx = wire_ctx_ident(wire)?;
        for route in routed {
            let send = registration_send(route, &ctx, shared);
            wire.block.stmts.push(parse_quote!(#send));
        }
        return Ok(());
    }

    // Synthesize a fresh `wire`, copying the receiver + ctx shape from
    // the first routed method (all routes on one impl share a transport).
    let template = &routed[0];
    let first_arg = &template.first_arg;
    let ctx_c = &template.ctx_c;
    let ctx = format_ident!("__aether_ctx");
    let sends = routed
        .iter()
        .map(|route| registration_send(route, &ctx, shared))
        .collect::<Vec<_>>();
    let wire: ImplItemFn = parse_quote! {
        fn wire(#first_arg, #ctx: &mut #ctx_c) {
            #(#sends)*
        }
    };
    item.items.push(ImplItem::Fn(wire));
    Ok(())
}

/// The ctx-parameter identifier of an author-written `wire`, so
/// appended registration sends address the same binding.
fn wire_ctx_ident(wire: &ImplItemFn) -> syn::Result<Ident> {
    let ctx_arg = wire.sig.inputs.iter().nth(1).ok_or_else(|| {
        syn::Error::new(
            wire.sig.span(),
            "`wire` must take a ctx parameter (`fn wire(&mut self, ctx: &mut …)`)",
        )
    })?;
    let FnArg::Typed(PatType { pat, .. }) = ctx_arg else {
        return Err(syn::Error::new(
            ctx_arg.span(),
            "`wire`'s ctx parameter must be a plainly-named `&mut` ctx",
        ));
    };
    let Pat::Ident(ident) = pat.as_ref() else {
        return Err(syn::Error::new(
            pat.span(),
            "name `wire`'s ctx parameter so route registration can address it",
        ));
    };
    Ok(ident.ident.clone())
}

/// Peel invisible `Expr::Group` wrappers a `macro_rules` `:literal` /
/// `:expr` metavariable carries when it reaches a proc macro.
fn peel_group(expr: &Expr) -> &Expr {
    let mut current = expr;
    while let Expr::Group(group) = current {
        current = &group.expr;
    }
    current
}

/// Convert a `snake_case` identifier to `CamelCase` for minting a
/// struct name (`on_users` → `OnUsers`).
fn to_camel(snake: &str) -> String {
    let mut camel = String::with_capacity(snake.len());
    let mut upper_next = true;
    for ch in snake.chars() {
        if ch == '_' {
            upper_next = true;
        } else if upper_next {
            camel.extend(ch.to_uppercase());
            upper_next = false;
        } else {
            camel.push(ch);
        }
    }
    camel
}

// The proc-macro logic here (kind minting, extraction ordering, wire
// synthesis) is exercised end-to-end through the http server's native
// route fixtures and the `RoutedHttpHandler` wasm fixture — a routed
// dispatch decoding a request under the minted kind, an extractor
// failure early-returning its 400, and registration reaching the cap
// over the wire — rather than by unit tests over token output here,
// which would only restate the `quote!` blocks.
#[cfg(test)]
mod tests {
    use super::to_camel;

    // Tripwire: minted struct names are built from this snake→camel
    // fold; a regression that mangled multi-segment names would collide
    // sibling kinds silently.
    #[test]
    fn camel_folds_snake_segments() {
        assert_eq!(to_camel("on_users"), "OnUsers");
        assert_eq!(to_camel("list"), "List");
        assert_eq!(to_camel("get_api_v2"), "GetApiV2");
    }
}
