//! The impl-block walk: consume the markers, mint the names, group the
//! mappings, and refuse every shape the emitter could not honour.
//!
//! Nothing downstream re-checks what happens here, so this module owns the
//! whole diagnostic surface. The ordering is deliberate: structural refusals
//! (generics, expansion order, namespace) come before per-method parsing, so an
//! impl written in the wrong attribute order reports that rather than a cascade
//! of signature complaints caused by it.

pub mod attributes;
pub mod signature;

use std::collections::BTreeMap;

use proc_macro2::Span;
use quote::format_ident;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, ExprLit, Ident, ImplItem, ImplItemFn, ItemImpl, Lit, LitStr, Meta, Type, TypePath};

use crate::model::{Fallback, Host, Mapper, Mapping, ReplyGroup, Tool};
use crate::naming::{camel, snake};

use attributes::ReplyMarker;

/// The prefix every generated method carries. Authored code that used it would
/// be indistinguishable from expansion output, so the walk refuses it outright.
const GENERATED_PREFIX: &str = "__aether_model_context_protocol_";

/// The prefix `#[http::router]` gives its own generated dispatchers — the
/// signal that it has already expanded over this impl.
const HTTP_GENERATED_PREFIX: &str = "__aether_route_";

/// Everything one `#[mcp::router]` expansion needs.
pub struct Router {
    pub tools: Vec<Tool>,
    pub groups: Vec<ReplyGroup>,
    pub shared: bool,
}

/// Walk `item`, stripping the Model Context Protocol markers it consumes.
pub fn router(item: &mut ItemImpl, shared: bool) -> syn::Result<Router> {
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new(
            item.generics.span(),
            "#[mcp::router] does not support a generic impl block: the minted sibling kinds are concrete types \
             and could not be named per instantiation",
        ));
    }
    require_outermost(item)?;

    let namespace = namespace_of(item)?;
    let actor = actor_ident(&item.self_ty)?;

    let mut tools = Vec::new();
    let mut markers: Vec<(ReplyMarker, MarkerHost)> = Vec::new();
    let mut authored: Vec<Ident> = Vec::new();

    for entry in &mut item.items {
        let ImplItem::Fn(method) = entry else {
            continue;
        };
        authored.push(method.sig.ident.clone());
        if method.sig.ident.to_string().starts_with(GENERATED_PREFIX) {
            return Err(syn::Error::new(
                method.sig.ident.span(),
                format!("`{GENERATED_PREFIX}…` is reserved for #[mcp::router]'s own expansion"),
            ));
        }

        if let Some(tool) = take_tool(method, &actor, &namespace)? {
            tools.push(tool);
            continue;
        }
        markers.extend(take_reply_markers(method)?);
    }

    if tools.is_empty() {
        return Err(syn::Error::new(item.span(), "#[mcp::router] found no #[mcp::tool] methods on the impl block"));
    }
    reject_duplicate_names(&tools)?;

    let groups = group_mappings(markers)?;
    check_pairings(&tools, &groups)?;
    reject_generated_collisions(&tools, &groups, &authored)?;

    Ok(Router { tools, groups, shared })
}

/// Refuse an impl `#[http::router]` has already rewritten.
///
/// Outer-first expansion is what lets this macro consume an `#[http::reply]`
/// marker and compose one handler with it. Expanded second, it would find those
/// markers already gone and a second handler for the same reply kind already
/// emitted, so the composition would silently not happen — the failure this
/// check turns into a stated one.
fn require_outermost(item: &ItemImpl) -> syn::Result<()> {
    let expanded = item.items.iter().find_map(|entry| match entry {
        ImplItem::Fn(method) if method.sig.ident.to_string().starts_with(HTTP_GENERATED_PREFIX) => Some(&method.sig),
        _ => None,
    });
    if let Some(signature) = expanded {
        return Err(syn::Error::new(
            signature.span(),
            "#[http::router] has already expanded over this impl; write #[mcp::router] above it \
             (the required order is #[mcp::router], then #[http::router], then #[runtime])",
        ));
    }
    Ok(())
}

