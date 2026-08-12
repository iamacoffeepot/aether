use quote::quote;
use syn::{Attribute, Expr, FnArg, GenericArgument, Meta, PathArguments, ReturnType, Signature, Type};

/// The `#[cfg(...)]` attributes a handler method carries, cloned at collection
/// time so every artifact the expansion derives from that handler can be gated
/// the same way the method itself is (iamacoffeepot/aether#4811).
///
/// `syn` does not evaluate `cfg`, so a derived artifact that omits these is
/// emitted unconditionally while the compiler strips the method and the kind
/// type it names — the crate then fails to build in exactly the configuration
/// the author gated the handler out of. `#[cfg_attr]` is deliberately not
/// collected: it rewrites an item's attributes rather than deciding whether the
/// item exists, so it has no bearing on which artifacts must survive.
pub fn handler_cfgs(attrs: &[Attribute]) -> Vec<Attribute> {
    attrs.iter().filter(|attr| attr.path().is_ident("cfg")).cloned().collect()
}

pub struct HandlerFn {
    pub method: syn::ImplItemFn,
    pub kind_ty: Type,
    pub agent_doc: Option<String>,
    /// The handler method's `#[cfg]` attributes (see [`handler_cfgs`]), replayed
    /// onto its dispatch arm, marker impl, manifest record, and retention
    /// statics.
    pub cfgs: Vec<Attribute>,
    /// ADR-0109: the handler's reply contract, classified from its
    /// return type. Drives the auto-emitted `ctx.reply` and the reply
    /// kind id on the inputs manifest record.
    pub reply: HandlerReply,
    /// ADR-0112 / ADR-0134: the declared reply class (single / manual /
    /// multi). Selects the ctx view the macro passes (`as_single()` for
    /// single, the full `Manual` ctx for manual, `as_multi::<K>()` for
    /// multi) and the manifest `ReplyContract` tag.
    pub class: HandlerClass,
    /// ADR-0134: the multi-class emit kind `K`, read off the `Multi<K>` ctx
    /// marker. `Some` iff `class == Multi`; drives the `ReplyContract::Multi(K::ID)`
    /// manifest pair.
    pub multi_kind: Option<Type>,
}

pub struct FallbackFn {
    pub method: syn::ImplItemFn,
    pub agent_doc: Option<String>,
}

/// Match a handler attribute — bare `#[handler]` (any path whose last
/// segment is `handler`, so `#[crate::handler]` / `#[aether_data::handler]`
/// resolve too) or a class-marked `#[handler::single|manual|multi]`
/// (ADR-0112 / ADR-0134), whose last segment is the class and whose
/// preceding segment is `handler`. The class path never reaches attribute
/// resolution — `#[actor]` parses and strips it.
pub fn attr_is_handler(attr: &Attribute) -> bool {
    let segments = &attr.path().segments;
    let Some(last) = segments.last() else {
        return false;
    };
    if last.ident == "handler" {
        return true;
    }
    if matches!(last.ident.to_string().as_str(), "single" | "manual" | "multi") {
        let len = segments.len();
        return len >= 2 && segments[len - 2].ident == "handler";
    }
    false
}

/// Same logic for `#[fallback]`.
pub fn attr_is_fallback(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|s| s.ident == "fallback")
}

/// The category of a `#[handler]` method (ADR-0093 §3). Bare `#[handler]`
/// and `#[handler(mail)]` both select the inbound-mail variant, which also
/// requires an explicit reply class (ADR-0134, [`HandlerClass`]);
/// `#[handler(task)]` marks a hold-until-resolve dispatch completion,
/// matched by its `TaskDone<O, C>` output type rather than a kind id.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HandlerVariant {
    Mail,
    Task,
}

/// Parse the parenthesized argument of a `#[handler(...)]` attribute into
/// a [`HandlerVariant`]. Bare `#[handler]` (no parens) is `Mail`. The
/// only accepted parenthesized spellings are `mail` and `task`; anything
/// else is a pointed compile error spanned at the attribute.
pub fn parse_handler_variant(attr: &Attribute) -> syn::Result<HandlerVariant> {
    match &attr.meta {
        // Bare `#[handler]` — the default inbound-mail handler.
        Meta::Path(_) => Ok(HandlerVariant::Mail),
        // `#[handler(mail)]` / `#[handler(task)]` — parse the single
        // ident argument.
        Meta::List(_) => {
            let ident: syn::Ident = attr.parse_args().map_err(|_| {
                syn::Error::new_spanned(
                    attr,
                    "#[handler(...)] accepts exactly `mail` or `task` — \
                     `mail` (bare `#[handler::<class>]` or `#[handler::<class>(mail)]`) \
                     selects the inbound-mail variant, `task` (`#[handler(task)]`) is a \
                     dispatch completion (ADR-0093 §3)",
                )
            })?;
            if ident == "mail" {
                Ok(HandlerVariant::Mail)
            } else if ident == "task" {
                Ok(HandlerVariant::Task)
            } else {
                Err(syn::Error::new_spanned(
                    &ident,
                    "unknown #[handler] variant — accepts exactly `mail` or `task` \
                     (`#[handler::<class>]` / `#[handler::<class>(mail)]` = inbound mail, \
                     `#[handler(task)]` = a dispatch completion, ADR-0093 §3)",
                ))
            }
        }
        Meta::NameValue(nv) => Err(syn::Error::new_spanned(
            nv,
            "#[handler] takes no `= value` — write `#[handler::single]`, \
             `#[handler::manual]`, `#[handler::multi]`, or `#[handler(task)]`",
        )),
    }
}

