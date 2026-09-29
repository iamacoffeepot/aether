//! Proc macros for fixed aggregate Bloomery view authoring.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{quote, quote_spanned};
use syn::parse::{Parse, ParseStream};
use syn::spanned::Spanned;
use syn::{
    Attribute, FnArg, GenericArgument, Ident, ImplItem, ImplItemFn, ItemImpl, Meta, PathArguments, ReturnType, Token,
    Type, TypePath, parse_macro_input,
};

struct ViewArgs {
    cursor: Ident,
}

impl Parse for ViewArgs {
    fn parse(input: ParseStream<'_>) -> syn::Result<Self> {
        let name: Ident = input.parse()?;
        if name != "cursor" {
            return Err(syn::Error::new(name.span(), "#[view] expects `cursor = field`"));
        }
        input.parse::<Token![=]>()?;
        let cursor = input.parse()?;
        if !input.is_empty() {
            return Err(input.error("#[view] accepts only `cursor = field`"));
        }
        Ok(Self { cursor })
    }
}

struct ViewDef {
    attrs: Vec<Attribute>,
    self_ty: Type,
    cursor: Ident,
    folds: Vec<Fold>,
}

struct Fold {
    method: ImplItemFn,
    event_ty: Type,
    fallible: bool,
}

/// Generate an ordinary View implementation from typed #[fold] methods on a
/// fixed aggregate.
#[proc_macro_attribute]
pub fn view(attr: TokenStream, item: TokenStream) -> TokenStream {
    let args = parse_macro_input!(attr as ViewArgs);
    let item = parse_macro_input!(item as ItemImpl);
    match parse_view(args, item) {
        Ok(def) => expand(def).into(),
        Err(error) => error.to_compile_error().into(),
    }
}

/// Marker consumed by the view macro.
#[proc_macro_attribute]
pub fn fold(_attr: TokenStream, item: TokenStream) -> TokenStream {
    let item = TokenStream2::from(item);
    quote_spanned! { item.span() =>
        ::core::compile_error!("#[fold] may only appear inside a #[view] impl block");
        #item
    }
    .into()
}

fn parse_view(args: ViewArgs, item: ItemImpl) -> syn::Result<ViewDef> {
    if item.unsafety.is_some() {
        return Err(syn::Error::new_spanned(item.unsafety, "#[view] does not support unsafe impls"));
    }
    if item.defaultness.is_some() {
        return Err(syn::Error::new_spanned(item.defaultness, "#[view] does not support default impls"));
    }
    if !item.generics.params.is_empty() || item.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(&item.generics, "#[view] does not support generic impls"));
    }
    require_view_trait(&item)?;
    require_named_type(&item.self_ty)?;

    let mut folds = Vec::new();
    for impl_item in item.items {
        match impl_item {
            ImplItem::Fn(method) if has_fold(&method) => folds.push(parse_fold(method)?),
            other => {
                return Err(syn::Error::new_spanned(
                    other,
                    "#[view] impl blocks contain only #[fold] methods; put query helpers in a separate inherent impl",
                ));
            }
        }
    }
    if folds.is_empty() {
        return Err(syn::Error::new(item.self_ty.span(), "#[view] requires at least one #[fold] method"));
    }

    Ok(ViewDef { attrs: item.attrs, self_ty: *item.self_ty, cursor: args.cursor, folds })
}

fn require_view_trait(item: &ItemImpl) -> syn::Result<()> {
    let Some((polarity, path, _)) = &item.trait_ else {
        return Err(syn::Error::new_spanned(&item.self_ty, "#[view] expects `impl View for Name`"));
    };
    if polarity.is_some() {
        return Err(syn::Error::new_spanned(path, "#[view] does not support negative impls"));
    }
    if !path
        .segments
        .last()
        .is_some_and(|segment| segment.ident == "View" && matches!(segment.arguments, PathArguments::None))
    {
        return Err(syn::Error::new_spanned(path, "#[view] expects `impl View for Name`"));
    }
    Ok(())
}

