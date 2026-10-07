//! Proc macros for the typed route-authoring surface over the
//! `aether.http.server` capability. Two attributes, re-exported through
//! `aether-http` so a consumer writes `#[http::router]` and `#[http::route]`
//! beside the `http::FromRequest` / `http::Path` / `http::Ctx` runtime types
//! the parent crate owns.
//!
//! `#[http::router]` sits on an actor's `impl` block, above `#[actor]` (or
//! `#[runtime]`). Attribute macros expand outer first, so `router` runs first:
//! it consumes the `#[http::route(<Method|any>, "<template>")]` attributes on
//! the methods, groups the routes sharing a `(static-head, method)` claim,
//! emits one `#[handler::request]` over `HttpServerRequest` for the whole
//! router (the one row of the `HttpRouter` protocol every route holder
//! covers, replying `HttpRouterResult`), injects one `RegisterRouteSelf`
//! registration per group into `wire`, and hands `#[actor]` an ordinary impl
//! block.
//!
//! The generated handler picks the group the server picked: among the groups
//! whose claim matches the request, the one `route_rank` ranks highest, the
//! rule the server's route table applies to its keys. It then tries only that
//! group's templates, most specific first, matching path segments, binding
//! `{capture}` segments through `FromPathSegment`, and running `FromRequest`
//! extractors. Every answer, a route's response, a bind failure's, or the
//! `404` when no group or template matches, is an `HttpRouterResult::Response`.
//!
//! Every route returns `HttpServerResponse`, and the handler returns
//! `HttpRouterResult`, every arm its reply. A handler that forwards to a peer
//! and answers when the peer replies, or that streams, is a hand-written
//! `HttpServerRequest` handler that holds its reply (ADR-0243), not a route.
//!
//! A template's static head, its leading run of literal segments, is what is
//! claimed with the capability, which keys routes by `(prefix, method)`.
//! Capture and sub-path matching run in the generated guest-side glue, so the
//! capability never grows a routing trie (ADR-0154). Routes sharing a claim
//! collapse into one registration.
//!
//! Bare `#[http::router]` registers every route exclusively.
//! `#[http::router(shared)]` registers them all `shared: true` instead
//! (ADR-0136), the opt-in for a component built to run as N interchangeable
//! instances of one round-robin member set.
//!
//! The actor must declare `depends(HttpServerCapability)` on its `#[actor]`
//! attribute (or on the struct of a `#[runtime]`-split cap), because the
//! injected `wire` registration mails the server. A missing declaration is a
//! compile error at `#[http::router]`.
//!
//! The injected registration is the flat send
//! `ctx.send::<HttpServerCapability>(..)` (ADR-0232 §1), so the router's
//! `wire` ctx must be typed by its actor. A synthesized `wire`, or an
//! author-written one that omits its ctx actor, is typed by `Self`; an
//! author-written `wire` that spells an explicit `Erased` ctx fails to compile
//! at the generated send.
//!
//! A route's ctx that omits its actor is typed by the router's actor:
//! `http::Ctx<'_, WasmCtx<'_>>` reads as `http::Ctx<'_, WasmCtx<'_, Self>>`,
//! and the generated handler takes the same typed ctx (ADR-0231 §7). A route
//! therefore reaches its declared dependencies through the flat verbs,
//! `ctx.send::<R>(..)` and its siblings, and a route that sends to `R` needs
//! `depends(R)`. A ctx that names its actor, including an explicit erased one,
//! passes through unchanged. Every route takes the single ctx the generated
//! handler has; one that names the `Unchecked` reply mode is a compile error at
//! the method.

#![forbid(unsafe_code)]

use std::cmp::Reverse;

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{ToTokens, format_ident, quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Attribute, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, ItemImpl, LitStr, Pat, PatType, PathArguments,
    ReturnType, Type, TypePath, parse_macro_input, parse_quote, parse_quote_spanned,
};

/// `#[http::route(<Method|any>, "<template>")]` — a marker attribute
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
/// route-authoring surface (ADR-0131 / ADR-0154). Written above
/// `#[actor]`. Takes no arguments (today's exclusive registration) or the
/// bare ident `shared` (ADR-0136 joint opt-in — every route on the impl
/// registers `shared: true`, so N instances of the component join one
/// round-robin member set). The actor must declare
/// `depends(HttpServerCapability)` on its `#[actor]` attribute (or on the
/// struct of a `#[runtime]`-split cap), because the injected `wire`
/// registration mails the server; a missing declaration is a compile error
/// at `#[http::router]`.
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

/// Parsed `#[http::route(Get, "/drafts/{id}")]` arguments: an HTTP-method
/// identifier (or the bare `any`) and a path-template string literal.
struct RouteArgs {
    method: Ident,
    template: LitStr,
}

