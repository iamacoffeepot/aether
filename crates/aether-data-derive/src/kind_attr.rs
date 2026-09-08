//! `#[aether_data::kind(...)]` — one attribute that declares a mail kind
//! and emits the derive stack every kind otherwise repeats by hand.
//!
//! The stack is not a style choice: `Kind` supplies identity, `Schema`
//! supplies the wire codec (ADR-0188), and `Debug` / `Clone` are what
//! every consumer of a mail payload assumes. Spelled out at each
//! declaration site it drifted into dozens of orderings and memberships
//! of the same idea, none of them load-bearing. Naming the *contract*
//! instead — a kind, optionally copyable, comparable, defaultable, POD,
//! or serde-free — fixes the membership in one place.
//!
//! The emitted derives use absolute paths for everything outside the
//! prelude (`::aether_data`, `::serde`, `::bytemuck`) so a declaring
//! module needs no imports for them; the prelude traits stay unqualified
//! because that spelling resolves identically in `std` hosts and
//! `no_std` guests.

use proc_macro2::{Span, TokenStream as TokenStream2};
use quote::quote;
use syn::meta::parser as nested_meta_parser;
use syn::parse::Parser;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::{Attribute, Data, DeriveInput, LitStr, Path, Token};

/// One bare option of `#[aether_data::kind(...)]`. Held as a set rather
/// than as a field per option so adding the next contract knob doesn't
/// grow a wide boolean record.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flag {
    Copy,
    Default,
    PartialEq,
    Eq,
    Pod,
    NoSerde,
}

impl Flag {
    fn from_ident(path: &Path) -> Option<Self> {
        for (name, flag) in [
            ("copy", Self::Copy),
            ("default", Self::Default),
            ("partial_eq", Self::PartialEq),
            ("eq", Self::Eq),
            ("pod", Self::Pod),
            ("no_serde", Self::NoSerde),
        ] {
            if path.is_ident(name) {
                return Some(flag);
            }
        }
        None
    }
}

/// The parsed argument list of one `#[aether_data::kind(...)]`.
pub(crate) struct KindArgs {
    name: LitStr,
    flags: Vec<Flag>,
    extra: Vec<Path>,
}

const EXPECTED_OPTIONS: &str = "expected `name = \"...\"`, `copy`, `default`, `partial_eq`, `eq`, `pod`, \
                                `no_serde`, or `derive(Trait, ...)`";

impl KindArgs {
    fn has(&self, flag: Flag) -> bool {
        self.flags.contains(&flag)
    }