/// The reply class of a handler (ADR-0112, ADR-0134), read off the
/// attribute path: `#[handler::single]` is [`Single`](HandlerClass::Single),
/// `#[handler::manual]` is [`Manual`](HandlerClass::Manual), and
/// `#[handler::multi]` is [`Multi`](HandlerClass::Multi) — every mail
/// handler names its class explicitly. Orthogonal to [`HandlerVariant`]
/// (the `mail` / `task` trigger), which is read from the parens.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum HandlerClass {
    Single,
    Manual,
    Multi,
}

/// Read a handler's [`HandlerClass`] off its attribute path (ADR-0112,
/// ADR-0134), given the already-parsed [`HandlerVariant`]. The last path
/// segment is the class (`single` / `manual` / `multi`); a bare `handler`
/// segment is classless task exemption for [`HandlerVariant::Task`] (its
/// reply rides `TaskDone`, not the handler class) and a pointed compile
/// error for [`HandlerVariant::Mail`] — the class is no longer defaulted.
/// `attr_is_handler` is the gate, so the path is known to end in one of
/// these segments.
pub fn parse_handler_class(attr: &Attribute, variant: HandlerVariant) -> syn::Result<HandlerClass> {
    let last = attr.path().segments.last().expect("attr_is_handler guarantees a non-empty path");
    let class = match last.ident.to_string().as_str() {
        "handler" => match variant {
            // The task variant has no reply class — its reply rides
            // `TaskDone`, not the handler class — so it stays classless.
            HandlerVariant::Task => HandlerClass::Single,
            HandlerVariant::Mail => {
                return Err(syn::Error::new_spanned(
                    attr,
                    "#[handler] requires an explicit reply class (ADR-0134): write \
                     `#[handler::single]` (the return value is the reply), \
                     `#[handler::manual]` (the handler issues replies), or \
                     `#[handler::multi]` (detached emissions of a declared kind)",
                ));
            }
        },
        "single" => HandlerClass::Single,
        "manual" => HandlerClass::Manual,
        "multi" => HandlerClass::Multi,
        other => {
            return Err(syn::Error::new_spanned(
                attr,
                format!(
                    "unknown #[handler::<class>] — accepts `single`, `manual`, or `multi` \
                     (ADR-0112 / ADR-0134); got `{other}`"
                ),
            ));
        }
    };
    Ok(class)
}

/// The ctx parameter's angle-bracketed **type** arguments in declaration
/// order, lifetimes skipped: `[]` for `NativeCtx<'_>`, `[Manual]` for
/// `NativeCtx<'_, Manual>`, `[Manual, Self]` for `NativeCtx<'_, Manual, Self>`.
/// `None` when the second parameter is not a reference to a path type at all —
/// each caller phrases that failure in its own vocabulary.
fn ctx_type_args(sig: &Signature) -> Option<Vec<&Type>> {
    let FnArg::Typed(pt) = sig.inputs.get(1)? else {
        return None;
    };
    // Peel the leading `&mut` / `&` off the ctx reference, then read the
    // last path segment — the ctx type itself (`WasmCtx` / `NativeCtx`).
    let Type::Reference(ctx_ref) = &*pt.ty else {
        return None;
    };
    let Type::Path(ctx_path) = &*ctx_ref.elem else {
        return None;
    };
    let PathArguments::AngleBracketed(args) = &ctx_path.path.segments.last()?.arguments else {
        return Some(Vec::new());
    };
    Some(
        args.args
            .iter()
            .filter_map(|a| match a {
                GenericArgument::Type(t) => Some(t),
                _ => None,
            })
            .collect(),
    )
}

/// Issue 4158: whether a native handler's ctx signature names the actor it
/// dispatches for — the *second* type argument, as in
/// `NativeCtx<'_, Manual, Self>`. Only a handler that asks receives the typed
/// ctx `spawn_child` lives on; every other arm is handed an `erase()`d view,
/// so a spawn cannot name a parent other than the actor being dispatched.
pub fn ctx_names_actor(sig: &Signature) -> bool {
    ctx_type_args(sig).is_some_and(|args| args.len() >= 2)
}

