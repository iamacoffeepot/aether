//! Parse `#[program] impl Program for Name`.

use aether_bloomery_kinds::ProgramName;
use syn::spanned::Spanned;
use syn::{Expr, ExprLit, ImplItem, ImplItemConst, ItemImpl, Lit, LitStr, Type};

use crate::check::{pair_run_with_env, reject_run_receiver};

pub struct ProgramDef {
    pub item: ItemImpl,
    pub self_ty: Type,
    pub name: LitStr,
    pub intent: LitStr,
    pub input: Type,
    pub result: Type,
    pub async_run: bool,
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
    let mut mode = None;
    let mut input = None;
    let mut result = None;
    let mut async_run = None;

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
                if mode.is_some() {
                    return Err(syn::Error::new_spanned(&konst.ident, "`const MODE` is given twice"));
                }
                require_pure_mode(konst)?;
                mode = Some(());
            }
            ImplItem::Type(alias) if alias.ident == "Input" => {
                if input.is_some() {
                    return Err(syn::Error::new_spanned(&alias.ident, "`type Input` is given twice"));
                }
                input = Some(alias.ty.clone());
            }
            ImplItem::Type(alias) if alias.ident == "Result" => {
                if result.is_some() {
                    return Err(syn::Error::new_spanned(&alias.ident, "`type Result` is given twice"));
                }
                result = Some(alias.ty.clone());
            }
            ImplItem::Fn(method) if method.sig.ident == "run" => {
                if async_run.is_some() {
                    return Err(syn::Error::new_spanned(&method.sig.ident, "`fn run` is given twice"));
                }
                reject_run_receiver(&method.sig)?;
                async_run = Some(pair_run_with_env(&method.sig)?);
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
    if mode.is_none() {
        return Err(syn::Error::new(item.self_ty.span(), "#[program] requires `const MODE: Mode = Mode::Pure`"));
    }
    let input = input.ok_or_else(|| syn::Error::new(item.self_ty.span(), "#[program] requires `type Input`"))?;
    let result = result.ok_or_else(|| syn::Error::new(item.self_ty.span(), "#[program] requires `type Result`"))?;
    let async_run = async_run.ok_or_else(|| syn::Error::new(item.self_ty.span(), "#[program] requires `fn run`"))?;

    Ok(ProgramDef { self_ty: (*item.self_ty).clone(), name, intent, input, result, async_run, item })
}

fn string_literal(konst: &ImplItemConst, ident: &str) -> syn::Result<LitStr> {
    match peel_group(&konst.expr) {
        Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) => Ok(value.clone()),
        other => {
            Err(syn::Error::new_spanned(other, format!("#[program] needs `const {ident}` to be a string literal")))
        }
    }
}

fn require_pure_mode(konst: &ImplItemConst) -> syn::Result<()> {
    let expr = peel_group(&konst.expr);
    let last = match expr {
        Expr::Path(path) => path.path.segments.last().map(|segment| segment.ident.to_string()),
        _ => None,
    };
    if last.as_deref() == Some("Pure") {
        Ok(())
    } else {
        Err(syn::Error::new_spanned(expr, "#[program] requires `const MODE: Mode = Mode::Pure`"))
    }
}

fn peel_group(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group(group) => peel_group(&group.expr),
        other => other,
    }
}