impl Parse for RouteArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let method: Ident = input.parse()?;
        let _comma: syn::Token![,] = input.parse()?;
        let template: LitStr = input.parse()?;
        Ok(Self { method, template })
    }
}

/// One parsed segment of a route template.
enum Segment {
    /// A literal path segment, matched verbatim.
    Literal(String),
    /// A `{name}` capture segment, bound positionally to a `Path<_>`
    /// parameter. The name is authoring documentation only; binding is by
    /// position, so it is not retained.
    Capture,
}

/// A parsed route path template (ADR-0154).
struct Template {
    /// The literal static head registered with the cap: the leading run
    /// of literal segments (the whole path when there are no captures),
    /// normalized to a `/`-prefixed string with no trailing slash.
    static_head: String,
    /// Every segment in declaration order, literal or capture.
    segments: Vec<Segment>,
    /// The number of `{capture}` segments — must equal the method's
    /// `Path<_>` parameter count.
    capture_count: usize,
}

impl Template {
    /// How many literal segments the template carries — the specificity
    /// key that orders a group's routes (a literal beats a capture at the
    /// same position, so more-literal templates are matched first).
    fn literal_count(&self) -> usize {
        self.segments.iter().filter(|seg| matches!(seg, Segment::Literal(_))).count()
    }

    /// The `__aether_segs` indices carrying a capture, in order — the
    /// k-th entry is where the k-th `Path<_>` parameter binds.
    fn capture_positions(&self) -> Vec<usize> {
        self.segments
            .iter()
            .enumerate()
            .filter_map(|(index, seg)| matches!(seg, Segment::Capture).then_some(index))
            .collect()
    }
}

/// Parse a `#[http::route]` template literal into a [`Template`]. A `/`
/// catch-all is the empty-segment template; a template whose first
/// segment is a capture is rejected — there is no literal head to claim
/// as the cap prefix.
fn parse_template(template: &LitStr) -> syn::Result<Template> {
    let raw = template.value();
    if !raw.starts_with('/') {
        return Err(syn::Error::new_spanned(template, format!("route template {raw:?} must start with '/'")));
    }
    let raw_segments: Vec<&str> = raw.split('/').filter(|segment| !segment.is_empty()).collect();
    if raw_segments.is_empty() {
        return Ok(Template { static_head: "/".to_string(), segments: Vec::new(), capture_count: 0 });
    }

    let mut segments = Vec::with_capacity(raw_segments.len());
    let mut head = String::new();
    let mut head_open = true;
    let mut capture_count = 0usize;
    for raw_segment in &raw_segments {
        if let Some(name) = raw_segment.strip_prefix('{').and_then(|rest| rest.strip_suffix('}')) {
            if name.is_empty() || name.contains(['{', '}']) {
                return Err(syn::Error::new_spanned(template, format!("malformed capture segment {raw_segment:?}")));
            }
            segments.push(Segment::Capture);
            capture_count += 1;
            head_open = false;
        } else {
            if raw_segment.contains(['{', '}']) {
                return Err(syn::Error::new_spanned(
                    template,
                    format!(
                        "segment {raw_segment:?} mixes a literal and a capture; a capture is a whole `{{name}}` segment"
                    ),
                ));
            }
            segments.push(Segment::Literal((*raw_segment).to_string()));
            if head_open {
                head.push('/');
                head.push_str(raw_segment);
            }
        }
    }

    if head.is_empty() {
        return Err(syn::Error::new_spanned(
            template,
            "a route template must begin with a literal segment: a capture cannot be the claimed prefix",
        ));
    }

    Ok(Template { static_head: head, segments, capture_count })
}

/// One method parameter after the receiver and ctx: a `Path<_>` capture
/// or a `FromRequest` extractor.
enum Param {
    /// `Path<T>` — bound from a captured segment through `FromPathSegment`;
    /// `ty` is the inner `T`.
    Path { ident: Ident, ty: Type },
    /// Any other type — bound from the whole request through `FromRequest`.
    FromReq { ident: Ident, ty: Type },
}

impl Param {
    fn ident(&self) -> &Ident {
        match self {
            Self::Path { ident, .. } | Self::FromReq { ident, .. } => ident,
        }
    }
}