/// Extract the element kind `K` from a `#[handler::multi]` method's ctx
/// parameter (ADR-0134). The ctx is the second parameter and must be
/// `ctx: &mut WasmCtx<'_, Multi<K>>` (wasm) or
/// `ctx: &mut NativeCtx<'_, Multi<K>>` (native): the macro reads `K` off
/// the `Multi<K>` marker so the manifest's `ReplyContract::Multi(K::ID)`
/// and the `emit` element kind cannot drift. A ctx that lacks the
/// `Multi<K>` marker earns a pointed error naming the required shape
/// rather than an opaque unification failure at the generated call site.
fn extract_multi_emit_kind(sig: &Signature) -> syn::Result<Type> {
    // Nested (non-capturing) so every `Multi<K>`-shape failure earns one
    // message, spanned at whichever token the parse got stuck on.
    fn shape_err<T: quote::ToTokens>(span: T) -> syn::Error {
        syn::Error::new_spanned(
            span,
            "#[handler::multi] requires a `Multi<K>` ctx marker naming the emit kind — \
             write `ctx: &mut WasmCtx<'_, Multi<K>>` (or `NativeCtx<'_, Multi<K>>`), \
             where `K` is the kind the handler emits (ADR-0134)",
        )
    }
    let ctx_param = sig.inputs.get(1).ok_or_else(|| shape_err(sig))?;
    // Span every shape failure below on the ctx *type* rather than the whole
    // parameter — the type is what the author has to rewrite.
    let FnArg::Typed(ctx_pat) = ctx_param else {
        return Err(shape_err(ctx_param));
    };
    let ctx_ty = &*ctx_pat.ty;
    // The reply mode is the ctx's *first* type argument; an optional second
    // one names the actor (`NativeCtx<'_, Multi<K>, Self>`, issue 4158), so
    // reading positionally is what keeps the two apart.
    let marker_ty = *ctx_type_args(sig).ok_or_else(|| shape_err(ctx_ty))?.first().ok_or_else(|| shape_err(ctx_ty))?;
    let Type::Path(marker_path) = marker_ty else {
        return Err(shape_err(marker_ty));
    };
    let marker_seg = marker_path.path.segments.last().ok_or_else(|| shape_err(marker_ty))?;
    if marker_seg.ident != "Multi" {
        return Err(shape_err(marker_ty));
    }
    let PathArguments::AngleBracketed(marker_args) = &marker_seg.arguments else {
        return Err(shape_err(marker_ty));
    };
    marker_args
        .args
        .iter()
        .find_map(|a| match a {
            GenericArgument::Type(t) => Some(t.clone()),
            _ => None,
        })
        .ok_or_else(|| shape_err(marker_ty))
}

/// Resolve a mail handler's [`HandlerClass::Multi`] emit kind, enforcing
/// its `-> ()` return contract (ADR-0134). A non-multi class carries no
/// emit kind (`Ok(None)`). A multi handler must return `()` — its 0..n
/// `ctx.emit` calls *are* the reply, so a return value is a contradiction
/// — and its `K` is read off the `Multi<K>` ctx marker. Shared by the wasm
/// and native collection sites so the enforcement can't drift between them.
pub fn multi_kind_or_return_error(
    class: HandlerClass,
    reply: &HandlerReply,
    sig: &Signature,
) -> syn::Result<Option<Type>> {
    if class != HandlerClass::Multi {
        return Ok(None);
    }
    if !matches!(reply, HandlerReply::None) {
        return Err(syn::Error::new_spanned(
            &sig.output,
            "#[handler::multi] must return `()` — a multi handler answers with 0..n \
             `ctx.emit` calls, so a return value has no reply path (ADR-0134)",
        ));
    }
    Ok(Some(extract_multi_emit_kind(sig)?))
}

