//! `#[aether_data::kind(...)]` — one attribute that declares a mail kind
//! and emits the derive stack every kind otherwise repeats by hand.
//!
//! The stack is not a style choice: `Kind` supplies identity, `Schema`
//! supplies the wire codec (ADR-0188), and `Debug` / `Clone` are what
//! every consumer of a mail payload assumes. Spelled out at each
//! declaration site it drifted into dozens of orderings and memberships
//! of the same idea, none of them load-bearing. Naming the *contract*
//! instead — a kind, optionally copyable, comparable, defaultable, POD,
//! serde-free, or engine-only — fixes the membership in one place.
//!
//! A field that holds an ADR-0243 `Held<..>` ticket makes the kind
//! move-only: the ticket is a runtime obligation that neither clones nor
//! crosses serde, so the stack leaves `Clone`, `Serialize` and
//! `Deserialize` out. The check reads field types syntactically, as
//! `#[actor]` reads ctx types: a `Held` reached through a type alias is
//! not seen, keeps `Clone`, and fails to compile at the field because
//! `Held` has no `Clone` impl.
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
use syn::visit::{self, Visit};
use syn::{Attribute, Data, DeriveInput, Fields, LitStr, Path, Token, TypePath};

/// One bare option of `#[aether_data::kind(...)]`. Held as a set rather
/// than as a field per option so adding the next contract knob doesn't
/// grow a wide boolean record.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Flag {
    Copy,
    Default,
    PartialEq,
    Eq,
    Pod,
    NoSerde,
    EngineOnly,
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
            ("engine_only", Self::EngineOnly),
        ] {
            if path.is_ident(name) {
                return Some(flag);
            }
        }
        None
    }
}

/// The parsed argument list of one `#[aether_data::kind(...)]`.
pub struct KindArgs {
    name: LitStr,
    flags: Vec<(Flag, Span)>,
    extra: Vec<Path>,
}

const EXPECTED_OPTIONS: &str = "expected `name = \"...\"`, `copy`, `default`, `partial_eq`, `eq`, `pod`, \
                                `no_serde`, `engine_only`, or `derive(Trait, ...)`";

impl KindArgs {
    fn has(&self, flag: Flag) -> bool {
        self.span_of(flag).is_some()
    }

    fn span_of(&self, flag: Flag) -> Option<Span> {
        self.flags.iter().find(|(given, _)| *given == flag).map(|(_, span)| *span)
    }

