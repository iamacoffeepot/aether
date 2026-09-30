//! ADR-0231 §2: `#[protocol]` — a trait of row signatures becomes a protocol
//! type.
//!
//! The expansion is sugar for a hand-written protocol: the trait becomes a unit
//! struct carrying the trait's visibility and docs, plus a row list in its docs,
//! and `impl ::aether_actor::Protocol` names the rows as a tuple of
//! `::aether_actor::Row<K, O>`. The one other emission is the hidden
//! `ProtocolCast` marker, which opts the protocol into the native guard cast's
//! protocol arm (ADR-0231 §4) and carries no rule of its own. The rows' list,
//! the coverage check, and the cast rule are computed in `aether-actor` from
//! that tuple, through sealed traits, so this macro has no `Contract`,
//! `CoveredBy`, `CoversRows`, `RowSet`, or `CastTarget` impl to get wrong.
//!
//! Each method is one row: `fn name(mail: K) -> O;` is a single row,
//! `fn name(mail: K);` (or `-> ()`) a silent one, and `-> Undeclared` an unchecked
//! row. The explicit return type follows the same parser and emission path as
//! any other reply shape. Every grammar violation is reported, each at its own
//! span, in one combined error.

use proc_macro2::{Delimiter, Group, Span, TokenStream as TokenStream2};
use quote::{ToTokens, quote, quote_spanned};
use syn::spanned::Spanned;
use syn::{Attribute, Expr, ExprLit, FnArg, Ident, ItemTrait, Lit, Meta, Pat, TraitItem, TraitItemFn, Type};

use crate::diagnostics::doc_attrs;
use crate::handler_parse::{HandlerReply, classify_handler_reply, types_token_eq};

/// The most rows one protocol names: the largest tuple `RowSet` is
/// implemented for in `aether-actor`.
const MAX_ROWS: usize = 16;

/// One parsed row: the method that labels it, its kind, its reply (`None` for
/// a silent row), and the method's docs.
struct ProtocolRow {
    method: Ident,
    kind: Type,
    reply: Option<Type>,
    docs: Vec<Attribute>,
    span: Span,
}

/// Every grammar violation found, combined into one error so the author sees
/// them all at once.
#[derive(Default)]
struct Violations(Option<syn::Error>);

impl Violations {
    fn push(&mut self, error: syn::Error) {
        match &mut self.0 {
            Some(all) => all.combine(error),
            None => self.0 = Some(error),
        }
    }

    fn at(&mut self, tokens: impl ToTokens, message: &str) {
        self.push(syn::Error::new_spanned(tokens, message));
    }