/// Extract `(O, C, is_borrow)` from a `#[handler(task)]` method's third
/// parameter, which must be `done: TaskDone<O>` (where `C` defaults to
/// `()`) or `done: TaskDone<O, C>`, optionally behind a shared `&`.
/// Unlike a mail handler's third parameter (a `Kind`), a task
/// completion's parameter is the framework's `TaskDone<...>` — `O` / `C`
/// are its generic arguments, not a kind. `is_borrow` is `true` when the
/// parameter is `&TaskDone<…>` (the ADR-0109 opt-in for a macro-driven
/// reply) versus the by-value `TaskDone<…>` self-resolve form.
pub fn extract_task_handler_types(sig: &Signature, is_split: bool) -> syn::Result<(Type, Type, bool)> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[handler(task)] method must have signature \
             `(&self | &mut self, ctx: &mut NativeCtx<'_>, done: TaskDone<O>)` \
             (or `TaskDone<O, C>` with an opt-in context)",
        ));
    }
    let first = &sig.inputs[0];
    if is_split {
        // iamacoffeepot/aether#2341: a split `#[actor]` (`type State = …`) is on
        // the identity, so a `#[handler(task)]` takes the runtime state
        // explicitly — `(state: &mut Self::State, ctx: &mut NativeCtx<'_>, done:
        // TaskDone<O>)` — rather than a `self` receiver, mirroring the split
        // `#[handler]` / `#[fallback]` shapes. `rewrite_self_state_first_param`
        // is already applied to `task_handlers` and the task-completion dispatch
        // arm already passes `__aether_state`; this validator was the last gap.
        if !matches!(first, FnArg::Typed(_)) {
            return Err(syn::Error::new_spanned(
                first,
                "a split `#[actor]` (`type State = …`) #[handler(task)]'s first parameter \
                 must be `state: &mut Self::State` (the runtime state), not a `self` receiver",
            ));
        }
    } else if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(first, "#[handler(task)] first parameter must be `&self` or `&mut self`"));
    }
    let third = &sig.inputs[2];
    let FnArg::Typed(pt) = third else {
        return Err(syn::Error::new_spanned(
            third,
            "#[handler(task)] third parameter must be `done: TaskDone<O>` or `TaskDone<O, C>`",
        ));
    };
    // ADR-0109: `&TaskDone<…>` (the macro-driven reply opt-in) vs the
    // by-value `TaskDone<…>` self-resolve form. Peel a leading shared
    // reference and remember which shape it was.
    let (is_borrow, inner_ty): (bool, &Type) = match &*pt.ty {
        Type::Reference(r) => (true, &*r.elem),
        other => (false, other),
    };
    let Type::Path(type_path) = inner_ty else {
        return Err(syn::Error::new_spanned(
            &pt.ty,
            "#[handler(task)] third parameter must be a `TaskDone<O>` / `TaskDone<O, C>` path type \
             (optionally behind `&`)",
        ));
    };
    let last = type_path
        .path
        .segments
        .last()
        .ok_or_else(|| syn::Error::new_spanned(&pt.ty, "#[handler(task)] third parameter must be `TaskDone<…>`"))?;
    if last.ident != "TaskDone" {
        return Err(syn::Error::new_spanned(
            &pt.ty,
            "#[handler(task)] third parameter must be `TaskDone<O>` or `TaskDone<O, C>` \
             (the framework completion type, ADR-0093 §3)",
        ));
    }
    let PathArguments::AngleBracketed(args) = &last.arguments else {
        return Err(syn::Error::new_spanned(
            last,
            "#[handler(task)] `TaskDone` needs an output type argument: `TaskDone<O>` or \
             `TaskDone<O, C>`",
        ));
    };
    let type_args: Vec<&Type> = args
        .args
        .iter()
        .filter_map(|a| match a {
            GenericArgument::Type(t) => Some(t),
            _ => None,
        })
        .collect();
    let output = match type_args.first() {
        Some(t) => (*t).clone(),
        None => {
            return Err(syn::Error::new_spanned(
                last,
                "#[handler(task)] `TaskDone` needs an output type argument: `TaskDone<O>`",
            ));
        }
    };
    // `C` defaults to `()` (a bare `TaskDone<O>` / `dispatch_blocking`).
    let context = type_args.get(1).map_or_else(|| syn::parse_quote!(()), |t| (*t).clone());
    if type_args.len() > 2 {
        return Err(syn::Error::new_spanned(
            last,
            "#[handler(task)] `TaskDone` takes at most two type arguments: `TaskDone<O, C>`",
        ));
    }
    Ok((output, context, is_borrow))
}

/// How a `#[handler(task)]` completion discharges its reply (ADR-0109),
/// classified from its third-parameter borrow-ness plus its return type.
/// The `&TaskDone` borrow is the opt-in signal for a macro-driven reply.
pub enum TaskReplyMode {
    /// `TaskDone<O, C>` by value, `-> ()`: the handler owns the
    /// completion and calls `done.resolve*` itself (the ADR-0093 path,
    /// untouched).
    ByValue,
    /// `&TaskDone<O, C>` returning `-> R`: the handler borrows the
    /// completion and returns the reply; the macro calls
    /// `done.resolve_value(ctx, &r)` and releases the hold.
    BorrowReply,
    /// `&TaskDone<O, C>` returning `-> ()`: the handler borrows the
    /// completion and replies nothing; the macro calls
    /// `done.release_no_reply()` (the sanctioned no-reply discharge).
    BorrowNoReply,
}

/// Classify a `#[handler(task)]` completion's reply discharge from its
/// third-parameter borrow-ness (`is_borrow`) and return type (ADR-0109).
/// A by-value `TaskDone` keeps the self-resolve path and must return
/// `()`; `&TaskDone -> R` sends `R` via `resolve_value`, `&TaskDone -> ()`
/// releases via `release_no_reply`. A task completion can't itself defer
/// (`-> Pending<R>`).
pub fn classify_task_reply_mode(sig: &Signature, is_borrow: bool) -> syn::Result<TaskReplyMode> {
    let reply = classify_handler_reply(&sig.output);
    if is_borrow {
        match reply {
            HandlerReply::Sync(_) => Ok(TaskReplyMode::BorrowReply),
            HandlerReply::None => Ok(TaskReplyMode::BorrowNoReply),
            HandlerReply::Deferred(_) => Err(syn::Error::new_spanned(
                &sig.output,
                "#[handler(task)] cannot return `Pending<R>` — a dispatch completion \
                 is the terminal reply. Return `R` (the macro sends it via \
                 `resolve_value`) or `()` (release without replying)",
            )),
        }
    } else {
        match reply {
            HandlerReply::None => Ok(TaskReplyMode::ByValue),
            HandlerReply::Sync(_) | HandlerReply::Deferred(_) => Err(syn::Error::new_spanned(
                &sig.output,
                "a by-value `TaskDone<…>` #[handler(task)] must return `()` and call \
                 `done.resolve*` itself; to have the macro send the reply, borrow it: \
                 `done: &TaskDone<…>` returning `-> R`",
            )),
        }
    }
}

