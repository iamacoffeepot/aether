//! Proc macros for the typed route-authoring surface over the
//! `aether.http.server` capability (ADR-0131 / ADR-0154). Two attribute
//! macros, re-exported through `aether_capabilities::http` so consumers
//! write `#[http::router]` / `#[http::route]` next to the
//! `http::FromRequest` / `http::Path` / `http::Ctx` runtime types the
//! parent crate owns.
//!
//! `#[http::router]` sits on an actor's `impl` block, *above* `#[actor]`
//! (or `#[runtime]`). Attribute macros expand outer-first, so `router`
//! runs first: it consumes the `#[http::route(<Method|any>, "<template>")]`
//! attributes on methods, groups routes that share a `(static-head,
//! method)` claim, mints one hidden request-shaped route kind per group,
//! emits one `#[handler]` glue per group that matches the request's path
//! segments against every template in the group (binding `{capture}`
//! segments through `FromPathSegment` and running `FromRequest`
//! extractors), injects the `RegisterRouteSelf` registration into `wire`,
//! and hands `#[actor]` an ordinary impl block.
//!
//! A template's *static head* — its leading run of literal segments — is
//! the prefix claimed with the cap (ADR-0130 keys routes by `(prefix,
//! method)`); the capture and sub-path matching runs entirely in the
//! generated guest-side glue, so the capability never grows a routing
//! trie (ADR-0154 §1). Routes that share a `(static-head, method)` claim
//! collapse into one registration and one dispatcher, most-specific
//! template first, `404` when none match.
//!
//! Bare `#[http::router]` registers every route exclusively (today's
//! default); `#[http::router(shared)]` registers them all `shared: true`
//! instead (ADR-0136) — the impl-level opt-in for a component built to
//! run as N interchangeable instances of one round-robin member set.
//!
//! The macros only emit token paths at the runtime vocabulary
//! (`::aether_capabilities::http::…`, `::aether_data::…`, `::serde::…`);
//! they name none of those types directly, so this crate depends on
//! nothing but `syn` / `quote` / `proc-macro2`.

use std::cmp::Reverse;