/// The required `const NAMESPACE: &'static str = "…"` literal.
fn namespace_of(item: &ItemImpl) -> syn::Result<LitStr> {
    let declared = item.items.iter().find_map(|entry| match entry {
        ImplItem::Const(konst) if konst.ident == "NAMESPACE" => Some(&konst.expr),
        _ => None,
    });
    let Some(expr) = declared else {
        return Err(syn::Error::new(
            item.span(),
            "#[mcp::router] requires `const NAMESPACE: &'static str = \"…\"` on the impl block: \
             every minted tool kind is named from it",
        ));
    };
    // A `macro_rules` `:literal` metavariable reaches a proc macro wrapped in an
    // invisible group; peel before matching.
    let mut peeled = expr;
    while let Expr::Group(group) = peeled {
        peeled = &group.expr;
    }
    match peeled {
        Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) => Ok(text.clone()),
        other => Err(syn::Error::new(other.span(), "#[mcp::router] needs `const NAMESPACE` to be a string literal")),
    }
}

/// The self type's final segment, which every minted sibling name carries.
fn actor_ident(self_ty: &Type) -> syn::Result<Ident> {
    match self_ty {
        Type::Path(TypePath { path, .. }) => path
            .segments
            .last()
            .map(|last| last.ident.clone())
            .ok_or_else(|| syn::Error::new(self_ty.span(), "#[mcp::router] could not name the self type")),
        other => Err(syn::Error::new(
            other.span(),
            "#[mcp::router] expects a named self type (e.g. `impl NativeActor for MyCapability`)",
        )),
    }
}

/// True for `#[mcp::tool(...)]`, matched on the attribute's final segment so
/// any import style works. No other macro in the workspace claims `tool`.
fn is_tool_marker(attribute: &Attribute) -> bool {
    attribute.path().segments.last().is_some_and(|last| last.ident == "tool")
}

/// True for `#[mcp::reply(...)]`.
///
/// `#[http::reply]` ends in the same segment, so two further signals separate
/// them: an `http` qualifier is never this macro's, and this macro's marker
/// always carries arguments while the HTTP one never does.
fn is_reply_marker(attribute: &Attribute) -> bool {
    let segments = &attribute.path().segments;
    let tail_matches = segments.last().is_some_and(|last| last.ident == "reply");
    let http_qualified = segments.len() >= 2 && segments[segments.len() - 2].ident == "http";
    tail_matches && !http_qualified && matches!(attribute.meta, Meta::List(_))
}

/// True for the `#[http::reply]` this macro composes with rather than consumes.
fn is_http_reply(attribute: &Attribute) -> bool {
    attribute.path().segments.last().is_some_and(|last| last.ident == "reply") && !is_reply_marker(attribute)
}

/// True for `#[handler::manual]`.
fn is_manual_handler(attribute: &Attribute) -> bool {
    let segments = &attribute.path().segments;
    segments.len() >= 2
        && segments[segments.len() - 2].ident == "handler"
        && segments.last().is_some_and(|last| last.ident == "manual")
}

/// Remove and return the positions matching `predicate`, rightmost first so the
/// removals do not disturb one another's indices.
fn drain_attributes(method: &mut ImplItemFn, predicate: fn(&Attribute) -> bool) -> Vec<Attribute> {
    let mut taken = Vec::new();
    let mut index = method.attrs.len();
    while index > 0 {
        index -= 1;
        if predicate(&method.attrs[index]) {
            taken.push(method.attrs.remove(index));
        }
    }
    taken.reverse();
    taken
}