pub struct NativeActorHandlerFn {
    pub method: syn::ImplItemFn,
    pub kind_ty: Type,
    /// `true` when the handler's `mail` parameter is `&[K]` rather
    /// than `K`. The dispatcher decodes via `decode_cast_slice` so a
    /// single envelope with `count > 1` reaches the handler intact.
    pub is_slice: bool,
    /// ADR-0109: the handler's reply contract, classified from its
    /// return type. A `-> R` native handler auto-replies `R` through
    /// `OutboundReply::reply`, the same path a manual `ctx.reply` takes.
    pub reply: HandlerReply,
    /// ADR-0112 / ADR-0134: the declared reply class (single / manual /
    /// multi). Selects the ctx view the dispatch arm passes and the
    /// manifest reply tag. The multi emit kind `K` is not stored — the
    /// native dispatch arm reads it off the handler's `Multi<K>` ctx
    /// signature by inference, and the native reply manifest is the
    /// inventory `HandlerEntry`, not the wasm `ReplyContract` record.
    pub class: HandlerClass,
    /// The handler method's `#[cfg]` attributes (see [`handler_cfgs`]), replayed
    /// onto its dispatch arm, capability entry, measured-kind id, marker impl,
    /// and inventory submission.
    pub cfgs: Vec<Attribute>,
}

/// A `#[handler(task)]` completion handler (ADR-0093 §3). Its third
/// parameter is `done: TaskDone<O, C>` (C defaults to `()`); `output_ty`
/// / `context_ty` are the extracted `O` / `C`. Routed not by a kind id
/// but by output type, via a non-consuming `try_take_task_done::<O, C>`
/// probe in the single `TaskCompletionWake` dispatch arm.
pub struct NativeActorTaskHandlerFn {
    pub method: syn::ImplItemFn,
    pub output_ty: Type,
    pub context_ty: Type,
    /// ADR-0109: how the completion discharges its reply — self-resolve
    /// (by-value), macro-driven `resolve_value` (`&TaskDone -> R`), or
    /// `release_no_reply` (`&TaskDone -> ()`).
    pub mode: TaskReplyMode,
}

/// Token-level type equality, used to reject duplicate `TaskDone<O>`
/// output types across `#[handler(task)]` methods. `syn::Type` is not
/// `PartialEq`, so compare the pretty-printed token streams — exact
/// enough for the duplicate-`O` ambiguity check (two spellings of the
/// same type that tokenize differently are a corner case the author can
/// resolve by normalising).
pub fn types_token_eq(a: &Type, b: &Type) -> bool {
    quote!(#a).to_string() == quote!(#b).to_string()
}

/// Shared helper: reject duplicate `#[handler]` kinds across both the wasm
/// and native expanders. Both expander handler types share a `kind_ty: Type`
/// and `method: syn::ImplItemFn` field; this trait abstracts over them so one
/// dedup loop serves both paths.
pub trait HasKindTy {
    fn kind_ty(&self) -> &Type;
    fn method_ident(&self) -> &syn::Ident;
}

impl HasKindTy for HandlerFn {
    fn kind_ty(&self) -> &Type {
        &self.kind_ty
    }
    fn method_ident(&self) -> &syn::Ident {
        &self.method.sig.ident
    }
}

impl HasKindTy for NativeActorHandlerFn {
    fn kind_ty(&self) -> &Type {
        &self.kind_ty
    }
    fn method_ident(&self) -> &syn::Ident {
        &self.method.sig.ident
    }
}

/// Reject duplicate handler kinds in an `#[actor]` impl block. Two
/// `#[handler]` methods that accept the same mail kind would emit two
/// `HandlesKind<K>` impls (a coherence error) plus a dead second dispatch arm
/// the first arm always shadows. The macro has no type resolution, so dedup is
/// by token equality (`types_token_eq`), not by resolved `KindId`.
pub fn reject_duplicate_handler_kinds<H: HasKindTy>(handlers: &[H]) -> syn::Result<()> {
    for (i, later) in handlers.iter().enumerate() {
        if let Some(earlier) = handlers[..i].iter().find(|earlier| types_token_eq(earlier.kind_ty(), later.kind_ty())) {
            let earlier_name = earlier.method_ident();
            let kind_ty = later.kind_ty();
            return Err(syn::Error::new_spanned(
                later.method_ident(),
                format!(
                    "two #[handler] methods accept the same mail kind `{}` (also on \
                     `{earlier_name}`) — each kind routes to exactly one handler. Give each \
                     handler a distinct kind.",
                    quote!(#kind_ty)
                ),
            ));
        }
    }
    Ok(())
}

/// Validate the `NAMESPACE` / const surface inside an `#[actor] impl <Trait>
/// for X` block. Returns a reference to the `NAMESPACE` const's value
/// expression (used by the native expander to wire `impl Addressable`; the
/// wasm expander discards it and re-emits `consts` as-is). Errors are spanned
/// at the offending const or at `self_ty` when `NAMESPACE` is absent.
pub fn validate_addressable_consts<'a>(
    consts: &'a [syn::ImplItemConst],
    self_ty: &Type,
    trait_name: &str,
) -> syn::Result<&'a Expr> {
    let mut has_namespace = false;
    for c in consts {
        if c.ident == "NAMESPACE" {
            has_namespace = true;
        } else if c.ident == "SCHEDULING" {
            return Err(syn::Error::new_spanned(
                c,
                "`SCHEDULING` was removed (issue 1187): every actor drains on the chassis \
                 worker pool. Drop the const — never block a handler; offload blocking work \
                 to a `ctx.spawn`'d thread that feeds results back as mail.",
            ));
        } else {
            return Err(syn::Error::new_spanned(
                c,
                format!(
                    "#[actor] impl {trait_name} for X accepts only \
                     `const NAMESPACE: &'static str = …` — the `Addressable` super-trait carries no \
                     other authorable const"
                ),
            ));
        }
    }
    if !has_namespace {
        return Err(syn::Error::new_spanned(
            self_ty,
            format!(
                "#[actor] impl {trait_name} for X must declare \
                 `const NAMESPACE: &'static str = ...` so the marker `impl Addressable` can carry it"
            ),
        ));
    }
    consts
        .iter()
        .find_map(|c| (c.ident == "NAMESPACE").then_some(&c.expr))
        .ok_or_else(|| syn::Error::new_spanned(self_ty, "internal: NAMESPACE confirmed above but not found"))
}