use proc_macro::TokenStream;
use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::{format_ident, quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Attribute, Expr, ExprLit, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, ItemImpl, Lit, LitStr, Pat, PatType,
    PathArguments, ReturnType, Type, TypePath, parse_macro_input, parse_quote,
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

/// `#[http::reply(ReplyKind)]` — a marker attribute consumed by
/// `#[http::router]` (ADR-0154 §2): it names the method that maps a
/// deferred route's downstream reply into the response, and the router
/// generates the glue that recovers the held request and answers it.
/// Like `#[http::route]`, reaching this expansion means the enclosing
/// impl is missing `#[http::router]`.
#[proc_macro_attribute]
pub fn reply(_args: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    quote_spanned! { item.span() =>
        ::core::compile_error!(
            "#[http::reply] requires #[http::router] on the enclosing impl block (written above #[actor])"
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
/// round-robin member set).
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
    /// method half and the source of the `Option<HttpMethod>` filter token.
    method_ident: Ident,
    /// The parsed path template.
    template: Template,
    /// The method's first parameter (receiver or `state: &mut Self::State`),
    /// copied verbatim onto the glue handler.
    first_arg: FnArg,
    /// How the glue calls back into the retained method.
    call_style: CallStyle,
    /// The transport ctx type `C` from `http::Ctx<'_, C>`.
    ctx_c: Type,
    /// Each parameter after the receiver + ctx, in signature order.
    params: Vec<Param>,
    /// `true` when the method returns `http::Outcome` (deferred-capable,
    /// ADR-0154 §2); `false` for a synchronous `HttpServerResponse` route.
    /// Every route sharing a `(static-head, method)` claim must agree.
    returns_outcome: bool,
    /// `#[doc]` attributes carried onto the glue handler for
    /// `describe_component` prose.
    docs: Vec<Attribute>,
}

/// A `#[http::reply(ReplyKind)]` method (ADR-0154 §2): maps a deferred
/// route's downstream reply into the response and answers the held
/// request. It registers no route; the `#[actor]` dispatch table routes
/// its reply kind to the generated glue, which recovers the obligation.
struct ReplyRoute {
    /// The retained user method's name.
    fn_name: Ident,
    /// The reply kind this method maps — its third parameter's type, the
    /// kind the generated glue dispatches on.
    reply_kind: Type,
    /// The receiver parameter, copied onto the glue.
    first_arg: FnArg,
    /// How the glue calls back into the retained method.
    call_style: CallStyle,
    /// The transport ctx type `C` (from `ctx: &mut C`).
    ctx_c: Type,
    /// `#[doc]` attributes carried onto the glue.
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

/// A `(static-head, method)` group of routes: one cap registration, one
/// minted kind, one dispatcher glue. The receiver / ctx shape is taken
/// from the group's first route (all routes on one impl share a
/// transport), and its routes are held most-specific-first for dispatch.
struct Group<'a> {
    /// The grouping key: `(static_head, method-ident string)`.
    key: (String, String),
    /// The static head registered with the cap.
    static_head: String,
    /// The `Option<HttpMethod>` filter token for the registration and the
    /// `Route` handed to the handler.
    method_expr: TokenStream2,
    /// The group's routes, sorted most-literal-first.
    routes: Vec<&'a Routed>,
    /// Minted route-kind struct identifier (a sibling item).
    kind_struct: Ident,
    /// Minted route-kind wire name (`"{NAMESPACE}.route.{slug}_{method}"`).
    kind_name: LitStr,
    /// The generated dispatcher's name.
    glue_name: Ident,
    /// The receiver parameter copied onto the glue.
    first_arg: FnArg,
    /// The call style shared by the group's routes.
    call_style: CallStyle,
    /// The transport ctx type `C`.
    ctx_c: Type,
    /// `true` when the group's routes return `http::Outcome` — the glue
    /// is `manual`-class and may hold the reply (ADR-0154 §2). All routes
    /// in a group agree (mixed is a compile error).
    deferred: bool,
}

fn expand_router(mut item: ItemImpl, shared: bool) -> syn::Result<TokenStream2> {
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new(item.generics.span(), "#[http::router] does not support generic impl blocks"));
    }

    let namespace = read_namespace(&item)?;
    let self_ident = self_type_ident(&item.self_ty)?;

    // Collect routed + reply methods, stripping the `#[http::route]` /
    // `#[http::reply]` markers so each survives as a plain helper `#[actor]`
    // re-emits verbatim.
    let mut routed = Vec::new();
    let mut reply_routes = Vec::new();
    for impl_item in &mut item.items {
        let ImplItem::Fn(method) = impl_item else {
            continue;
        };
        if let Some(desc) = take_routed(method)? {
            routed.push(desc);
            continue;
        }
        if let Some(desc) = take_reply(method)? {
            reply_routes.push(desc);
        }
    }

    if routed.is_empty() {
        return Err(syn::Error::new(
            item.span(),
            "#[http::router] found no #[http::route(...)] methods on the impl block",
        ));
    }

    let groups = build_groups(&routed, &self_ident, &namespace)?;

    // A deferred route or a reply route needs the shared `Settled` handler
    // that answers `504` for a downstream chain that settles without a
    // reply (ADR-0154 §2).
    let needs_settled = groups.iter().any(|group| group.deferred) || !reply_routes.is_empty();

    let minted = groups.iter().map(emit_minted_kind).collect::<Vec<_>>();
    let mut glue = groups.iter().map(emit_group_glue).collect::<Vec<_>>();
    glue.extend(reply_routes.iter().map(emit_reply_glue));
    if needs_settled {
        glue.push(emit_settled_handler(&groups, &reply_routes)?);
    }
    for handler in glue {
        item.items.push(parse_quote!(#handler));
    }

    inject_registration(&mut item, &groups, shared)?;

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
        let Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) = peel_group(&konst.expr) else {
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
    let returns_outcome = parse_return_kind(&method.sig.output)?;

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

    Ok(Some(Routed {
        fn_name,
        method_ident: args.method,
        template,
        first_arg,
        call_style,
        ctx_c,
        params,
        returns_outcome,
        docs,
    }))
}

/// Group routes by `(static-head, method)`, minting one kind + glue name
/// per group and sorting each group's routes most-literal-first.
fn build_groups<'a>(routed: &'a [Routed], self_ident: &Ident, namespace: &LitStr) -> syn::Result<Vec<Group<'a>>> {
    let mut groups: Vec<Group<'a>> = Vec::new();
    for route in routed {
        let key = (route.template.static_head.clone(), route.method_ident.to_string());
        if let Some(group) = groups.iter_mut().find(|group| group.key == key) {
            // Routes sharing a claim compile into one dispatcher, so they
            // share a reply class (ADR-0154 §2): all synchronous or all
            // deferred, never mixed.
            if route.returns_outcome != group.deferred {
                return Err(syn::Error::new(
                    route.fn_name.span(),
                    "routes sharing a (prefix, method) claim must all return HttpServerResponse \
                     or all return http::Outcome — they compile into one dispatcher",
                ));
            }
            group.routes.push(route);
            continue;
        }
        let slug = slug_of(&route.template.static_head);
        let method_lower = route.method_ident.to_string().to_lowercase();
        let kind_struct = format_ident!("{}Route{}{}", self_ident, to_camel(&slug), to_camel(&method_lower));
        let kind_name = LitStr::new(&format!("{}.route.{slug}_{method_lower}", namespace.value()), self_ident.span());
        let glue_name = format_ident!("__aether_route_{slug}_{method_lower}");
        let method_expr = method_filter_token(&route.method_ident)?;
        groups.push(Group {
            key,
            static_head: route.template.static_head.clone(),
            method_expr,
            routes: vec![route],
            kind_struct,
            kind_name,
            glue_name,
            first_arg: route.first_arg.clone(),
            call_style: route.call_style.clone(),
            ctx_c: route.ctx_c.clone(),
            deferred: route.returns_outcome,
        });
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

/// Sanitize a static head into an identifier-safe slug for kind naming:
/// non-alphanumerics collapse to single underscores, edges trimmed, the
/// `/` catch-all becomes `root`.
fn slug_of(head: &str) -> String {
    let mut slug = String::with_capacity(head.len());
    for ch in head.chars() {
        if ch.is_ascii_alphanumeric() {
            slug.push(ch);
        } else if !slug.ends_with('_') {
            slug.push('_');
        }
    }
    let trimmed = slug.trim_matches('_');
    if trimmed.is_empty() {
        "root".to_string()
    } else {
        trimmed.to_string()
    }
}

/// True for `#[http::route]` / `#[route]` (matched on the last path
/// segment, so it works through any import style).
fn attr_is_route(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|seg| seg.ident == "route")
}

/// True for `#[http::reply]` / `#[reply]` (matched on the last path
/// segment, like [`attr_is_route`]).
fn attr_is_reply(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|seg| seg.ident == "reply")
}

/// If `method` carries `#[http::reply]`, strip it and build the reply-route
/// descriptor (ADR-0154 §2): a `fn(&mut self | state, ctx: &mut C, reply:
/// K) -> HttpServerResponse` that maps a deferred route's downstream reply
/// `K` into the response answered through the held obligation.
fn take_reply(method: &mut ImplItemFn) -> syn::Result<Option<ReplyRoute>> {
    let positions: Vec<usize> =
        method.attrs.iter().enumerate().filter(|(_, attr)| attr_is_reply(attr)).map(|(index, _)| index).collect();
    let Some(&index) = positions.first() else {
        return Ok(None);
    };
    if positions.len() > 1 {
        return Err(syn::Error::new(
            method.attrs[positions[1]].span(),
            "a reply method takes exactly one #[http::reply] attribute",
        ));
    }
    method.attrs.remove(index);

    let fn_name = method.sig.ident.clone();
    let (first_arg, call_style) = parse_receiver(method)?;
    let ctx_c = parse_ref_ctx_type(method)?;
    let reply_kind = parse_reply_kind(method)?;
    if parse_return_kind(&method.sig.output)? {
        return Err(syn::Error::new(
            method.sig.output.span(),
            "a #[http::reply] method must return HttpServerResponse (it maps the downstream reply into the response)",
        ));
    }
    let docs = method.attrs.iter().filter(|attr| attr.path().is_ident("doc")).cloned().collect();

    Ok(Some(ReplyRoute { fn_name, reply_kind, first_arg, call_style, ctx_c, docs }))
}

/// Extract the transport ctx type `C` from a `#[http::reply]` method's
/// second parameter, which is `ctx: &mut C` (a reply method takes the raw
/// transport ctx, not the request-shaped `Ctx`, since it serves a reply
/// rather than a request).
fn parse_ref_ctx_type(method: &ImplItemFn) -> syn::Result<Type> {
    let ctx_arg = method.sig.inputs.iter().nth(1).ok_or_else(|| {
        syn::Error::new(
            method.sig.span(),
            "a reply method's second parameter must be `ctx: &mut NativeCtx<'_, Manual>`",
        )
    })?;
    let FnArg::Typed(PatType { ty, .. }) = ctx_arg else {
        return Err(syn::Error::new(ctx_arg.span(), "a reply method's second parameter must be `ctx: &mut …`"));
    };
    let Type::Reference(reference) = ty.as_ref() else {
        return Err(syn::Error::new(ty.span(), "a reply method's ctx parameter must be a `&mut` transport ctx"));
    };
    Ok((*reference.elem).clone())
}

/// The reply kind a `#[http::reply]` method maps — its third parameter's
/// type (after the receiver and ctx).
fn parse_reply_kind(method: &ImplItemFn) -> syn::Result<Type> {
    let arg = method.sig.inputs.iter().nth(2).ok_or_else(|| {
        syn::Error::new(
            method.sig.span(),
            "a reply method needs a reply-kind parameter (`fn(&mut self, ctx, reply: K)`)",
        )
    })?;
    let FnArg::Typed(PatType { ty, .. }) = arg else {
        return Err(syn::Error::new(arg.span(), "a reply method's reply parameter must be a plainly-typed kind"));
    };
    Ok((**ty).clone())
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
        syn::Error::new(method.sig.span(), "a routed method's second parameter must be `ctx: http::Ctx<'_, C>`")
    })?;
    let FnArg::Typed(PatType { ty, .. }) = ctx_arg else {
        return Err(syn::Error::new(
            ctx_arg.span(),
            "a routed method's second parameter must be `ctx: http::Ctx<'_, C>`",
        ));
    };
    let Type::Path(TypePath { path, .. }) = ty.as_ref() else {
        return Err(syn::Error::new(ty.span(), "a routed method's ctx parameter must be `http::Ctx<'_, C>`"));
    };
    let seg = path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new(ty.span(), "a routed method's ctx parameter must be `http::Ctx<'_, C>`"))?;
    if seg.ident != "Ctx" {
        return Err(syn::Error::new(ty.span(), "a routed method's ctx parameter must be `http::Ctx<'_, C>`"));
    }
    let PathArguments::AngleBracketed(args) = &seg.arguments else {
        return Err(syn::Error::new(ty.span(), "http::Ctx needs a transport ctx type argument: `http::Ctx<'_, C>`"));
    };
    args.args
        .iter()
        .rev()
        .find_map(|arg| match arg {
            GenericArgument::Type(ty) => Some(ty.clone()),
            _ => None,
        })
        .ok_or_else(|| syn::Error::new(ty.span(), "http::Ctx needs a transport ctx type argument: `http::Ctx<'_, C>`"))
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