/// If `method` carries `#[mcp::tool]`, strip it and describe the tool.
fn take_tool(method: &mut ImplItemFn, actor: &Ident, namespace: &LitStr) -> syn::Result<Option<Tool>> {
    let mut found = drain_attributes(method, is_tool_marker);
    let Some(marker) = found.pop() else {
        return Ok(None);
    };
    if let Some(extra) = found.pop() {
        return Err(syn::Error::new(extra.span(), "a tool method takes exactly one #[mcp::tool] attribute"));
    }

    let metadata = attributes::tool_metadata(&marker)?;
    let answer = signature::tool_answer(method)?;
    signature::require_arity(method, "#[mcp::tool] method")?;

    let stem = format!("{actor}ModelContextProtocol{}", camel(&method.sig.ident.to_string()));
    Ok(Some(Tool {
        kind_name: LitStr::new(&format!("{}.tool.{}", namespace.value(), metadata.name), metadata.name_span),
        request_struct: format_ident!("{stem}Request"),
        value_struct: format_ident!("{stem}OutputValue"),
        boundary_struct: format_ident!("{stem}BoundaryOutput"),
        dispatch_name: format_ident!("{GENERATED_PREFIX}dispatch_{}", method.sig.ident),
        host: signature::host_of(method)?,
        ctx: signature::tool_context(method)?,
        input: signature::payload(method, "#[mcp::tool] method")?,
        output: answer.output,
        deferred: answer.deferred,
        docs: method.attrs.iter().filter(|attribute| attribute.path().is_ident("doc")).cloned().collect(),
        method: method.sig.ident.clone(),
        metadata,
    }))
}

/// Which method a batch of `#[mcp::reply]` markers was written on, and what
/// that method's own role is.
struct MarkerHost {
    method: Ident,
    kind: HostKind,
}

/// The three ways an annotated method participates.
enum HostKind {
    /// The method is itself the mapping: it takes the owned reply.
    Mapping { host: Host, ctx: Type },
    /// The method keeps its `#[handler::manual]` slot; branches are injected.
    Manual { ctx_ident: Ident, reply_ident: Ident },
    /// The method was an `#[http::reply]` mapper, now retained as a helper.
    Http { host: Host, ctx: Type },
}

/// Strip `#[mcp::reply]` markers from `method` and classify the method's role.
fn take_reply_markers(method: &mut ImplItemFn) -> syn::Result<Vec<(ReplyMarker, MarkerHost)>> {
    let found = drain_attributes(method, is_reply_marker);
    if found.is_empty() {
        return Ok(Vec::new());
    }

    let markers: Vec<ReplyMarker> =
        found.iter().map(Attribute::parse_args).collect::<syn::Result<Vec<ReplyMarker>>>()?;
    let with_map = markers.iter().filter(|marker| marker.map.is_some()).count();
    if with_map != 0 && with_map != markers.len() {
        return Err(syn::Error::new(
            method.sig.ident.span(),
            "every #[mcp::reply] on one method must agree: either all name a separate `map =` helper (the method \
             keeps the handler) or none does (the method is itself the mapping)",
        ));
    }

    // Every marker on one method resolves the same way — `classify` above
    // refuses a batch that disagrees — so the role is recomputed per marker
    // rather than cloned, which keeps `HostKind` free of a `Clone` bound it
    // would only carry for this loop.
    let retained_handler = method.attrs.iter().any(is_manual_handler);
    let composes_http = !drain_attributes(method, is_http_reply).is_empty();
    let has_map = with_map != 0;
    let owner = method.sig.ident.clone();

    markers
        .into_iter()
        .map(|marker| {
            let kind = classify(method, has_map, retained_handler, composes_http)?;
            Ok((marker, MarkerHost { method: owner.clone(), kind }))
        })
        .collect()
}

