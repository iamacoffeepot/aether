//! Parse `#[program] impl Program for Name`.

use aether_bloomery_kinds::ProgramName;
use syn::spanned::Spanned;
use syn::{Attribute, Expr, ExprLit, ImplItem, ImplItemConst, ItemImpl, Lit, LitStr, Meta, Type};

use crate::program::check::{ApiBinding, pair_run_with_env, reject_run_receiver, require_async_return, trailing_apis};

pub struct ProgramDef {
    pub item: ItemImpl,
    pub self_ty: Type,
    pub name: LitStr,
    pub intent: LitStr,
    /// The impl's `///` doc: the program's tool description.
    pub doc: String,
    /// The `type Input` the author wrote.
    pub input: Type,
    pub async_run: bool,
    pub sampled: bool,
    pub apis: Vec<ApiBinding>,
}

pub fn parse_program(item: ItemImpl) -> syn::Result<ProgramDef> {
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&item.generics, "#[program] does not support generic impls"));
    }
    let trait_ok = item
        .trait_
        .as_ref()
        .and_then(|(_, path, _)| path.segments.last())
        .is_some_and(|segment| segment.ident == "Program");
    if !trait_ok {
        return Err(syn::Error::new_spanned(&item.self_ty, "#[program] expects `impl Program for Name`"));
    }

    let mut name = None;
    let mut intent = None;
    let mut sampled = None;
    let mut input = None;
    let mut has_result = false;
    let mut async_run = None;
    let mut apis = Vec::new();

    for impl_item in &item.items {
        match impl_item {
            ImplItem::Const(konst) if konst.ident == "NAME" => {
                if name.is_some() {
                    return Err(syn::Error::new_spanned(&konst.ident, "`const NAME` is given twice"));
                }
                name = Some(string_literal(konst, "NAME")?);
            }
            ImplItem::Const(konst) if konst.ident == "INTENT" => {
                if intent.is_some() {
                    return Err(syn::Error::new_spanned(&konst.ident, "`const INTENT` is given twice"));
                }
                intent = Some(string_literal(konst, "INTENT")?);
            }
            ImplItem::Const(konst) if konst.ident == "MODE" => {
                if sampled.is_some() {
                    return Err(syn::Error::new_spanned(&konst.ident, "`const MODE` is given twice"));
                }
                sampled = Some(parse_mode(konst)?);
            }
            ImplItem::Type(alias) if alias.ident == "Input" => {
                if input.is_some() {
                    return Err(syn::Error::new_spanned(&alias.ident, "`type Input` is given twice"));
                }
                input = Some(alias.ty.clone());
            }
            ImplItem::Type(alias) if alias.ident == "Result" => {
                if has_result {
                    return Err(syn::Error::new_spanned(&alias.ident, "`type Result` is given twice"));
                }
                has_result = true;
            }
            ImplItem::Fn(method) if method.sig.ident == "run" => {
                if async_run.is_some() {
                    return Err(syn::Error::new_spanned(&method.sig.ident, "`fn run` is given twice"));
                }
                reject_run_receiver(&method.sig)?;
                let is_async = pair_run_with_env(&method.sig)?;
                if is_async {
                    require_async_return(&method.sig)?;
                }
                apis = trailing_apis(&method.sig, is_async)?;
                async_run = Some(is_async);
            }
            _ => {}
        }
    }

    let name = name.ok_or_else(|| {
        syn::Error::new(item.self_ty.span(), "#[program] requires `const NAME: &'static str = \"…\"`")
    })?;
    if let Err(error) = ProgramName::new(name.value()) {
        return Err(syn::Error::new_spanned(
            &name,
            format!("#[program] NAME `{}` is not a valid ProgramName: {error}", name.value()),
        ));
    }
    let intent = intent.ok_or_else(|| {
        syn::Error::new(item.self_ty.span(), "#[program] requires `const INTENT: &'static str = \"…\"`")
    })?;
    let sampled = sampled.ok_or_else(|| {
        syn::Error::new(item.self_ty.span(), "#[program] requires `const MODE: Mode = Mode::Pure` or `Mode::Sampled`")
    })?;
    let input = input.ok_or_else(|| syn::Error::new(item.self_ty.span(), "#[program] requires `type Input`"))?;
    if !has_result {
        return Err(syn::Error::new(item.self_ty.span(), "#[program] requires `type Result`"));
    }
    let async_run = async_run.ok_or_else(|| syn::Error::new(item.self_ty.span(), "#[program] requires `fn run`"))?;

    if sampled && !async_run {
        return Err(syn::Error::new(item.self_ty.span(), "#[program] Mode::Sampled requires async fn run"));
    }

    let doc = program_doc(&item)?;

    Ok(ProgramDef { self_ty: (*item.self_ty).clone(), name, intent, doc, input, async_run, sampled, apis, item })
}

/// The impl's `///` doc, which `#[program]` writes as `const DOC`: required,
/// and never declared by hand.
fn program_doc(item: &ItemImpl) -> syn::Result<String> {
    let authored = item.items.iter().find_map(|impl_item| match impl_item {
        ImplItem::Const(konst) if konst.ident == "DOC" => Some(konst),
        _ => None,
    });
    if let Some(konst) = authored {
        return Err(syn::Error::new_spanned(
            &konst.ident,
            "#[program] writes `const DOC` from the impl's `///` doc; do not declare it",
        ));
    }
    doc_text(&item.attrs).ok_or_else(|| {
        syn::Error::new(
            item.self_ty.span(),
            "#[program] needs a `///` doc on the impl: it is the program's tool description",
        )
    })
}

/// The joined text of `attrs`' `///` lines, each with one leading space
/// stripped, trimmed; `None` when blank.
fn doc_text(attrs: &[Attribute]) -> Option<String> {
    let lines: Vec<String> = attrs
        .iter()
        .filter(|attr| attr.path().is_ident("doc"))
        .filter_map(|attr| match &attr.meta {
            Meta::NameValue(pair) => match &pair.value {
                Expr::Lit(ExprLit { lit: Lit::Str(text), .. }) => Some(text.value()),
                _ => None,
            },
            _ => None,
        })
        .map(|line| line.strip_prefix(' ').map(str::to_owned).unwrap_or(line))
        .collect();
    let text = lines.join("\n").trim().to_owned();
    (!text.is_empty()).then_some(text)
}

fn string_literal(konst: &ImplItemConst, ident: &str) -> syn::Result<LitStr> {
    match peel_group(&konst.expr) {
        Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) => Ok(value.clone()),
        other => {
            Err(syn::Error::new_spanned(other, format!("#[program] needs `const {ident}` to be a string literal")))
        }
    }
}

fn parse_mode(konst: &ImplItemConst) -> syn::Result<bool> {
    let expr = peel_group(&konst.expr);
    let last = match expr {
        Expr::Path(path) => path.path.segments.last().map(|segment| segment.ident.to_string()),
        _ => None,
    };
    match last.as_deref() {
        Some("Pure") => Ok(false),
        Some("Sampled") => Ok(true),
        _ => {
            Err(syn::Error::new_spanned(expr, "#[program] requires `const MODE: Mode = Mode::Pure` or `Mode::Sampled`"))
        }
    }
}

fn peel_group(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group(group) => peel_group(&group.expr),
        other => other,
    }
}