    fn finish(self) -> syn::Result<()> {
        match self.0 {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

pub fn expand_protocol(attr: &TokenStream2, item: &ItemTrait) -> syn::Result<TokenStream2> {
    let mut violations = Violations::default();
    if !attr.is_empty() {
        violations.at(attr, "#[protocol] takes no arguments");
    }
    check_trait_header(item, &mut violations);

    let rows: Vec<ProtocolRow> = item.items.iter().filter_map(|member| parse_member(member, &mut violations)).collect();
    check_duplicate_kinds(&rows, &mut violations);
    if item.items.is_empty() {
        violations
            .at(&item.ident, "a protocol declares at least one row: `fn name(mail: K) -> O;` or `fn name(mail: K);`");
    }
    if rows.len() > MAX_ROWS {
        violations.at(&item.ident, &format!("a protocol has at most {MAX_ROWS} rows; this one has {}", rows.len()));
    }
    violations.finish()?;

    Ok(emit(item, &rows))
}

fn check_trait_header(item: &ItemTrait, violations: &mut Violations) {
    for attr in item.attrs.iter().filter(|attr| !attr.path().is_ident("doc")) {
        violations.at(attr, "only doc comments are allowed on a `#[protocol]` trait");
    }
    if let Some(unsafety) = &item.unsafety {
        violations.at(unsafety, "a protocol trait cannot be `unsafe`");
    }
    if let Some(auto_token) = &item.auto_token {
        violations.at(auto_token, "a protocol trait cannot be `auto`");
    }
    if !item.generics.params.is_empty() {
        violations.at(&item.generics, "a protocol takes no generics");
    }
    if let Some(where_clause) = &item.generics.where_clause {
        violations.at(where_clause, "a protocol takes no where-clause");
    }
    if !item.supertraits.is_empty() {
        violations.at(&item.supertraits, "a protocol has no supertraits");
    }
}

/// Parse one trait member into a row, recording every violation. Returns a
/// row whenever its kind could be read, so duplicate kinds are still found
/// beside other violations.
fn parse_member(member: &TraitItem, violations: &mut Violations) -> Option<ProtocolRow> {
    let TraitItem::Fn(method) = member else {
        violations.at(member, "a `#[protocol]` trait holds only row signatures: `fn name(mail: K) -> O;`");
        return None;
    };
    check_method_shape(method, violations);

    let sig = &method.sig;
    let reply = match classify_handler_reply(&sig.output) {
        HandlerReply::None => None,
        HandlerReply::Sync(ty) => Some(ty),
        HandlerReply::Deferred(_) => {
            violations.at(
                &sig.output,
                "a protocol row is spelled `-> O`, not `-> Pending<O>`; a target's deferred \
                 `-> Pending<O>` handler covers the row `O`",
            );
            None
        }
    };

    let kind = row_kind(method, violations)?;
    Some(ProtocolRow { method: sig.ident.clone(), kind, reply, docs: doc_attrs(&method.attrs), span: sig.span() })
}

fn check_method_shape(method: &TraitItemFn, violations: &mut Violations) {
    let sig = &method.sig;
    for attr in method.attrs.iter().filter(|attr| !attr.path().is_ident("doc")) {
        violations.at(attr, "only doc comments are allowed on a protocol row");
    }
    if let Some(constness) = &sig.constness {
        violations.at(constness, "a protocol row cannot be `const`");
    }
    if let Some(asyncness) = &sig.asyncness {
        violations.at(asyncness, "a protocol row cannot be `async`");
    }
    if let Some(unsafety) = &sig.unsafety {
        violations.at(unsafety, "a protocol row cannot be `unsafe`");
    }
    if let Some(abi) = &sig.abi {
        violations.at(abi, "a protocol row cannot be `extern`");
    }
    if !sig.generics.params.is_empty() {
        violations.at(&sig.generics, "a protocol row takes no generics");
    }
    if let Some(where_clause) = &sig.generics.where_clause {
        violations.at(where_clause, "a protocol row takes no where-clause");
    }
    if let Some(variadic) = &sig.variadic {
        violations.at(variadic, "a protocol row is not variadic");
    }
    if let Some(body) = &method.default {
        violations.at(body, "a protocol row has no body: `fn name(mail: K) -> O;`");
    }
}

/// The row's kind, read off its one parameter `mail: K` or `_: K`.
fn row_kind(method: &TraitItemFn, violations: &mut Violations) -> Option<Type> {
    let sig = &method.sig;
    let mut typed = Vec::new();
    for input in &sig.inputs {
        match input {
            FnArg::Receiver(receiver) => {
                violations.at(receiver, "a protocol row has no receiver: `fn name(mail: K) -> O;`");
            }
            FnArg::Typed(pat_type) => typed.push(pat_type),
        }
    }

    let [pat_type] = typed.as_slice() else {
        let message = "a protocol row takes exactly one parameter, the kind: `fn name(mail: K) -> O;`";
        if sig.inputs.is_empty() {
            violations.at(&sig.ident, message);
        } else {
            violations.at(&sig.inputs, message);
        }
        return None;
    };
    let plain_name = matches!(
        pat_type.pat.as_ref(),
        Pat::Ident(ident) if ident.by_ref.is_none() && ident.mutability.is_none() && ident.subpat.is_none()
    );
    if !plain_name && !matches!(pat_type.pat.as_ref(), Pat::Wild(_)) {
        violations.at(&pat_type.pat, "a protocol row's parameter is a plain name or `_`");
    }
    Some(pat_type.ty.as_ref().clone())
}

fn check_duplicate_kinds(rows: &[ProtocolRow], violations: &mut Violations) {
    for (index, row) in rows.iter().enumerate() {
        if let Some(first) = rows[..index].iter().find(|earlier| types_token_eq(&earlier.kind, &row.kind)) {
            violations.at(
                &row.kind,
                &format!(
                    "this kind already has a row in this protocol, `{}`: a protocol lists a kind once",
                    first.method
                ),
            );
        }
    }
}

fn emit(item: &ItemTrait, rows: &[ProtocolRow]) -> TokenStream2 {
    let vis = &item.vis;
    let ident = &item.ident;
    let docs = doc_attrs(&item.attrs);
    let row_docs = row_list_docs(rows);
    let row_tys = rows.iter().map(|row| {
        let kind = &row.kind;
        let reply = if let Some(ty) = &row.reply {
            quote! { #ty }
        } else {
            quote! { ::aether_actor::Silent }
        };
        quote_spanned! {row.span=> ::aether_actor::Row<#kind, #reply> }
    });
    // The rows tuple carries the trait body's span, so a reply type the sealed
    // row vocabulary refuses is reported on the author's rows.
    let mut rows_ty = Group::new(Delimiter::Parenthesis, quote! { #(#row_tys,)* });
    rows_ty.set_span(item.brace_token.span.join());

    quote! {
        #(#docs)*
        #(#row_docs)*
        #vis struct #ident;

        impl ::aether_actor::Protocol for #ident {
            type Rows = #rows_ty;
        }

        impl ::aether_actor::__macro_internals::ProtocolCast for #ident {}
    }
}

/// The struct's `# Rows` doc section: one item per row, labelled by its method
/// name, with the method's docs beneath it, since the method name labels the
/// row only in rustdoc.
fn row_list_docs(rows: &[ProtocolRow]) -> Vec<TokenStream2> {
    let mut lines = vec![String::new(), " # Rows".to_owned(), String::new()];
    for row in rows {
        let kind = type_display(&row.kind);
        let entry = match &row.reply {
            Some(reply) => format!(" - `{}`: `{kind} -> {}`", row.method, type_display(reply)),
            None => format!(" - `{}`: `{kind}`, silent", row.method),
        };
        lines.push(entry);

        // A row is not a rustdoc item, so only its doc text is kept; any other
        // doc attribute (`#[doc(hidden)]`, `#[doc(alias = ..)]`) is dropped
        // rather than applied to the whole protocol.
        let mut text = Vec::new();
        for value in row.docs.iter().filter_map(doc_text) {
            text.extend(value.lines().map(|line| line.strip_prefix(' ').unwrap_or(line).to_owned()));
        }
        if !text.is_empty() {
            lines.push(String::new());
            lines.extend(text.into_iter().map(|line| format!("   {line}")));
        }
    }

    lines.into_iter().map(|line| quote! { #[doc = #line] }).collect()
}

fn doc_text(attr: &Attribute) -> Option<String> {
    let Meta::NameValue(name_value) = &attr.meta else {
        return None;
    };
    let Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) = &name_value.value else {
        return None;
    };
    Some(text.value())
}

/// A type as its author would write it, without the token spacing
/// `to_string` puts around `::`, `<`, `>`, and `,`.
fn type_display(ty: &Type) -> String {
    quote! { #ty }
        .to_string()
        .replace(" :: ", "::")
        .replace(":: ", "::")
        .replace(" <", "<")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" ,", ",")
}