/// Rename `wire` → `__aether_wire` and `unwire` → `__aether_unwire` in the
/// given method slice, pushing `#[allow(clippy::unused_self)]` onto each
/// renamed method. Returns `(has_wire, has_unwire)`.
///
/// The safe `else { continue; }` form is used so the helper is correct over a
/// mixed-content slice (e.g. the native expander's full `lifecycle_methods`)
/// as well as a pre-partitioned slice (the wasm expander's `boot_hooks`, which
/// by construction contains only `wire`/`unwire` methods — the `else { continue;
/// }` branch is never reached there, preserving the existing output exactly).
pub fn rename_lifecycle_hooks(methods: &mut [syn::ImplItemFn]) -> (bool, bool) {
    let mut has_wire = false;
    let mut has_unwire = false;
    for m in methods {
        if m.sig.ident == "wire" {
            has_wire = true;
            m.sig.ident = syn::Ident::new("__aether_wire", m.sig.ident.span());
        } else if m.sig.ident == "unwire" {
            has_unwire = true;
            m.sig.ident = syn::Ident::new("__aether_unwire", m.sig.ident.span());
        } else {
            continue;
        }
        // iamacoffeepot/aether#2311: the renamed hook keeps its `&mut self`
        // receiver (the forwarding `Lifecycle<S>` fn passes `&mut S` in as
        // `self`), so a stateless `wire`/`unwire` body trips
        // `clippy::unused_self` on the now-inherent method — the receiver is
        // the required ABI, so suppress it on the generated copy.
        m.attrs.push(syn::parse_quote!(#[allow(clippy::unused_self)]));
    }
    (has_wire, has_unwire)
}

/// Issue 576: native-side `#[fallback]` collected on a
/// `#[actor] impl NativeActor for X` block. Mirrors the wasm-side
/// [`FallbackFn`] but the native handler signature pivots on
/// `aether_substrate::actor::native::envelope::Envelope` — it carries
/// the kind id, kind name, origin, sender, and payload in one borrow so
/// catch-all caps (broadcast, future hub-as-actor) can lift the whole
/// envelope into a downstream call without rebuilding fields the
/// trampoline already has.
pub struct NativeFallbackFn {
    pub method: syn::ImplItemFn,
}

/// Validate a native `#[fallback]` method signature. Required shape:
/// `(&self | &mut self, ctx: &mut NativeCtx<'_>, env: &Envelope)`.
/// The third argument's exact type isn't checked here — the
/// synthesized override calls `self.<fallback>(ctx, env)` and the
/// user's fn body will type-error against `&Envelope` if they wrote
/// the wrong parameter type.
///
/// Issue 629 / Phase B: `&mut self` is now allowed alongside `&self`.
/// The dispatcher owns the cap as `Box<A>` and calls the fallback
/// through `&mut Box<A>`, so either receiver shape works.
pub fn validate_native_fallback_sig(sig: &Signature, is_split: bool) -> syn::Result<()> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[fallback] on `impl NativeActor for X` must have signature \
             `(&self | &mut self, ctx: &mut NativeCtx<'_>, env: &Envelope)`",
        ));
    }
    let first = &sig.inputs[0];
    if is_split {
        // iamacoffeepot/aether#2338: a split `#[actor]` (`type State = …`) is on
        // the identity, so the `#[fallback]` takes the runtime state explicitly —
        // `(state: &mut Self::State, ctx: &mut NativeCtx<'_>, env: &Envelope)` —
        // rather than a `self` receiver, mirroring the split `#[handler]` shape.
        // `dispatch_fallback` already passes `__aether_state` and
        // `rewrite_self_state_first_param` already rewrites the first param; this
        // validator was the only gap.
        if !matches!(first, FnArg::Typed(_)) {
            return Err(syn::Error::new_spanned(
                first,
                "a split `#[actor]` (`type State = …`) #[fallback]'s first parameter \
                 must be `state: &mut Self::State` (the runtime state), not a `self` receiver",
            ));
        }
    } else if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(first, "#[fallback] first parameter must be `&self` or `&mut self`"));
    }
    let third = &sig.inputs[2];
    if !matches!(third, FnArg::Typed(_)) {
        return Err(syn::Error::new_spanned(third, "#[fallback] third parameter must be `env: &Envelope`"));
    }
    Ok(())
}