/// Decide a marker-bearing method's role from the attributes around it.
fn classify(method: &ImplItemFn, has_map: bool, manual: bool, http: bool) -> syn::Result<HostKind> {
    if !has_map {
        if manual || http {
            return Err(syn::Error::new(
                method.sig.ident.span(),
                "a #[mcp::reply] without `map =` makes this method the mapping itself, so it cannot also be the \
                 retained handler; name a separate `map =` helper instead",
            ));
        }
        signature::require_result_return(method)?;
        signature::require_arity(method, "#[mcp::reply] mapping method")?;
        return Ok(HostKind::Mapping { host: signature::host_of(method)?, ctx: signature::transport_context(method)? });
    }
    if http {
        return Ok(HostKind::Http { host: signature::host_of(method)?, ctx: signature::transport_context(method)? });
    }
    if manual {
        return Ok(HostKind::Manual {
            ctx_ident: signature::binding_at(method, 1, "transport context")?,
            reply_ident: signature::binding_at(method, 2, "reply")?,
        });
    }
    Err(syn::Error::new(
        method.sig.ident.span(),
        "a #[mcp::reply] with `map =` rides above the handler that already owns this reply kind; annotate a \
         #[handler::manual] method or an #[http::reply] mapper, or drop `map =` to make this method the mapping",
    ))
}

fn reject_duplicate_names(tools: &[Tool]) -> syn::Result<()> {
    let mut claimed: BTreeMap<&str, Span> = BTreeMap::new();
    for tool in tools {
        if claimed.insert(tool.metadata.name.as_str(), tool.metadata.name_span).is_some() {
            return Err(syn::Error::new(
                tool.metadata.name_span,
                format!("tool name `{}` is declared twice on this impl", tool.metadata.name),
            ));
        }
    }
    Ok(())
}

/// Fold the markers into one group per reply kind.
fn group_mappings(markers: Vec<(ReplyMarker, MarkerHost)>) -> syn::Result<Vec<ReplyGroup>> {
    let mut groups: Vec<ReplyGroup> = Vec::new();
    let mut owners: BTreeMap<String, Ident> = BTreeMap::new();

    for (marker, role) in markers {
        let key = ReplyGroup::key(&marker.kind);
        let span = marker.kind.span();

        // Each marker states both what its branch does and what the group's
        // fallback is, so the pair is settled before the group is touched and
        // no half-built entry is ever visible.
        let (fallback, mapper) = match role.kind {
            HostKind::Mapping { host, ctx } => {
                let mapper = Mapper::Standalone { method: role.method, host: host.clone() };
                (Fallback::Vacant { host, ctx }, mapper)
            }
            HostKind::Manual { ctx_ident, reply_ident } => {
                claim_owner(&mut owners, &key, &role.method, span)?;
                (
                    Fallback::Manual { method: role.method, ctx_ident, reply_ident },
                    Mapper::Branch { method: branch_helper(&marker)? },
                )
            }
            HostKind::Http { host, ctx } => {
                claim_owner(&mut owners, &key, &role.method, span)?;
                (Fallback::Http { method: role.method, host, ctx }, Mapper::Branch { method: branch_helper(&marker)? })
            }
        };

        let index = place_group(&mut groups, &key, &marker.kind, fallback);
        let group = &mut groups[index];

        if group.mappings.iter().any(|existing| existing.tool == marker.tool) {
            return Err(syn::Error::new(
                span,
                format!("tool `{}` already has a mapping for this reply kind", marker.tool),
            ));
        }
        group.mappings.push(Mapping { tool: marker.tool, mapper, span });
    }

    Ok(groups)
}

/// The index of `key`'s group, appending one when this is its first marker.
///
/// The fallback is settled by the marker being placed, so it is applied here
/// either way: a group's fallback is whatever its most recent marker declared,
/// and `classify` has already refused a batch whose markers disagree.
fn place_group(groups: &mut Vec<ReplyGroup>, key: &str, kind: &Type, fallback: Fallback) -> usize {
    if let Some(index) = groups.iter().position(|group| ReplyGroup::key(&group.kind) == key) {
        groups[index].fallback = fallback;
        return index;
    }
    groups.push(ReplyGroup {
        handler_name: format_ident!("{GENERATED_PREFIX}reply_{}", kind_stem(kind)),
        kind: kind.clone(),
        mappings: Vec::new(),
        fallback,
    });
    groups.len() - 1
}