/// Everything the emitter needs about one routed method, gathered as
/// `#[http::router]` walks the impl block.
struct Routed {
    /// The retained user method's name (also the glue's call target).
    fn_name: Ident,
    /// The HTTP-method identifier (`Get` / `any` / …) — the grouping key's
    /// method half and the source of the `MethodFilter` token.
    method_ident: Ident,
    /// The parsed path template.
    template: Template,
    /// The method's first parameter (receiver or `state: &mut Self::State`),
    /// copied verbatim onto the glue handler.
    first_arg: FnArg,
    /// How the glue calls back into the retained method.
    call_style: CallStyle,
    /// The transport ctx type `C` from `http::Ctx<'_, C>`, with its actor
    /// filled in when the author omitted it.
    ctx_c: Type,
    /// Each parameter after the receiver + ctx, in signature order.
    params: Vec<Param>,
    /// `#[doc]` attributes carried onto the glue handler for
    /// `describe_component` prose.
    docs: Vec<Attribute>,
}

/// How a glue handler dispatches back into the retained user method:
/// a `self`-receiver method call, or an associated call threading a
/// fresh split-cap state binding of the carried state type. (The glue
/// mints its own state binding rather than reusing the user's — which
/// is typically `_state` — so it never *uses* an underscore binding.)
#[derive(Clone)]
enum CallStyle {
    SelfReceiver,
    State(Box<Type>),
}

/// A `(static-head, method)` group of routes: one cap registration and one
/// arm of the router's handler, its routes held most-specific-first.
struct Group<'a> {
    /// The grouping key: `(static_head, method-ident string)`.
    key: (String, String),
    /// The static head registered with the cap.
    static_head: String,
    /// The `MethodFilter` token for the registration, the
    /// handler's group selection, and the `Route` handed to the route.
    method_expr: TokenStream2,
    /// The group's routes, sorted most-literal-first.
    routes: Vec<&'a Routed>,
}

fn expand_router(mut item: ItemImpl, shared: bool) -> syn::Result<TokenStream2> {
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new(item.generics.span(), "#[http::router] does not support generic impl blocks"));
    }

    // Collect routed methods, stripping the `#[http::route]` markers so each
    // survives as a plain helper `#[actor]` re-emits verbatim.
    let mut routed = Vec::new();
    for impl_item in &mut item.items {
        let ImplItem::Fn(method) = impl_item else {
            continue;
        };
        if let Some(desc) = take_routed(method)? {
            routed.push(desc);
        }
    }

    let Some(first) = routed.first() else {
        return Err(syn::Error::new(
            item.span(),
            "#[http::router] found no #[http::route(...)] methods on the impl block",
        ));
    };

    let groups = build_groups(&routed)?;

    let handler = emit_router_glue(&groups, first);
    item.items.push(parse_quote!(#handler));

    inject_registration(&mut item, &groups, first, shared)?;

    let depends_check = emit_depends_check(&item.self_ty);

    Ok(quote! {
        #depends_check
        #item
    })
}

/// A compile-time check that the router actor declares
/// `depends(HttpServerCapability)`: the injected `wire` registration mails the
/// server, so the declaration is required. The self type is respanned to the
/// attribute too, so a missing declaration reports E0277 on `#[http::router]`,
/// naming the helper whose name says what to write. Emits no runtime code.
fn emit_depends_check(self_ty: &Type) -> TokenStream2 {
    let self_ty = self_ty
        .to_token_stream()
        .into_iter()
        .map(|mut token| {
            token.set_span(Span::call_site());
            token
        })
        .collect::<TokenStream2>();
    quote_spanned! { Span::call_site() =>
        const _: fn() = {
            fn router_actor_must_declare_depends_http_server_capability<
                A: ::aether_actor::DependsOn<::aether_http::HttpServerCapability>,
            >() {}
            router_actor_must_declare_depends_http_server_capability::<#self_ty>
        };
    }
}

/// If `method` carries `#[http::route(...)]`, strip it and build the
/// routed-method descriptor; otherwise return `None`.
fn take_routed(method: &mut ImplItemFn) -> syn::Result<Option<Routed>> {
    let route_positions: Vec<usize> =
        method.attrs.iter().enumerate().filter(|(_, attr)| attr_is_route(attr)).map(|(index, _)| index).collect();
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
    // Validate the method identifier early; the token itself is recomputed
    // per group so all of a group's routes share one filter.
    method_filter_token(&args.method)?;
    let template = parse_template(&args.template)?;

    let fn_name = method.sig.ident.clone();
    let (first_arg, call_style) = parse_receiver(method)?;
    let ctx_c = parse_ctx_type(method)?;
    let params = classify_params(method)?;
    parse_return_kind(&method.sig.output)?;

    let path_count = params.iter().filter(|param| matches!(param, Param::Path { .. })).count();
    if path_count != template.capture_count {
        return Err(syn::Error::new(
            method.sig.span(),
            format!(
                "route template has {} path capture(s) but the method has {path_count} `Path<_>` parameter(s); \
                 they must match one-to-one, in order",
                template.capture_count,
            ),
        ));
    }

    let docs = method.attrs.iter().filter(|attr| attr.path().is_ident("doc")).cloned().collect();

    Ok(Some(Routed { fn_name, method_ident: args.method, template, first_arg, call_style, ctx_c, params, docs }))
}

/// Group routes by `(static-head, method)`, sorting each group's routes
/// most-literal-first.
fn build_groups(routed: &[Routed]) -> syn::Result<Vec<Group<'_>>> {
    let mut groups: Vec<Group<'_>> = Vec::new();
    for route in routed {
        let key = (route.template.static_head.clone(), route.method_ident.to_string());
        if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
            group.routes.push(route);
            continue;
        }
        let method_expr = method_filter_token(&route.method_ident)?;
        groups.push(Group { key, static_head: route.template.static_head.clone(), method_expr, routes: vec![route] });
    }
    for group in &mut groups {
        // Order most-specific first: an exact (capture-bearing) template
        // before the bare-head prefix template that would also match it,
        // then more-literal templates first. A no-capture template is a
        // prefix match (ADR-0130) and the loosest, so it sorts last within
        // its group.
        group.routes.sort_by_key(|route| {
            let is_prefix = route.template.capture_count == 0;
            (is_prefix, Reverse(route.template.literal_count()), Reverse(route.template.segments.len()))
        });
    }
    Ok(groups)
}