/// Extract `K` from a `#[actor] impl NativeActor` handler method's
/// third parameter and a flag for slice-handler shape. Accepts:
///   - `(&self | &mut self, ctx: &mut NativeCtx<'_>, mail: K)` —
///     single-payload handler, decodes via `Kind::decode_from_bytes`.
///   - `(&self | &mut self, ctx: &mut NativeCtx<'_>, mails: &[K])` —
///     batched cast-shape handler, decodes the whole envelope as a
///     contiguous `&[K]` slice via `decode_cast_slice` so a single
///     envelope with `count > 1` (`Mailbox::send_many`, ADR-0019)
///     reaches the handler intact. Only meaningful for cast-shape
///     kinds; structured kinds have no batch wire.
///
/// Issue 629 / Phase B: `&mut self` is now allowed alongside `&self`.
/// The dispatcher owns the cap as `Box<A>` and calls each handler
/// through `&mut Box<A>`, so either receiver shape works. Caps with
/// mutable state migrate from interior mutability (`Mutex` / `Atomic`)
/// to plain fields by flipping handler signatures to `&mut self` per
/// cap.
/// Spike split: rewrite a split handler's first parameter type
/// (`state: &mut Self::State` — or a bare `state: Self::State`) so the bare
/// `Self::State` becomes the concrete declared state type. Needed because
/// these methods are emitted into the identity's *inherent* impl, where
/// `Self::State` (a trait-associated type) is ambiguous. Only the first
/// parameter carries `Self::State` in the split authoring shape, so a
/// surgical first-param rewrite suffices — no full `visit_mut` pass (and no
/// extra `syn` feature).
fn type_is_self_state(ty: &Type) -> bool {
    if let Type::Path(tp) = ty
        && tp.qself.is_none()
        && tp.path.segments.len() == 2
        && tp.path.segments[0].ident == "Self"
        && tp.path.segments[1].ident == "State"
    {
        return true;
    }
    false
}

pub fn rewrite_self_state_first_param(method: &mut syn::ImplItemFn, concrete: &Type) {
    let Some(FnArg::Typed(pt)) = method.sig.inputs.first_mut() else {
        return;
    };
    match &mut *pt.ty {
        Type::Reference(r) if type_is_self_state(&r.elem) => {
            *r.elem = concrete.clone();
        }
        ty if type_is_self_state(ty) => {
            *pt.ty = concrete.clone();
        }
        _ => {}
    }
}

pub fn extract_native_actor_handler_kind(sig: &Signature, is_split: bool) -> syn::Result<(Type, bool)> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[actor] impl NativeActor #[handler] method must have signature \
             `(&self | &mut self, ctx: &mut NativeCtx<'_>, arg: K)` \
             (or `mail: &[K]` for batched cast kinds)",
        ));
    }
    let first = &sig.inputs[0];
    if is_split {
        // Spike split shape: the impl block is on the identity, so a handler
        // takes the runtime state explicitly — `(state: &mut Self::State,
        // ctx: &mut NativeCtx<'_>, arg: K)` — rather than a `self` receiver.
        if !matches!(first, FnArg::Typed(_)) {
            return Err(syn::Error::new_spanned(
                first,
                "a split `#[actor]` (`type State = …`) #[handler]'s first parameter \
                 must be `state: &mut Self::State` (the runtime state), not a `self` receiver",
            ));
        }
    } else if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(first, "#[handler] first parameter must be `&self` or `&mut self`"));
    }
    let third = &sig.inputs[2];
    let FnArg::Typed(pt) = third else {
        return Err(syn::Error::new_spanned(
            third,
            "#[handler] third parameter must be a typed `arg: K` or `mail: &[K]`",
        ));
    };
    // Detect `&[K]` slice handlers (any reference to a slice). Inner
    // `K` is what `HandlesKind` / `Kind::ID` reference.
    if let Type::Reference(type_ref) = &*pt.ty
        && let Type::Slice(slice) = &*type_ref.elem
    {
        return Ok(((*slice.elem).clone(), true));
    }
    Ok(((*pt.ty).clone(), false))
}