/// The snake-cased tail of a reply kind, which names its generated handler.
fn kind_stem(kind: &Type) -> String {
    match kind {
        Type::Path(TypePath { path, .. }) => {
            path.segments.last().map_or_else(|| "reply".to_owned(), |last| snake(&last.ident.to_string()))
        }
        _ => "reply".to_owned(),
    }
}

fn branch_helper(marker: &ReplyMarker) -> syn::Result<Ident> {
    marker.map.clone().ok_or_else(|| syn::Error::new(marker.tool.span(), "a branch mapping needs its `map =` helper"))
}

fn claim_owner(owners: &mut BTreeMap<String, Ident>, key: &str, method: &Ident, span: Span) -> syn::Result<()> {
    match owners.get(key) {
        Some(held) if held == method => Ok(()),
        Some(held) => Err(syn::Error::new(
            span,
            format!("`{held}` already owns the retained handler for this reply kind; one reply kind has one handler"),
        )),
        None => {
            owners.insert(key.to_owned(), method.clone());
            Ok(())
        }
    }
}

/// Every deferring tool needs exactly one terminal mapping, and a synchronous
/// tool needs none.
fn check_pairings(tools: &[Tool], groups: &[ReplyGroup]) -> syn::Result<()> {
    let mappings: Vec<&Mapping> = groups.iter().flat_map(|group| group.mappings.iter()).collect();

    for mapping in &mappings {
        if !tools.iter().any(|tool| tool.method == mapping.tool) {
            return Err(syn::Error::new(
                mapping.span,
                format!("#[mcp::reply] names `{}`, which is not a #[mcp::tool] method on this impl", mapping.tool),
            ));
        }
    }

    for tool in tools {
        let matched = mappings.iter().filter(|mapping| mapping.tool == tool.method).count();
        let span = tool.method.span();
        match (tool.deferred, matched) {
            (true, 1) | (false, 0) => {}
            (true, 0) => {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "tool `{}` returns mcp::Outcome, so exactly one #[mcp::reply] must map its terminal reply; \
                         a deferred call with no mapping never answers",
                        tool.metadata.name,
                    ),
                ));
            }
            (true, count) => {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "tool `{}` has {count} terminal reply mappings; a deferred call has exactly one",
                        tool.metadata.name,
                    ),
                ));
            }
            (false, _) => {
                return Err(syn::Error::new(
                    span,
                    format!(
                        "tool `{}` returns Result, so it answers inside its own dispatcher and takes no \
                         #[mcp::reply] mapping",
                        tool.metadata.name,
                    ),
                ));
            }
        }
    }
    Ok(())
}

/// Refuse a generated name that an authored item already uses, or that this
/// expansion would mint twice.
fn reject_generated_collisions(tools: &[Tool], groups: &[ReplyGroup], authored: &[Ident]) -> syn::Result<()> {
    let mut minted: BTreeMap<String, Span> = BTreeMap::new();
    let generated = tools.iter().map(|tool| &tool.dispatch_name).chain(
        groups
            .iter()
            .filter(|group| !matches!(group.fallback, Fallback::Manual { .. }))
            .map(|group| &group.handler_name),
    );

    for name in generated {
        if authored.iter().any(|existing| existing == name) {
            return Err(syn::Error::new(name.span(), format!("`{name}` collides with a method on this impl")));
        }
        if minted.insert(name.to_string(), name.span()).is_some() {
            return Err(syn::Error::new(
                name.span(),
                format!("#[mcp::router] would generate `{name}` twice; rename one of the methods it is derived from"),
            ));
        }
    }
    Ok(())
}