/// True for `#[http::route]` / `#[route]` (matched on the last path
/// segment, so it works through any import style).
fn attr_is_route(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|seg| seg.ident == "route")
}

/// Map a `#[http::route]` method identifier to its
/// `MethodFilter` token: `any` → `MethodFilter::Any`, a variant name →
/// `MethodFilter::Only(HttpMethod::Variant)`.
fn method_filter_token(method: &Ident) -> syn::Result<TokenStream2> {
    if method == "any" {
        return Ok(quote! { ::aether_http::kinds::MethodFilter::Any });
    }
    let known = ["Get", "Post", "Put", "Delete", "Patch", "Head", "Options"];
    if known.iter().any(|name| method == name) {
        return Ok(quote! {
            ::aether_http::kinds::MethodFilter::Only(::aether_http::kinds::HttpMethod::#method)
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
/// parameter, which must be `http::Ctx<'_, C>`. A `C` that omits its actor
/// is filled with [`fill_actor`] in the method's own signature, and the
/// filled type is returned for the glue.
fn parse_ctx_type(method: &mut ImplItemFn) -> syn::Result<Type> {
    let signature_span = method.sig.span();
    let ctx_arg = method.sig.inputs.iter_mut().nth(1).ok_or_else(|| {
        syn::Error::new(signature_span, "a routed method's second parameter must be `ctx: http::Ctx<'_, C>`")
    })?;
    let ctx_span = ctx_arg.span();
    let FnArg::Typed(PatType { ty, .. }) = ctx_arg else {
        return Err(syn::Error::new(ctx_span, "a routed method's second parameter must be `ctx: http::Ctx<'_, C>`"));
    };
    let ty_span = ty.span();
    let Type::Path(TypePath { path, .. }) = ty.as_mut() else {
        return Err(syn::Error::new(ty_span, "a routed method's ctx parameter must be `http::Ctx<'_, C>`"));
    };
    let seg = path
        .segments
        .last_mut()
        .ok_or_else(|| syn::Error::new(ty_span, "a routed method's ctx parameter must be `http::Ctx<'_, C>`"))?;
    if seg.ident != "Ctx" {
        return Err(syn::Error::new(ty_span, "a routed method's ctx parameter must be `http::Ctx<'_, C>`"));
    }
    let PathArguments::AngleBracketed(args) = &mut seg.arguments else {
        return Err(syn::Error::new(ty_span, "http::Ctx needs a transport ctx type argument: `http::Ctx<'_, C>`"));
    };
    let transport = args
        .args
        .iter_mut()
        .rev()
        .find_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty),
            _ => None,
        })
        .ok_or_else(|| syn::Error::new(ty_span, "http::Ctx needs a transport ctx type argument: `http::Ctx<'_, C>`"))?;
    reject_unchecked_ctx(transport)?;
    fill_actor(transport);
    Ok(transport.clone())
}

/// Classify every parameter after the receiver and ctx as a `Path<_>`
/// capture or a `FromRequest` extractor. Each must be a plainly-named
/// parameter.
fn classify_params(method: &ImplItemFn) -> syn::Result<Vec<Param>> {
    let mut params = Vec::new();
    for arg in method.sig.inputs.iter().skip(2) {
        let FnArg::Typed(PatType { pat, ty, .. }) = arg else {
            return Err(syn::Error::new(arg.span(), "a routed method cannot take a second `self` receiver"));
        };
        let Pat::Ident(ident) = pat.as_ref() else {
            return Err(syn::Error::new(
                pat.span(),
                "a routed method parameter must be a plain identifier (`name: Extractor`)",
            ));
        };
        let ident = ident.ident.clone();
        if let Some(inner) = path_param_inner(ty) {
            params.push(Param::Path { ident, ty: inner });
        } else {
            params.push(Param::FromReq { ident, ty: (**ty).clone() });
        }
    }
    Ok(params)
}

/// If `ty` is `Path<T>` (matched on the last path segment, so it works
/// through any import style), return the inner `T`.
fn path_param_inner(ty: &Type) -> Option<Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return None;
    };
    let seg = path.segments.last()?;
    if seg.ident != "Path" {
        return None;
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return None;
    };
    args.args.iter().find_map(|arg| match arg {
        GenericArgument::Type(inner) => Some(inner.clone()),
        _ => None,
    })
}