    /// The derive list this option set stands for, in a fixed order:
    /// prelude traits, then the data-layer pair, then the POD pair, then
    /// serde, then whatever `derive(...)` added.
    fn derive_paths(&self) -> Vec<TokenStream2> {
        let mut paths = vec![quote!(Debug), quote!(Clone)];
        if self.has(Flag::Copy) || self.has(Flag::Pod) {
            paths.push(quote!(Copy));
        }
        if self.has(Flag::Default) {
            paths.push(quote!(Default));
        }
        if self.has(Flag::PartialEq) || self.has(Flag::Eq) {
            paths.push(quote!(PartialEq));
        }
        if self.has(Flag::Eq) {
            paths.push(quote!(Eq));
        }
        paths.push(quote!(::aether_data::Kind));
        paths.push(quote!(::aether_data::Schema));
        if self.has(Flag::Pod) {
            paths.push(quote!(::bytemuck::Pod));
            paths.push(quote!(::bytemuck::Zeroable));
        }
        if !self.has(Flag::Pod) && !self.has(Flag::NoSerde) {
            paths.push(quote!(::serde::Serialize));
            paths.push(quote!(::serde::Deserialize));
        }
        paths.extend(self.extra.iter().map(|path| quote!(#path)));
        paths
    }
}

/// Parse the attribute's argument list. Every option is a bare flag
/// except `name = "..."` (required, once) and the `derive(...)` escape
/// hatch, which appends its paths verbatim.
pub(crate) fn parse_args(attr: &TokenStream2) -> syn::Result<KindArgs> {
    let mut name: Option<LitStr> = None;
    let mut flags: Vec<Flag> = Vec::new();
    let mut extra: Vec<Path> = Vec::new();

    nested_meta_parser(|entry| {
        if entry.path.is_ident("name") {
            if name.is_some() {
                return Err(entry.error("`name` is given twice"));
            }
            name = Some(entry.value()?.parse::<LitStr>()?);
            return Ok(());
        }
        if entry.path.is_ident("derive") {
            let inner;
            syn::parenthesized!(inner in entry.input);
            extra.extend(Punctuated::<Path, Token![,]>::parse_terminated(&inner)?);
            return Ok(());
        }
        let Some(flag) = Flag::from_ident(&entry.path) else {
            return Err(entry.error(EXPECTED_OPTIONS));
        };
        flags.push(flag);
        Ok(())
    })
    .parse2(attr.clone())?;

    let Some(name) = name else {
        let span = if attr.is_empty() {
            Span::call_site()
        } else {
            attr.span()
        };
        return Err(syn::Error::new(span, "`#[aether_data::kind]` requires `name = \"...\"`"));
    };
    let args = KindArgs { name, flags, extra };
    if args.has(Flag::Eq) && args.has(Flag::PartialEq) {
        return Err(syn::Error::new(args.name.span(), "`eq` already implies `partial_eq`; give only one"));
    }

    Ok(args)
}

/// Emit the derive stack above the untouched item. The item tokens are
/// re-emitted verbatim rather than reprinted from the parsed
/// `DeriveInput`, so doc comments, `#[repr(C)]`, `#[serde(...)]` field
/// attributes and formatting survive byte-for-byte.
pub(crate) fn expand(args: &KindArgs, item: &TokenStream2) -> syn::Result<TokenStream2> {
    let parsed: DeriveInput = syn::parse2(item.clone())?;
    reject_redundant_attrs(&parsed.attrs)?;
    if let Data::Union(u) = &parsed.data {
        return Err(syn::Error::new_spanned(u.union_token, "`#[aether_data::kind]` does not support unions"));
    }

    let derives = args.derive_paths();
    let name = &args.name;
    Ok(quote! {
        #[derive(#(#derives),*)]
        #[kind(name = #name)]
        #item
    })
}

/// A leftover `#[derive(...)]` or `#[kind(...)]` on the item is the
/// failure mode of a half-applied migration: the derive list would be
/// emitted twice (conflicting impls) or the name declared twice. Both
/// are caught here with a message naming the fix, rather than surfacing
/// as an error inside macro-expanded code the author never wrote.
fn reject_redundant_attrs(attrs: &[Attribute]) -> syn::Result<()> {
    for attr in attrs {
        if attr.path().is_ident("derive") {
            return Err(syn::Error::new_spanned(
                attr,
                "`#[aether_data::kind]` emits the derive stack itself; remove this `#[derive(...)]` \
                 (extra traits go in `derive(...)` inside the attribute)",
            ));
        }
        if attr.path().is_ident("kind") {
            return Err(syn::Error::new_spanned(
                attr,
                "`#[aether_data::kind(name = \"...\")]` already declares the name; remove this `#[kind(...)]`",
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{expand, parse_args};
    use quote::quote;

    // The option set -> derive list mapping is the only logic this
    // attribute owns; everything downstream is the existing derives.
    // Each case pins one option's contribution, so a reordered or
    // renamed flag can't silently change what a kind declaration means.
    fn derives_for(attr: &proc_macro2::TokenStream) -> String {
        let args = parse_args(attr).expect("test fixture parses");
        let item = quote! { pub struct Probe { pub value: u32 } };
        let rendered = expand(&args, &item).expect("test fixture expands").to_string();
        rendered.split("] #").next().expect("expansion starts with the derive attribute").to_owned()
    }

    #[test]
    fn base_stack_is_kind_schema_debug_clone_and_serde() {
        let derives = derives_for(&quote! { name = "test.base" });
        for expected in
            ["Debug", "Clone", ":: aether_data :: Kind", ":: aether_data :: Schema", ":: serde :: Serialize"]
        {
            assert!(derives.contains(expected), "base stack must carry {expected}, got: {derives}");
        }
        assert!(!derives.contains("Copy"), "base stack must not be Copy, got: {derives}");
        assert!(!derives.contains("Default"), "base stack must not be Default, got: {derives}");
        assert!(!derives.contains("PartialEq"), "base stack must not compare, got: {derives}");
    }

    #[test]
    fn eq_implies_partial_eq() {
        let derives = derives_for(&quote! { name = "test.eq", eq });
        assert!(derives.contains("PartialEq"), "got: {derives}");
        assert!(derives.contains(", Eq"), "got: {derives}");
    }

    #[test]
    fn partial_eq_alone_stays_partial() {
        let derives = derives_for(&quote! { name = "test.partial", partial_eq });
        assert!(derives.contains("PartialEq"), "got: {derives}");
        assert!(!derives.contains(", Eq"), "float-carrying kinds must not gain Eq, got: {derives}");
    }

    #[test]
    fn pod_adds_bytemuck_and_drops_serde() {
        let derives = derives_for(&quote! { name = "test.pod", pod });
        assert!(derives.contains(":: bytemuck :: Pod"), "got: {derives}");
        assert!(derives.contains(":: bytemuck :: Zeroable"), "got: {derives}");
        assert!(derives.contains("Copy"), "a POD kind is Copy, got: {derives}");
        assert!(!derives.contains("serde"), "a cast-encoded kind carries no serde, got: {derives}");
    }

    #[test]
    fn no_serde_drops_only_serde() {
        let derives = derives_for(&quote! { name = "test.bare", no_serde });
        assert!(!derives.contains("serde"), "got: {derives}");
        assert!(derives.contains(":: aether_data :: Kind"), "got: {derives}");
        assert!(derives.contains("Debug"), "got: {derives}");
    }

    #[test]
    fn derive_escape_hatch_appends_verbatim() {
        let derives = derives_for(&quote! { name = "test.extra", derive(Hash, PartialOrd) });
        assert!(derives.contains("Hash"), "got: {derives}");
        assert!(derives.contains("PartialOrd"), "got: {derives}");
    }

    #[test]
    fn name_reaches_the_kind_helper_attribute() {
        let args = parse_args(&quote! { name = "test.named" }).expect("parses");
        let rendered = expand(&args, &quote! { pub struct Probe; }).expect("expands").to_string();
        assert!(rendered.contains("\"test.named\""), "got: {rendered}");
    }

    #[test]
    fn rejects_missing_name() {
        let err = parse_args(&quote! { eq }).expect_err("a nameless kind must not compile");
        assert!(err.to_string().contains("name"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_option() {
        let err = parse_args(&quote! { name = "test.x", ordered }).expect_err("unknown options must not compile");
        assert!(err.to_string().contains("derive(Trait"), "error must list the accepted options, got: {err}");
    }

    #[test]
    fn rejects_eq_with_partial_eq() {
        let err = parse_args(&quote! { name = "test.x", eq, partial_eq }).expect_err("redundant pair must not compile");
        assert!(err.to_string().contains("implies"), "got: {err}");
    }

    #[test]
    fn rejects_a_leftover_derive_on_the_item() {
        let args = parse_args(&quote! { name = "test.x" }).expect("parses");
        let item = quote! { #[derive(Clone)] pub struct Probe; };
        let err = expand(&args, &item).expect_err("a half-applied migration must not compile");
        assert!(err.to_string().contains("emits the derive stack"), "got: {err}");
    }

    #[test]
    fn rejects_a_leftover_kind_helper_on_the_item() {
        let args = parse_args(&quote! { name = "test.x" }).expect("parses");
        let item = quote! { #[kind(name = "test.x")] pub struct Probe; };
        let err = expand(&args, &item).expect_err("a doubled name must not compile");
        assert!(err.to_string().contains("already declares the name"), "got: {err}");
    }
}