    /// The derive list this option set stands for, in a fixed order:
    /// prelude traits, then the data-layer pair, then the POD pair, then
    /// serde, then whatever `derive(...)` added. A kind that holds a
    /// `Held` field carries neither `Clone` nor serde.
    fn derive_paths(&self, holds_held: bool) -> Vec<TokenStream2> {
        let mut paths = vec![quote!(Debug)];
        if !holds_held {
            paths.push(quote!(Clone));
        }
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
        if !holds_held && !self.has(Flag::Pod) && !self.has(Flag::NoSerde) {
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
pub fn parse_args(attr: &TokenStream2) -> syn::Result<KindArgs> {
    let mut name: Option<LitStr> = None;
    let mut flags: Vec<(Flag, Span)> = Vec::new();
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
        flags.push((flag, entry.path.span()));
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
pub fn expand(args: &KindArgs, item: &TokenStream2) -> syn::Result<TokenStream2> {
    let parsed: DeriveInput = syn::parse2(item.clone())?;
    reject_redundant_attrs(&parsed.attrs)?;
    if let Data::Union(u) = &parsed.data {
        return Err(syn::Error::new_spanned(u.union_token, "`#[aether_data::kind]` does not support unions"));
    }

    let holds_held = holds_held(&parsed);
    if holds_held && let Some(span) = args.span_of(Flag::Copy).or_else(|| args.span_of(Flag::Pod)) {
        return Err(syn::Error::new(span, "a kind holding a `Held` field is move-only; it cannot be `copy` or `pod`"));
    }

    let derives = args.derive_paths(holds_held);
    let name = &args.name;
    // `engine_only` adds no derive: it is a property of the `Kind` impl, so it
    // rides the helper attribute the `Kind` derive reads.
    let engine_only = args.has(Flag::EngineOnly).then(|| quote!(, engine_only));
    Ok(quote! {
        #[derive(#(#derives),*)]
        #[kind(name = #name #engine_only)]
        #item
    })
}

/// Whether any struct field or enum-variant field type names a path whose
/// last segment is `Held`, at any depth (`Option<Held<R>>`, `Vec<Held<R>>`).
fn holds_held(item: &DeriveInput) -> bool {
    let mut finder = HeldFinder(false);
    match &item.data {
        Data::Struct(data) => finder.visit_fields(&data.fields),
        Data::Enum(data) => data.variants.iter().for_each(|variant| finder.visit_fields(&variant.fields)),
        Data::Union(_) => {}
    }
    finder.0
}

struct HeldFinder(bool);

impl HeldFinder {
    fn visit_fields(&mut self, fields: &Fields) {
        fields.iter().for_each(|field| self.visit_type(&field.ty));
    }
}

impl<'ast> Visit<'ast> for HeldFinder {
    fn visit_type_path(&mut self, path: &'ast TypePath) {
        if path.path.segments.last().is_some_and(|segment| segment.ident == "Held") {
            self.0 = true;
        }
        visit::visit_type_path(self, path);
    }
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
        derives_for_item(attr, &quote! { pub struct Probe { pub value: u32 } })
    }

    fn derives_for_item(attr: &proc_macro2::TokenStream, item: &proc_macro2::TokenStream) -> String {
        let args = parse_args(attr).expect("test fixture parses");
        let rendered = expand(&args, item).expect("test fixture expands").to_string();
        rendered.split("] #").next().expect("expansion starts with the derive attribute").to_owned()
    }

    // Catches a detector that reads only a top-level field type: a `Held`
    // wrapped in an `Option` or inside an enum variant must drop `Clone`
    // and serde just as a bare field does.
    #[test]
    fn a_held_field_at_any_depth_drops_clone_and_serde() {
        for item in [
            quote! { pub struct Probe { pub debt: Held<Reply> } },
            quote! { pub struct Probe { pub debt: Option<Held<Reply>> } },
            quote! { pub struct Probe(pub Vec<crate::held::Held<Reply>>); },
            quote! { pub enum Probe { Idle, Waiting { debt: Held<Reply> } } },
            quote! { pub enum Probe { Idle, Waiting(Held<Reply>) } },
        ] {
            let derives = derives_for_item(&quote! { name = "test.held" }, &item);
            assert!(!derives.contains("Clone"), "a held field is move-only, got: {derives} for {item}");
            assert!(!derives.contains("serde"), "a held field never crosses serde, got: {derives} for {item}");
            assert!(derives.contains(":: aether_data :: Kind"), "got: {derives}");
        }
    }

    #[test]
    fn copy_or_pod_with_a_held_field_is_refused() {
        for flag in [quote!(copy), quote!(pod)] {
            let args = parse_args(&quote! { name = "test.held", #flag }).expect("parses");
            let item = quote! { pub struct Probe { pub debt: Held<Reply> } };
            let err = expand(&args, &item).expect_err("a move-only kind must not be copy or pod");
            assert!(err.to_string().contains("move-only"), "got: {err}");
        }
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

    fn parse_error(attr: &proc_macro2::TokenStream, why: &str) -> String {
        parse_args(attr).err().expect(why).to_string()
    }

    #[test]
    fn rejects_missing_name() {
        let err = parse_error(&quote! { eq }, "a nameless kind must not compile");
        assert!(err.contains("name"), "got: {err}");
    }

    #[test]
    fn rejects_unknown_option() {
        let err = parse_error(&quote! { name = "test.x", ordered }, "unknown options must not compile");
        assert!(err.contains("derive(Trait"), "error must list the accepted options, got: {err}");
    }

    #[test]
    fn rejects_eq_with_partial_eq() {
        let err = parse_error(&quote! { name = "test.x", eq, partial_eq }, "redundant pair must not compile");
        assert!(err.contains("implies"), "got: {err}");
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
