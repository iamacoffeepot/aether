//! Signature shape checks for the two authored method forms.
//!
//! The authoring contract is deliberately narrow — a tool takes the state, a
//! `mcp::Context`, and one input; a standalone mapping takes the state, the
//! transport context, and one owned reply — so every rejection here can name
//! the exact parameter that is wrong. Nothing in this module resolves a type;
//! it reads spellings, because that is all a proc macro has and pretending
//! otherwise would produce diagnostics that lie.

use syn::spanned::Spanned;
use syn::{FnArg, GenericArgument, Ident, ImplItemFn, Pat, PatType, PathArguments, ReturnType, Type, TypePath};

use crate::model::Host;

/// How a tool method answers.
pub struct Answer {
    /// The declared `Output`.
    pub output: Type,
    /// True for `mcp::Outcome<Output>`, false for `Result<Output, ToolError>`.
    pub deferred: bool,
}

/// The state parameter, which is the method's first.
pub fn host_of(method: &ImplItemFn) -> syn::Result<Host> {
    match method.sig.inputs.first() {
        Some(receiver @ FnArg::Receiver(_)) => Ok(Host::Receiver(Box::new(receiver.clone()))),
        Some(FnArg::Typed(PatType { pat, ty, .. })) => {
            if matches!(pat.as_ref(), Pat::Ident(_)) {
                Ok(Host::Split(Box::new((**ty).clone())))
            } else {
                Err(syn::Error::new(pat.span(), "name the state parameter so the generated handler can thread it"))
            }
        }
        None => Err(syn::Error::new(
            method.sig.span(),
            "the first parameter must be a `self` receiver or `state: &mut Self::State`",
        )),
    }
}

/// The `index`-th parameter's pattern and type, rejecting a stray receiver.
fn typed_at<'a>(method: &'a ImplItemFn, index: usize, expected: &str) -> syn::Result<&'a PatType> {
    match method.sig.inputs.iter().nth(index) {
        Some(FnArg::Typed(typed)) => Ok(typed),
        Some(other) => Err(syn::Error::new(other.span(), expected.to_owned())),
        None => Err(syn::Error::new(method.sig.span(), expected.to_owned())),
    }
}

/// The named parameter identifier at `index`, so injected statements can
/// address a binding the author chose.
pub fn binding_at(method: &ImplItemFn, index: usize, role: &str) -> syn::Result<Ident> {
    let typed = typed_at(method, index, &format!("the {role} parameter must be plainly named"))?;
    match typed.pat.as_ref() {
        Pat::Ident(named) => Ok(named.ident.clone()),
        other => Err(syn::Error::new(other.span(), format!("name the {role} parameter so its branches can read it"))),
    }
}

/// Every type argument of `ty`'s last path segment, in order.
fn type_arguments(ty: &Type) -> Vec<Type> {
    let Type::Path(TypePath { path, .. }) = ty else {
        return Vec::new();
    };
    let Some(PathArguments::AngleBracketed(bracketed)) = path.segments.last().map(|last| &last.arguments) else {
        return Vec::new();
    };
    bracketed
        .args
        .iter()
        .filter_map(|argument| match argument {
            GenericArgument::Type(inner) => Some(inner.clone()),
            _ => None,
        })
        .collect()
}

/// True when `ty`'s last path segment is named `name` — the match a macro can
/// make, and the reason `use aether_mcp as mcp;` and a direct import both work.
fn tail_is(ty: &Type, name: &str) -> bool {
    match ty {
        Type::Path(TypePath { path, .. }) => path.segments.last().is_some_and(|last| last.ident == name),
        _ => false,
    }
}

/// The transport context `C` carried by the tool method's `mcp::Context<'_, C>`.
pub fn tool_context(method: &ImplItemFn) -> syn::Result<Type> {
    const EXPECTED: &str = "a #[mcp::tool] method's second parameter must be `mcp::Context<'_, C>`";
    let typed = typed_at(method, 1, EXPECTED)?;
    if !tail_is(&typed.ty, "Context") {
        return Err(syn::Error::new(typed.ty.span(), EXPECTED));
    }
    type_arguments(&typed.ty)
        .pop()
        .ok_or_else(|| syn::Error::new(typed.ty.span(), "mcp::Context needs its transport type: `mcp::Context<'_, C>`"))
}

/// The transport context `C` behind a mapping method's `ctx: &mut C`.
pub fn transport_context(method: &ImplItemFn) -> syn::Result<Type> {
    const EXPECTED: &str = "a #[mcp::reply] mapping method's second parameter must be \
                            `ctx: &mut NativeCtx<'_, Manual>`";
    let typed = typed_at(method, 1, EXPECTED)?;
    match typed.ty.as_ref() {
        Type::Reference(borrowed) => Ok((*borrowed.elem).clone()),
        other => Err(syn::Error::new(other.span(), EXPECTED)),
    }
}

/// The third parameter's type: a tool's `Input`, or a mapping's owned reply.
pub fn payload(method: &ImplItemFn, role: &str) -> syn::Result<Type> {
    Ok((*typed_at(method, 2, &format!("a {role} needs a third parameter carrying its {role} value"))?.ty).clone())
}

/// Reject a fourth parameter rather than silently ignoring it: the generated
/// call passes exactly three arguments, and the error Rust would raise names
/// the generated call site instead of the author's signature.
pub fn require_arity(method: &ImplItemFn, role: &str) -> syn::Result<()> {
    if let Some(extra) = method.sig.inputs.iter().nth(3) {
        return Err(syn::Error::new(extra.span(), format!("a {role} takes exactly three parameters")));
    }
    Ok(())
}

/// Read a tool method's return form.
pub fn tool_answer(method: &ImplItemFn) -> syn::Result<Answer> {
    const EXPECTED: &str = "a #[mcp::tool] method must return `Result<Output, mcp::ToolError>` \
                            or `mcp::Outcome<Output>`";
    let ReturnType::Type(_, returned) = &method.sig.output else {
        return Err(syn::Error::new(method.sig.output.span(), EXPECTED));
    };

    let deferred = if tail_is(returned, "Outcome") {
        true
    } else if tail_is(returned, "Result") {
        false
    } else {
        return Err(syn::Error::new(returned.span(), EXPECTED));
    };

    let mut arguments = type_arguments(returned);
    if arguments.is_empty() {
        return Err(syn::Error::new(returned.span(), EXPECTED));
    }
    Ok(Answer { output: arguments.swap_remove(0), deferred })
}

/// A standalone mapping method must return `Result<Output, mcp::ToolError>`.
///
/// Its `Output` is not read here — the generated branch binds the tool's
/// declared output type at the call, so a mapper that produces the wrong type
/// fails there with both types named, which is a better diagnostic than any
/// spelling comparison this macro could make.
pub fn require_result_return(method: &ImplItemFn) -> syn::Result<()> {
    const EXPECTED: &str = "a #[mcp::reply] mapping method must return `Result<Output, mcp::ToolError>`";
    match &method.sig.output {
        ReturnType::Type(_, returned) if tail_is(returned, "Result") => Ok(()),
        ReturnType::Type(_, returned) => Err(syn::Error::new(returned.span(), EXPECTED)),
        ReturnType::Default => Err(syn::Error::new(method.sig.output.span(), EXPECTED)),
    }
}