/// Check the routed method returns `HttpServerResponse`. Any other return is
/// an error naming the replacement: a handler that forwards to a peer or
/// streams is a hand-written `HttpServerRequest` handler that holds its reply
/// (ADR-0243).
fn parse_return_kind(output: &ReturnType) -> syn::Result<()> {
    const EXPECTED: &str = "a routed method must return HttpServerResponse; a route that forwards to a peer or \
                            streams keeps a hand-written HttpServerRequest handler (ADR-0243 held reply)";
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new(output.span(), EXPECTED));
    };
    let Type::Path(TypePath { path, .. }) = ty.as_ref() else {
        return Err(syn::Error::new(ty.span(), EXPECTED));
    };
    match path.segments.last() {
        Some(seg) if seg.ident == "HttpServerResponse" => Ok(()),
        _ => Err(syn::Error::new(ty.span(), EXPECTED)),
    }
}

/// The router's one `#[handler::request]` over `HttpServerRequest`, the row
/// of the `HttpRouter` protocol its registrations prove. It picks the group
/// whose claim the server picked, by `route_rank` over the groups' claims,
/// then tries that group's templates most-specific first: matching literals,
/// binding captures through `FromPathSegment`, running `FromRequest`
/// extractors, and calling the matched route. A request no group or template
/// matches answers `404`. Its return is `HttpRouterResult`.
fn emit_router_glue(groups: &[Group<'_>], first: &Routed) -> TokenStream2 {
    let glue_first = match &first.call_style {
        CallStyle::SelfReceiver => {
            let first_arg = &first.first_arg;
            quote! { #first_arg }
        }
        CallStyle::State(state_ty) => quote! { __aether_state: #state_ty },
    };
    let glue_ctx = &first.ctx_c;
    let not_found = quote! {
        ::aether_http::kinds::HttpRouterResult::Response(::aether_http::kinds::HttpServerResponse {
            status: 404,
            headers: ::std::vec::Vec::new(),
            body: ::std::vec::Vec::from(&b"no matching route"[..]),
        })
    };
    let docs = groups.iter().flat_map(|group| group.routes.iter()).flat_map(|route| route.docs.iter());
    let claim_count = groups.len();
    let claims = groups.iter().map(|group| {
        let static_head = LitStr::new(&group.static_head, Span::call_site());
        let method_expr = &group.method_expr;
        quote! { (#static_head, #method_expr) }
    });
    let arms = groups.iter().enumerate().map(|(index, group)| {
        let routes = group.routes.iter().map(|route| emit_route_arm(route, group, &first.call_style));
        quote! {
            if __aether_group == ::core::option::Option::Some(#index) {
                #(#routes)*
            }
        }
    });

    quote! {
        #(#docs)*
        #[handler::request]
        fn __aether_route(
            #glue_first,
            __aether_ctx: &mut #glue_ctx,
            __aether_request: ::aether_http::kinds::HttpServerRequest,
        ) -> ::aether_http::kinds::HttpRouterResult {
            let __aether_path = __aether_request.path.clone();
            let __aether_segs: ::std::vec::Vec<&str> =
                __aether_path.split('/').filter(|__aether_seg| !__aether_seg.is_empty()).collect();
            let __aether_claims: [(&str, ::aether_http::kinds::MethodFilter); #claim_count] =
                [#(#claims),*];
            let __aether_group = __aether_claims
                .iter()
                .enumerate()
                .filter_map(|(__aether_index, (__aether_prefix, __aether_filter))| {
                    ::aether_http::route_rank(
                        __aether_prefix,
                        *__aether_filter,
                        &__aether_path,
                        __aether_request.method,
                    )
                    .map(|__aether_rank| (__aether_rank, __aether_index))
                })
                .max_by_key(|(__aether_rank, _)| *__aether_rank)
                .map(|(_, __aether_index)| __aether_index);
            #(#arms)*
            #not_found
        }
    }
}

/// One route's match arm inside its group: a length + literal guard, then
/// capture and extractor binding, then the call, each answer returned as the
/// handler's reply.
fn emit_route_arm(route: &Routed, group: &Group<'_>, call_style: &CallStyle) -> TokenStream2 {
    let seglen = route.template.segments.len();
    // Every route matches its exact segment structure (#3697) — a route
    // claims its own path, not the subtree beneath it, so it never swallows a
    // deeper path (the rule capture templates already used). The cap still
    // registers the template's static head as a prefix (ADR-0130), so the cap
    // routes the whole subtree to this router; the router then answers only
    // the exact path and 404s the rest.
    let len_check = quote! { __aether_segs.len() == #seglen };
    // A bind failure (unparseable capture / rejected extractor) answers with
    // its response and ends the handler.
    let on_fail = quote! {
        {
            return ::aether_http::kinds::HttpRouterResult::Response(__aether_response);
        }
    };
    let literal_checks = route.template.segments.iter().enumerate().filter_map(|(index, seg)| match seg {
        Segment::Literal(text) => {
            let lit = LitStr::new(text, Span::call_site());
            Some(quote! { && __aether_segs[#index] == #lit })
        }
        Segment::Capture => None,
    });

    let capture_positions = route.template.capture_positions();
    let path_binds = route
        .params
        .iter()
        .filter_map(|param| match param {
            Param::Path { ident, ty } => Some((ident, ty)),
            Param::FromReq { .. } => None,
        })
        .zip(capture_positions)
        .map(|((ident, ty), position)| {
            quote! {
                let #ident = match <#ty as ::aether_http::FromPathSegment>::from_path_segment(
                    __aether_segs[#position],
                ) {
                    ::core::result::Result::Ok(__aether_value) =>
                        ::aether_http::Path(__aether_value),
                    ::core::result::Result::Err(__aether_response) => #on_fail,
                };
            }
        });

    let req_binds = route.params.iter().filter_map(|param| match param {
        Param::FromReq { ident, ty } => Some(quote! {
            let #ident = match <#ty as ::aether_http::FromRequest>::from_request(
                &__aether_request,
            ) {
                ::core::result::Result::Ok(__aether_value) => __aether_value,
                ::core::result::Result::Err(__aether_response) => #on_fail,
            };
        }),
        Param::Path { .. } => None,
    });

    let param_idents = route.params.iter().map(Param::ident).collect::<Vec<_>>();
    let fn_name = &route.fn_name;
    let invoke = match call_style {
        CallStyle::SelfReceiver => quote! { self.#fn_name(__aether_http_ctx #(, #param_idents)*) },
        CallStyle::State(_) => quote! { Self::#fn_name(__aether_state, __aether_http_ctx #(, #param_idents)*) },
    };

    let static_head = LitStr::new(&group.static_head, Span::call_site());
    let method_expr = &group.method_expr;
    quote! {
        if #len_check #(#literal_checks)* {
            #(#path_binds)*
            #(#req_binds)*
            let __aether_http_ctx = ::aether_http::Ctx::new(
                __aether_ctx,
                __aether_request,
                ::aether_http::Route {
                    prefix: #static_head,
                    method: #method_expr,
                },
            );
            return ::aether_http::kinds::HttpRouterResult::Response(#invoke);
        }
    }
}

/// Build the `RegisterRouteSelf` send for one route group, addressed with
/// the given `wire` ctx binding. `shared` carries the impl-level
/// `#[http::router(shared)]` opt-in (ADR-0136) straight into the wire
/// field — every group on a `shared` impl registers `shared: true`.
fn registration_send(group: &Group<'_>, ctx: &Ident, shared: bool) -> TokenStream2 {
    let method_expr = &group.method_expr;
    let static_head = LitStr::new(&group.static_head, Span::call_site());
    quote! {
        #ctx.send::<::aether_http::HttpServerCapability>(&::aether_http::kinds::RegisterRouteSelf {
            prefix: #static_head.to_string(),
            method: #method_expr,
            shared: #shared,
        });
    }
}

/// Type a route's transport ctx by the router's actor when
/// it omits one: `NativeCtx<'_>` becomes `NativeCtx<'_, Self>` and a bare
/// `WasmCtx` becomes `WasmCtx<'_, Self>`, inserting `Self` as the first type
/// argument after the lifetimes. The router types a route's ctx by its actor
/// as ADR-0231 §7 types a handler's, so the generated glue and the route
/// share one typed ctx and nothing erases between them. A ctx whose last
/// segment already carries a type argument (an actor, erased or not) is left
/// alone. The match is syntactic, like `#[actor]`'s own reading of a
/// handler's ctx.
fn fill_actor(ty: &mut Type) {
    let actor: GenericArgument = parse_quote_spanned!(ty.span() => Self);
    let Type::Path(TypePath { path, .. }) = ty else {
        return;
    };
    let Some(seg) = path.segments.last_mut() else {
        return;
    };
    match &mut seg.arguments {
        PathArguments::None => seg.arguments = PathArguments::AngleBracketed(parse_quote!(<'_, #actor>)),
        PathArguments::AngleBracketed(args) => {
            if args.args.iter().any(|arg| matches!(arg, GenericArgument::Type(_))) {
                return;
            }
            let position = args.args.iter().take_while(|arg| matches!(arg, GenericArgument::Lifetime(_))).count();
            args.args.insert(position, actor);
        }
        PathArguments::Parenthesized(_) => {}
    }
}

/// Refuse a route's transport ctx that names the `Unchecked` reply mode, as
/// `NativeCtx<'_, Self, Anyone, Unchecked>` does: a type argument after its
/// actor whose last segment is `Unchecked`, which covers the mode's own third
/// position and a two-argument spelling that leaves the sender out. The
/// generated handler covers the `HttpRouter` row with a single handler, so it
/// passes a single ctx, and a route answers by returning. The match is
/// syntactic, like [`fill_actor`]'s.
fn reject_unchecked_ctx(ty: &Type) -> syn::Result<()> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return Ok(());
    };
    let Some(PathArguments::AngleBracketed(args)) = path.segments.last().map(|seg| &seg.arguments) else {
        return Ok(());
    };
    let names_unchecked = args.args.iter().filter_map(type_argument_path).skip(1).any(ends_in_unchecked);
    if names_unchecked {
        return Err(syn::Error::new(
            ty.span(),
            "a route takes the single ctx the #[http::router] handler passes, not an `Unchecked` one: it answers by \
             returning, and a handler that answers later is a hand-written one that holds its reply (ADR-0243)",
        ));
    }
    Ok(())
}

/// A generic argument's path when it is a path type, and `None` for a
/// lifetime or any other argument.
fn type_argument_path(arg: &GenericArgument) -> Option<&syn::Path> {
    match arg {
        GenericArgument::Type(Type::Path(TypePath { path, .. })) => Some(path),
        _ => None,
    }
}

/// Whether a path's last segment is `Unchecked`.
fn ends_in_unchecked(path: &syn::Path) -> bool {
    path.segments.last().is_some_and(|seg| seg.ident == "Unchecked")
}

/// Whether a transport ctx type is the wasm guest's `WasmCtx`, matched on its
/// last path segment like [`fill_actor`]'s reading.
fn is_wasm_ctx(ty: &Type) -> bool {
    matches!(ty, Type::Path(TypePath { path, .. }) if path.segments.last().is_some_and(|seg| seg.ident == "WasmCtx"))
}

/// Strip a transport ctx type down to its base by dropping any non-lifetime
/// generic arguments (the actor and any reply mode):
/// `NativeCtx<'a, Self>` → `NativeCtx<'a>`, `WasmCtx<'a>` → `WasmCtx<'a>`. The synthesized `wire` needs the base ctx because `wire` is
/// a `Lifecycle` method with the default reply class, not the handler's. The
/// actor [`fill_actor`] filled in is stripped too, and `#[actor]` then types
/// the synthesized `wire` by the router's actor, as it does every `wire` whose
/// ctx omits its actor (ADR-0231 §7).
fn base_ctx_type(ty: &Type) -> Type {
    let mut ty = ty.clone();
    if let Type::Path(TypePath { path, .. }) = &mut ty
        && let Some(seg) = path.segments.last_mut()
        && let PathArguments::AngleBracketed(args) = &mut seg.arguments
    {
        args.args = args.args.iter().filter(|arg| matches!(arg, GenericArgument::Lifetime(_))).cloned().collect();
        if args.args.is_empty() {
            seg.arguments = PathArguments::None;
        }
    }
    ty
}

/// The ctx type a router-synthesized `wire` takes (ADR-0163 §3). On the
/// wasm transport `wire` receives the window-bearing `WireCtx` — the
/// `#[actor]` macro wraps the `WasmCtx` it builds into one before invoking
/// the lifecycle body — so a synthesized `wire` must name `WireCtx` to
/// unify with that call; the native transport keeps its own ctx unchanged.
/// Route registration reaches its verbs through `WireCtx`'s `Deref` to
/// `WasmCtx`, so the appended sends are unaffected.
fn synthesized_wire_ctx_type(base: Type) -> Type {
    if is_wasm_ctx(&base) {
        return parse_quote!(::aether_actor::WireCtx<'_, '_>);
    }
    base
}

/// The birth error a router-synthesized `wire` returns: each transport pins
/// its own (`Lifecycle::InitError`), read off the route's ctx as
/// [`synthesized_wire_ctx_type`] reads it.
fn wire_error_type(base: &Type) -> Type {
    if is_wasm_ctx(base) {
        return parse_quote!(::aether_actor::ActorInitError);
    }
    parse_quote!(::aether_substrate::BootError)
}

/// Inject the per-group `RegisterRouteSelf` registrations into `wire` —
/// run after an author-written `wire` body that returned `Ok`, or
/// synthesized as a new `wire` when the impl has none. Receiver and ctx
/// shapes are copied from the first routed method, so one rewrite serves
/// both transports.
///
/// `wire` returns the birth's result (ADR-0247 rule 3). An author-written
/// body is bound whole, under the return type its signature declares, so its
/// tail and its early returns keep their meaning; an `Err` from it leaves
/// before any route is registered.
/// `shared` is the impl-level `#[http::router(shared)]` flag (ADR-0136),
/// applied uniformly to every group registration this impl emits.
fn inject_registration(item: &mut ItemImpl, groups: &[Group<'_>], first: &Routed, shared: bool) -> syn::Result<()> {
    let existing = item.items.iter_mut().find_map(|impl_item| match impl_item {
        ImplItem::Fn(method) if method.sig.ident == "wire" => Some(method),
        _ => None,
    });

    if let Some(wire) = existing {
        let ctx = wire_ctx_ident(wire)?;
        // A `wire` that declares no return type is refused by `#[actor]`
        // with the signature to write, so it is left as written for that.
        let ReturnType::Type(_, output) = wire.sig.output.clone() else {
            return Ok(());
        };
        let sends = groups.iter().map(|group| registration_send(group, &ctx, shared));
        let body = &wire.block;
        wire.block = parse_quote!({
            let __aether_wired: #output = #body;
            __aether_wired?;
            #(#sends)*
            ::core::result::Result::Ok(())
        });
        return Ok(());
    }

    // Synthesize a fresh `wire`, copying the receiver + ctx shape from
    // the first route (all routes on one impl share a transport). `wire`
    // is a `Lifecycle` method with the base (default reply-class) ctx, so
    // strip the type args (`NativeCtx<'_, Self>` → `NativeCtx<'_>`).
    // `#[actor]` then types the base ctx by the router's actor.
    let first_arg = &first.first_arg;
    let base = base_ctx_type(&first.ctx_c);
    let error = wire_error_type(&base);
    let ctx_c = synthesized_wire_ctx_type(base);
    let ctx = format_ident!("__aether_ctx");
    let sends = groups.iter().map(|group| registration_send(group, &ctx, shared)).collect::<Vec<_>>();
    let wire: ImplItemFn = parse_quote! {
        fn wire(#first_arg, #ctx: &mut #ctx_c) -> ::core::result::Result<(), #error> {
            #(#sends)*
            ::core::result::Result::Ok(())
        }
    };
    item.items.push(ImplItem::Fn(wire));
    Ok(())
}

/// The ctx-parameter identifier of an author-written `wire`, so
/// appended registration sends address the same binding.
fn wire_ctx_ident(wire: &ImplItemFn) -> syn::Result<Ident> {
    let ctx_arg = wire.sig.inputs.iter().nth(1).ok_or_else(|| {
        syn::Error::new(wire.sig.span(), "`wire` must take a ctx parameter (`fn wire(&mut self, ctx: &mut …)`)")
    })?;
    let FnArg::Typed(PatType { pat, .. }) = ctx_arg else {
        return Err(syn::Error::new(ctx_arg.span(), "`wire`'s ctx parameter must be a plainly-named `&mut` ctx"));
    };
    let Pat::Ident(ident) = pat.as_ref() else {
        return Err(syn::Error::new(pat.span(), "name `wire`'s ctx parameter so route registration can address it"));
    };
    Ok(ident.ident.clone())
}

// The proc-macro logic here (route grouping, group selection, segment
// matching, extraction ordering, wire synthesis) is exercised end to end
// through the http server's native route fixtures and the
// `RoutedHttpHandler` wasm fixture — a routed dispatch decoding the request,
// nested templates sharing a prefix, nested claims picking their group, a
// path-param parse failure early-returning its 400, and registration reaching the cap over
// the wire — rather than by unit tests over token output here, which would
// only restate the `quote!` blocks.
