//! Parse `#[reactor] impl Reactor for Name`.

use syn::spanned::Spanned;
use syn::{
    Attribute, Block, Expr, ExprLit, Ident, ImplItem, ImplItemConst, ImplItemFn, ItemImpl, Lit, Pat, Type, Visibility,
};

use crate::check::{
    param_ident, reject_async, reject_borrowed_param, reject_const, reject_generics, reject_unsafe,
    require_immutable_self, require_output_type, typed_inputs,
};

pub struct ReactorDef {
    pub attrs: Vec<Attribute>,
    pub self_ty: Type,
    pub name_const: ImplItemConst,
    pub rules: Vec<Rule>,
    pub helpers: Vec<ImplItem>,
}

pub struct Rule {
    pub vis: Visibility,
    pub attrs: Vec<Attribute>,
    pub ident: Ident,
    pub trigger_pat: Pat,
    pub trigger_ty: Type,
    pub params: Vec<Param>,
    pub output_ty: Type,
    pub body: Block,
}

pub struct Param {
    pub ident: Ident,
    pub ty: Type,
}

pub fn parse_reactor(item: ItemImpl) -> syn::Result<ReactorDef> {
    if !item.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(&item.generics, "#[reactor] does not support generic impls"));
    }
    let trait_ok = item
        .trait_
        .as_ref()
        .and_then(|(_, path, _)| path.segments.last())
        .is_some_and(|segment| segment.ident == "Reactor");
    if !trait_ok {
        return Err(syn::Error::new_spanned(&item.self_ty, "#[reactor] expects `impl Reactor for Name`"));
    }

    let mut name_const = None;
    let mut rules = Vec::new();
    let mut helpers = Vec::new();

    for impl_item in item.items {
        match impl_item {
            ImplItem::Const(konst) if konst.ident == "NAME" => {
                validate_name_const(&konst)?;
                if name_const.is_some() {
                    return Err(syn::Error::new_spanned(&konst.ident, "`const NAME` is given twice"));
                }
                name_const = Some(konst);
            }
            ImplItem::Fn(method) if has_rule(&method) => rules.push(parse_rule(method)?),
            ImplItem::Fn(method) => helpers.push(ImplItem::Fn(method)),
            ImplItem::Const(konst) => helpers.push(ImplItem::Const(konst)),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "#[reactor] impl blocks hold `const NAME`, `#[rule]` methods, and inherent helpers",
                ));
            }
        }
    }

    let mut name_const = name_const.ok_or_else(|| {
        syn::Error::new(item.self_ty.span(), "#[reactor] requires `const NAME: &'static str = \"…\"`")
    })?;
    name_const.vis = Visibility::Inherited;
    if rules.is_empty() {
        return Err(syn::Error::new(item.self_ty.span(), "#[reactor] requires at least one `#[rule]` method"));
    }

    Ok(ReactorDef { attrs: item.attrs, self_ty: *item.self_ty, name_const, rules, helpers })
}

fn validate_name_const(konst: &ImplItemConst) -> syn::Result<()> {
    let Expr::Lit(ExprLit { lit: Lit::Str(_), .. }) = peel_group(&konst.expr) else {
        return Err(syn::Error::new_spanned(&konst.expr, "#[reactor] needs `const NAME` to be a string literal"));
    };
    Ok(())
}

fn peel_group(expr: &Expr) -> &Expr {
    match expr {
        Expr::Group(group) => peel_group(&group.expr),
        other => other,
    }
}

fn has_rule(method: &ImplItemFn) -> bool {
    method.attrs.iter().any(is_rule)
}

fn parse_rule(mut method: ImplItemFn) -> syn::Result<Rule> {
    let rule_positions: Vec<usize> =
        method.attrs.iter().enumerate().filter(|(_, attr)| is_rule(attr)).map(|(index, _)| index).collect();
    if rule_positions.len() > 1 {
        return Err(syn::Error::new_spanned(
            &method.attrs[rule_positions[1]],
            "a rule takes exactly one #[rule] attribute",
        ));
    }
    method.attrs.remove(rule_positions[0]);

    let sig = &method.sig;
    reject_async(sig)?;
    reject_const(sig)?;
    reject_unsafe(sig)?;
    reject_generics(sig)?;
    require_immutable_self(sig)?;
    let output_ty = require_output_type(sig)?;
    let typed = typed_inputs(sig)?;
    for input in &typed {
        reject_borrowed_param(&input.ty)?;
    }

    let trigger = typed[0];
    let trigger_pat = (*trigger.pat).clone();
    let trigger_ty = (*trigger.ty).clone();

    let mut params = Vec::new();
    for input in typed.iter().skip(1) {
        params.push(Param { ident: param_ident(&input.pat)?, ty: (*input.ty).clone() });
    }

    Ok(Rule {
        vis: method.vis,
        attrs: method.attrs,
        ident: method.sig.ident,
        trigger_pat,
        trigger_ty,
        params,
        output_ty,
        body: method.block,
    })
}

fn is_rule(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|segment| segment.ident == "rule")
}