fn require_named_type(ty: &Type) -> syn::Result<()> {
    let Type::Path(TypePath { qself: None, path }) = ty else {
        return Err(syn::Error::new_spanned(ty, "#[view] requires a named non-generic aggregate type"));
    };
    if path.segments.iter().any(|segment| !matches!(segment.arguments, PathArguments::None)) {
        return Err(syn::Error::new_spanned(ty, "#[view] requires a named non-generic aggregate type"));
    }
    Ok(())
}

fn has_fold(method: &ImplItemFn) -> bool {
    method.attrs.iter().any(is_fold)
}

fn is_fold(attr: &Attribute) -> bool {
    attr.path().segments.last().is_some_and(|segment| segment.ident == "fold")
}

fn parse_fold(mut method: ImplItemFn) -> syn::Result<Fold> {
    let fold_positions: Vec<usize> =
        method.attrs.iter().enumerate().filter(|(_, attr)| is_fold(attr)).map(|(index, _)| index).collect();
    if fold_positions.len() > 1 {
        return Err(syn::Error::new_spanned(
            &method.attrs[fold_positions[1]],
            "a fold method takes exactly one #[fold] attribute",
        ));
    }
    let fold_attr = &method.attrs[fold_positions[0]];
    if !matches!(fold_attr.meta, Meta::Path(_)) {
        return Err(syn::Error::new_spanned(fold_attr, "#[fold] takes no arguments"));
    }
    method.attrs.remove(fold_positions[0]);
    // aether-suppression-request: authored folds take owned decoded events by contract
    method.attrs.push(syn::parse_quote!(#[allow(clippy::needless_pass_by_value)]));

    for attr in &method.attrs {
        if attr.path().is_ident("cfg") || attr.path().is_ident("cfg_attr") {
            return Err(syn::Error::new_spanned(
                attr,
                "#[fold] methods do not support cfg or cfg_attr; gate the complete #[view] impl instead",
            ));
        }
    }

    let sig = &method.sig;
    if matches!(sig.ident.to_string().as_str(), "empty" | "cursor" | "advance") {
        return Err(syn::Error::new_spanned(&sig.ident, "#[fold] method name conflicts with a generated View method"));
    }
    if let Some(asyncness) = &sig.asyncness {
        return Err(syn::Error::new_spanned(asyncness, "#[fold] methods are synchronous; remove `async`"));
    }
    if let Some(constness) = &sig.constness {
        return Err(syn::Error::new_spanned(constness, "#[fold] methods cannot be `const`"));
    }
    if let Some(unsafety) = &sig.unsafety {
        return Err(syn::Error::new_spanned(unsafety, "#[fold] methods cannot be `unsafe`"));
    }
    if let Some(abi) = &sig.abi {
        return Err(syn::Error::new_spanned(abi, "#[fold] methods cannot have an extern ABI"));
    }
    if let Some(variadic) = &sig.variadic {
        return Err(syn::Error::new_spanned(variadic, "#[fold] methods cannot be variadic"));
    }
    if !sig.generics.params.is_empty() || sig.generics.where_clause.is_some() {
        return Err(syn::Error::new_spanned(&sig.generics, "#[fold] methods cannot be generic"));
    }
    if sig.inputs.len() != 2 {
        return Err(syn::Error::new_spanned(
            &sig.inputs,
            "#[fold] methods take exactly `&mut self` and one owned typed event",
        ));
    }

    let Some(FnArg::Receiver(receiver)) = sig.inputs.first() else {
        return Err(syn::Error::new_spanned(&sig.inputs, "#[fold] methods start with `&mut self`"));
    };
    if receiver.reference.as_ref().is_none_or(|(_, lifetime)| lifetime.is_some())
        || receiver.mutability.is_none()
        || receiver.colon_token.is_some()
    {
        return Err(syn::Error::new_spanned(receiver, "#[fold] methods take exactly `&mut self`"));
    }
    let Some(FnArg::Typed(event)) = sig.inputs.iter().nth(1) else {
        return Err(syn::Error::new_spanned(&sig.inputs, "#[fold] methods need one owned typed event"));
    };
    if matches!(&*event.ty, Type::Reference(_)) {
        return Err(syn::Error::new_spanned(&event.ty, "#[fold] event parameters are owned values"));
    }

    let event_ty = (*event.ty).clone();
    let fallible = classify_output(&sig.output)?;
    Ok(Fold { method, event_ty, fallible })
}

fn classify_output(output: &ReturnType) -> syn::Result<bool> {
    let ReturnType::Type(_, ty) = output else {
        return Ok(false);
    };
    if is_unit(ty) {
        return Ok(false);
    }
    let Type::Path(path) = &**ty else {
        return Err(output_error(output));
    };
    let Some(segment) = path.path.segments.last() else {
        return Err(output_error(output));
    };
    let PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(output_error(output));
    };
    let mut arguments = args.args.iter();
    let first = arguments.next();
    let second = arguments.next();
    if segment.ident == "Result"
        && matches!(first, Some(GenericArgument::Type(ty)) if is_unit(ty))
        && matches!(second, Some(GenericArgument::Type(_)))
        && arguments.next().is_none()
    {
        Ok(true)
    } else {
        Err(output_error(output))
    }
}

fn is_unit(ty: &Type) -> bool {
    matches!(ty, Type::Tuple(tuple) if tuple.elems.is_empty())
}

fn output_error(output: &ReturnType) -> syn::Error {
    syn::Error::new_spanned(output, "#[fold] methods return `()` or `Result<(), E>`")
}

fn expand(def: ViewDef) -> TokenStream2 {
    let ViewDef { attrs, self_ty, cursor, folds } = def;
    let methods = folds.iter().map(|fold| &fold.method);
    let dispatches = folds.iter().map(expand_dispatch);

    quote! {
        #(#attrs)*
        impl #self_ty {
            #(#methods)*
        }

        #(#attrs)*
        impl ::aether_bloomery_view::View for #self_ty {
            type Error = ::aether_bloomery_view::__macro_internals::ViewFoldError;

            fn empty() -> Self {
                <Self as ::core::default::Default>::default()
            }

            fn cursor(&self) -> ::aether_bloomery_view::__macro_internals::Seq {
                self.#cursor.get()
            }

            fn advance(
                &mut self,
                entries: &[::aether_bloomery_view::__macro_internals::Entry],
            ) -> ::core::result::Result<(), Self::Error> {
                for entry in entries {
                    ::aether_bloomery_view::__macro_internals::check_next(self.#cursor, entry.seq)?;
                    #(#dispatches)*
                    self.#cursor.set(entry.seq);
                }
                Ok(())
            }
        }
    }
}

fn expand_dispatch(fold: &Fold) -> TokenStream2 {
    let ident = &fold.method.sig.ident;
    let event_ty = &fold.event_ty;
    let call = if fold.fallible {
        quote_spanned! { ident.span() =>
            if let ::core::result::Result::Err(source) = self.#ident(event) {
                return ::core::result::Result::Err(
                    ::aether_bloomery_view::__macro_internals::ViewFoldError::handler(
                        stringify!(#ident),
                        source,
                    ),
                );
            }
        }
    } else {
        quote_spanned! { ident.span() => self.#ident(event); }
    };

    quote_spanned! { event_ty.span() =>
        match ::aether_bloomery_view::__macro_internals::decode::<#event_ty>(entry) {
            ::core::result::Result::Ok(event) => {
                #call
            }
            ::core::result::Result::Err(error) if error.is_unmatched() => {}
            ::core::result::Result::Err(error) => {
                return ::core::result::Result::Err(
                    ::aether_bloomery_view::__macro_internals::ViewFoldError::decode(
                        stringify!(#ident),
                        error,
                    ),
                );
            }
        }
    }
}