/// Read the routed method's return type: `HttpServerResponse` (a
/// synchronous route, the rung-1 shape) or `http::Outcome` (a deferred
/// route, ADR-0154 §2). Returns `true` for `Outcome`. Any other return is
/// an error — a streaming route keeps the raw `#[handler]` surface.
fn parse_return_kind(output: &ReturnType) -> syn::Result<bool> {
    const EXPECTED: &str = "a routed method must return HttpServerResponse or http::Outcome \
                            (a streaming route keeps the raw #[handler] surface)";
    let ReturnType::Type(_, ty) = output else {
        return Err(syn::Error::new(output.span(), EXPECTED));
    };
    let Type::Path(TypePath { path, .. }) = ty.as_ref() else {
        return Err(syn::Error::new(ty.span(), EXPECTED));
    };
    match path.segments.last() {
        Some(seg) if seg.ident == "HttpServerResponse" => Ok(false),
        Some(seg) if seg.ident == "Outcome" => Ok(true),
        _ => Err(syn::Error::new(ty.span(), EXPECTED)),
    }
}

/// The `#[doc(hidden)]` single-field wrapper kind for one route group.
/// Its wire encoding is field-concatenation, so it decodes an
/// `HttpServerRequest` payload byte-for-byte however that type grows
/// (ADR-0131) — while the ordinary derives keep ID derivation, the
/// `aether.kinds` link-sections, and the descriptor inventory.
fn emit_minted_kind(group: &Group<'_>) -> TokenStream2 {
    let Group { kind_struct, kind_name, .. } = group;
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

/// The `#[handler]` glue for one route group: decode the minted kind,
/// split the request path into segments, and try each template in the
/// group (most-specific first) — matching literals, binding captures
/// through `FromPathSegment`, running `FromRequest` extractors, and
/// calling the matched user method. A request matching no template
/// answers `404`.
fn emit_group_glue(group: &Group<'_>) -> TokenStream2 {
    let Group { routes, kind_struct, glue_name, first_arg, call_style, ctx_c, deferred, .. } = group;

    let glue_first = match call_style {
        CallStyle::SelfReceiver => quote! { #first_arg },
        CallStyle::State(state_ty) => quote! { __aether_state: #state_ty },
    };
    let docs = routes.iter().flat_map(|route| route.docs.iter());
    let arms = routes.iter().map(|route| emit_route_arm(route, group)).collect::<Vec<_>>();

    let preamble = quote! {
        let __aether_request = __aether_mail.request;
        let __aether_path = __aether_request.path.clone();
        let __aether_segs: ::std::vec::Vec<&str> =
            __aether_path.split('/').filter(|__aether_seg| !__aether_seg.is_empty()).collect();
    };
    let not_found = quote! {
        ::aether_capabilities::http::kinds::HttpServerResponse {
            status: 404,
            headers: ::std::vec::Vec::new(),
            body: ::std::vec::Vec::from(&b"no matching route"[..]),
        }
    };

    if *deferred {
        // A deferred group is `manual`-class: an arm replies inline for
        // `Outcome::Reply`, holds for `Outcome::Deferred`, and the
        // no-match fallthrough answers `404` through the taken obligation.
        quote! {
            #(#docs)*
            #[handler::manual]
            fn #glue_name(#glue_first, __aether_ctx: &mut #ctx_c, __aether_mail: #kind_struct) {
                #preamble
                #(#arms)*
                __aether_ctx.take_inbound().reply(&#not_found);
            }
        }
    } else {
        quote! {
            #(#docs)*
            #[handler::single]
            fn #glue_name(
                #glue_first,
                __aether_ctx: &mut #ctx_c,
                __aether_mail: #kind_struct,
            ) -> ::aether_capabilities::http::kinds::HttpServerResponse {
                #preamble
                #(#arms)*
                #not_found
            }
        }
    }
}

/// One route's match arm inside its group's dispatcher: a length + literal
/// guard, then capture and extractor binding, then the call.
fn emit_route_arm(route: &Routed, group: &Group<'_>) -> TokenStream2 {
    let seglen = route.template.segments.len();
    // A no-capture template keeps ADR-0130 prefix semantics — it matches
    // its head and everything under it; a capture template matches its
    // exact segment structure so it never swallows a deeper path.
    let len_check = if route.template.capture_count == 0 {
        quote! { __aether_segs.len() >= #seglen }
    } else {
        quote! { __aether_segs.len() == #seglen }
    };
    // On a bind failure (unparseable capture / rejected extractor), a
    // synchronous glue returns the response; a deferred (`manual`) glue has
    // no return value, so it replies through the taken obligation and
    // returns unit. Both use the `__aether_response` bound by the `Err` arm.
    let on_fail = if group.deferred {
        quote! { { __aether_ctx.take_inbound().reply(&__aether_response); return; } }
    } else {
        quote! { return __aether_response }
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
                let #ident = match <#ty as ::aether_capabilities::http::FromPathSegment>::from_path_segment(
                    __aether_segs[#position],
                ) {
                    ::core::result::Result::Ok(__aether_value) =>
                        ::aether_capabilities::http::Path(__aether_value),
                    ::core::result::Result::Err(__aether_response) => #on_fail,
                };
            }
        });

    let req_binds = route.params.iter().filter_map(|param| match param {
        Param::FromReq { ident, ty } => Some(quote! {
            let #ident = match <#ty as ::aether_capabilities::http::FromRequest>::from_request(
                &__aether_request,
            ) {
                ::core::result::Result::Ok(__aether_value) => __aether_value,
                ::core::result::Result::Err(__aether_response) => return __aether_response,
            };
        }),
        Param::Path { .. } => None,
    });

    let param_idents = route.params.iter().map(Param::ident).collect::<Vec<_>>();
    let fn_name = &route.fn_name;
    let invoke = match &group.call_style {
        CallStyle::SelfReceiver => quote! { self.#fn_name(__aether_http_ctx #(, #param_idents)*) },
        CallStyle::State(_) => quote! { Self::#fn_name(__aether_state, __aether_http_ctx #(, #param_idents)*) },
    };
    // A synchronous route returns the response, which the glue returns; a
    // deferred route returns `Outcome`, which the glue answers (`Reply`) or
    // holds (`Deferred`) before returning unit.
    let call_and_tail = if group.deferred {
        quote! {
            match #invoke {
                ::aether_capabilities::http::Outcome::Reply(__aether_response) => {
                    __aether_ctx.take_inbound().reply(&__aether_response);
                }
                ::aether_capabilities::http::Outcome::Deferred => {}
            }
            return;
        }
    } else {
        quote! { return #invoke; }
    };

    let static_head = LitStr::new(&group.static_head, Span::call_site());
    let method_expr = &group.method_expr;
    quote! {
        if #len_check #(#literal_checks)* {
            #(#path_binds)*
            #(#req_binds)*
            let __aether_http_ctx = ::aether_capabilities::http::Ctx::new(
                __aether_ctx,
                __aether_request,
                ::aether_capabilities::http::Route {
                    prefix: #static_head,
                    method: #method_expr,
                },
            );
            #call_and_tail
        }
    }
}

/// The `#[handler::manual]` glue for one `#[http::reply]` route (ADR-0154
/// §2): call the retained user method to map the downstream reply into a
/// response, recover the request obligation held for this reply's
/// correlation (`reply_target().correlation_id`), and answer it. An
/// unmatched reply (no held obligation — already answered, or evicted by
/// the `504` net) is a no-op.
fn emit_reply_glue(reply: &ReplyRoute) -> TokenStream2 {
    let ReplyRoute { fn_name, reply_kind, first_arg, call_style, ctx_c, docs } = reply;
    let glue_name = format_ident!("__aether_reply_{fn_name}");
    let glue_first = match call_style {
        CallStyle::SelfReceiver => quote! { #first_arg },
        CallStyle::State(state_ty) => quote! { __aether_state: #state_ty },
    };
    let call = match call_style {
        CallStyle::SelfReceiver => quote! { self.#fn_name(__aether_ctx, __aether_reply) },
        CallStyle::State(_) => quote! { Self::#fn_name(__aether_state, __aether_ctx, __aether_reply) },
    };
    quote! {
        #(#docs)*
        #[handler::manual]
        fn #glue_name(#glue_first, __aether_ctx: &mut #ctx_c, __aether_reply: #reply_kind) {
            let __aether_response = #call;
            let __aether_correlation = __aether_ctx.reply_target().correlation_id;
            if let ::core::option::Option::Some(__aether_inbound) =
                __aether_ctx.take_deferred_reply(__aether_correlation)
            {
                __aether_inbound.reply(&__aether_response);
            }
        }
    }
}

/// The shared `#[handler::manual]` `Settled` handler (ADR-0154 §2): a
/// deferred request whose downstream chain settles without ever replying
/// (a dropped or unloaded peer) is answered `504` rather than left to hang
/// the client. Its receiver / ctx shape is copied from the first deferred
/// route or reply route (all share the impl's transport). Emitted once per
/// router that has any deferred or reply route.
fn emit_settled_handler(groups: &[Group<'_>], reply_routes: &[ReplyRoute]) -> syn::Result<TokenStream2> {
    let (first_arg, call_style, ctx_c) = groups
        .iter()
        .find(|group| group.deferred)
        .map(|group| (&group.first_arg, &group.call_style, &group.ctx_c))
        .or_else(|| reply_routes.first().map(|reply| (&reply.first_arg, &reply.call_style, &reply.ctx_c)))
        .ok_or_else(|| {
            syn::Error::new(Span::call_site(), "internal: settled handler emitted with no deferred routes")
        })?;

    // The handler reads neither self nor state (the obligation table is
    // per-actor, reached through the transport ctx), so name an unused
    // receiver and silence the lint.
    let (settled_first, allow) = match call_style {
        CallStyle::SelfReceiver => (quote! { #first_arg }, quote! { #[allow(clippy::unused_self)] }),
        CallStyle::State(state_ty) => (quote! { _aether_state: #state_ty }, quote! {}),
    };

    Ok(quote! {
        #allow
        #[handler::manual]
        fn __aether_route_settled(
            #settled_first,
            __aether_ctx: &mut #ctx_c,
            __aether_settled: ::aether_capabilities::http::Settled,
        ) {
            if let ::core::option::Option::Some(__aether_inbound) =
                __aether_ctx.take_deferred_reply(__aether_settled.root.correlation_id)
            {
                __aether_inbound.reply(&::aether_capabilities::http::kinds::HttpServerResponse {
                    status: 504,
                    headers: ::std::vec::Vec::new(),
                    body: ::std::vec::Vec::from(&b"downstream settled without a reply"[..]),
                });
            }
        }
    })
}

/// Build the `RegisterRouteSelf` send for one route group, addressed with
/// the given `wire` ctx binding. `shared` carries the impl-level
/// `#[http::router(shared)]` opt-in (ADR-0136) straight into the wire
/// field — every group on a `shared` impl registers `shared: true`.
fn registration_send(group: &Group<'_>, ctx: &Ident, shared: bool) -> TokenStream2 {
    let Group { method_expr, kind_struct, .. } = group;
    let static_head = LitStr::new(&group.static_head, Span::call_site());
    quote! {
        #ctx.actor::<::aether_capabilities::http::HttpServerCapability>()
            .send(&::aether_capabilities::http::kinds::RegisterRouteSelf {
                prefix: #static_head.to_string(),
                method: #method_expr,
                kind: <#kind_struct as ::aether_data::Kind>::ID,
                shared: #shared,
            });
    }
}

/// Strip a transport ctx type down to its base by dropping any non-lifetime
/// generic arguments (the reply-class marker a deferred route carries):
/// `NativeCtx<'a, Manual>` → `NativeCtx<'a>`, `WasmCtx<'a>` → `WasmCtx<'a>`.
/// The synthesized `wire` needs the base ctx because `wire` is a
/// `Lifecycle` method with the default reply class, not the handler's.
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

/// Inject the per-group `RegisterRouteSelf` registrations into `wire` —
/// appended to an author-written `wire` body, or synthesized as a new
/// `wire` when the impl has none. Receiver and ctx shapes are copied
/// from the routed methods, so one rewrite serves both transports.
/// `shared` is the impl-level `#[http::router(shared)]` flag (ADR-0136),
/// applied uniformly to every group registration this impl emits.
fn inject_registration(item: &mut ItemImpl, groups: &[Group<'_>], shared: bool) -> syn::Result<()> {
    let existing = item.items.iter_mut().find_map(|impl_item| match impl_item {
        ImplItem::Fn(method) if method.sig.ident == "wire" => Some(method),
        _ => None,
    });

    if let Some(wire) = existing {
        let ctx = wire_ctx_ident(wire)?;
        for group in groups {
            let send = registration_send(group, &ctx, shared);
            wire.block.stmts.push(parse_quote!(#send));
        }
        return Ok(());
    }

    // Synthesize a fresh `wire`, copying the receiver + ctx shape from
    // the first group (all routes on one impl share a transport). `wire`
    // is a `Lifecycle` method with the base (default reply-class) ctx, so
    // strip any reply-class type arg a deferred route carries
    // (`NativeCtx<'_, Manual>` → `NativeCtx<'_>`).
    let template = &groups[0];
    let first_arg = &template.first_arg;
    let ctx_c = base_ctx_type(&template.ctx_c);
    let ctx = format_ident!("__aether_ctx");
    let sends = groups.iter().map(|group| registration_send(group, &ctx, shared)).collect::<Vec<_>>();
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

// The proc-macro logic here (kind minting, route grouping, segment
// matching, extraction ordering, wire synthesis) is exercised end-to-end
// through the http server's native route fixtures and the
// `RoutedHttpHandler` wasm fixture — a routed dispatch decoding a request
// under the minted kind, nested templates sharing a prefix, a path-param
// parse failure early-returning its 400, and registration reaching the
// cap over the wire — rather than by unit tests over token output here,
// which would only restate the `quote!` blocks. The pure string helpers
// carry the tripwire tests below.
#[cfg(test)]
mod tests {
    use super::{slug_of, to_camel};

    // Tripwire: minted struct names are built from this snake→camel
    // fold; a regression that mangled multi-segment names would collide
    // sibling kinds silently.
    #[test]
    fn camel_folds_snake_segments() {
        assert_eq!(to_camel("on_users"), "OnUsers");
        assert_eq!(to_camel("list"), "List");
        assert_eq!(to_camel("get_api_v2"), "GetApiV2");
    }

    // Tripwire: the group kind name / glue name derive from this slug of
    // the static head; a regression that let a non-alphanumeric or an
    // empty head through would mint an invalid identifier or collide the
    // catch-all with another group.
    #[test]
    fn slug_sanitizes_static_head() {
        assert_eq!(slug_of("/drafts"), "drafts");
        assert_eq!(slug_of("/api/v2"), "api_v2");
        assert_eq!(slug_of("/"), "root");
    }
}