/// Extract `K` from a handler method's third parameter (`arg: K`).
/// Accepts any type path — trait-bound validation lives in the
/// generated call site: the `mail.decode_typed::<K>()` in the
/// synthesized dispatcher requires `K: Kind + AnyBitPattern + 'static`,
/// so unsupported types surface as a trait-bound error pointing at
/// the user's signature.
pub fn extract_handler_kind_type(sig: &Signature) -> syn::Result<Type> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[handler] method must have signature `(&mut self, ctx: &mut Ctx<'_>, arg: K)`",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(first, "#[handler] first parameter must be `&self` or `&mut self`"));
    }
    let third = &sig.inputs[2];
    let FnArg::Typed(pt) = third else {
        return Err(syn::Error::new_spanned(third, "#[handler] third parameter must be a typed `arg: K`"));
    };
    // `&[K]` slice/batch handlers are native-only: the wasm dispatcher
    // decodes a single `K` per mail, so an `impl HandlesKind<&[K]>` /
    // `decode_kind::<&[K]>()` would be emitted and fail to compile.
    // Reject at the boundary with a pointed message rather than letting
    // the opaque codegen error surface. (The native extractor accepts
    // this shape — see `extract_native_actor_handler_kind`.)
    if let Type::Reference(type_ref) = &*pt.ty
        && let Type::Slice(_) = &*type_ref.elem
    {
        return Err(syn::Error::new_spanned(
            &pt.ty,
            "`&[K]` slice/batch handlers are native-only; a wasm `#[handler]` \
             takes a single `arg: K` per mail",
        ));
    }
    Ok((*pt.ty).clone())
}

/// ADR-0109: a `#[handler]`'s reply contract, read off its return type.
/// The return type is the single source of truth for what a handler
/// replies — there is no separate `#[handler(reply = X)]` annotation.
pub enum HandlerReply {
    /// `-> ()` or no return type — fire-and-forget, replies nothing.
    None,
    /// `-> R: Kind` — reply `R` to the inbound sender synchronously on
    /// handler return, routed through the inbound guard's reply path.
    Sync(Type),
    /// `-> Pending<R>` — `R` is the deferred reply kind, discharged
    /// later via ADR-0093's hold ledger. The classifier recognizes the
    /// shape and publishes `R` to the manifest; the deferred send
    /// itself is wired in a follow-on (iamacoffeepot/aether#1805), so no
    /// synchronous reply is emitted for this arm here.
    Deferred(Type),
}

impl HandlerReply {
    /// The reply kind published to the `aether.kinds.inputs` manifest:
    /// `R` for both the synchronous and deferred arms (ADR-0109 §4 reads
    /// the inner `R` off `-> Pending<R>`), `None` for `-> ()`.
    pub fn manifest_kind(&self) -> Option<&Type> {
        match self {
            Self::None => None,
            Self::Sync(ty) | Self::Deferred(ty) => Some(ty),
        }
    }
}

/// Classify a handler's return type into a [`HandlerReply`] (ADR-0109).
/// `-> ()` (or an omitted return) is fire-and-forget; `-> Pending<R>`
/// — a path whose last segment is `Pending<R>` — is the deferred arm;
/// anything else is a synchronous `-> R` reply. The classifier is
/// purely syntactic (the macro has no type resolution): `R`'s `Kind`
/// bound is checked at the generated `ctx.reply(&r)` call site / the
/// `<R as Kind>::ID` manifest term.
pub fn classify_handler_reply(output: &ReturnType) -> HandlerReply {
    let ty = match output {
        ReturnType::Default => return HandlerReply::None,
        ReturnType::Type(_, ty) => ty.as_ref(),
    };
    // `-> ()` — the empty tuple — replies nothing, same as no return.
    if let Type::Tuple(tuple) = ty
        && tuple.elems.is_empty()
    {
        return HandlerReply::None;
    }
    // `-> Pending<R>` — last path segment `Pending` with one type arg.
    if let Type::Path(type_path) = ty
        && let Some(seg) = type_path.path.segments.last()
        && seg.ident == "Pending"
        && let PathArguments::AngleBracketed(args) = &seg.arguments
        && let Some(GenericArgument::Type(inner)) = args.args.first()
    {
        return HandlerReply::Deferred(inner.clone());
    }
    HandlerReply::Sync(ty.clone())
}

/// Soft validation that a `#[fallback]` method's signature is shaped
/// for `Mail<'_>`. We don't do deep type equality against
/// `::aether_actor::Mail<'_>` — the synthesized dispatcher's call
/// to `self.<fallback>(ctx, mail)` will type-check at the call site
/// and produce a clear error if the user wrote the wrong arg type.
pub fn validate_fallback_sig(sig: &Signature) -> syn::Result<()> {
    if sig.inputs.len() != 3 {
        return Err(syn::Error::new_spanned(
            sig,
            "#[fallback] method must have signature `(&mut self, ctx: &mut Ctx<'_>, mail: Mail<'_>)`",
        ));
    }
    let first = &sig.inputs[0];
    if !matches!(first, FnArg::Receiver(_)) {
        return Err(syn::Error::new_spanned(first, "#[fallback] first parameter must be `&self` or `&mut self`"));
    }
    let third = &sig.inputs[2];
    if !matches!(third, FnArg::Typed(_)) {
        return Err(syn::Error::new_spanned(third, "#[fallback] third parameter must be `mail: Mail<'_>`"));
    }
    Ok(())
}
