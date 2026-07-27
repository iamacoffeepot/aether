use proc_macro2::TokenStream as TokenStream2;
use quote::ToTokens;
use syn::meta;
use syn::parse::Parser;
use syn::token::Paren;

#[derive(Default, Clone)]
pub struct ActorOpts {
    /// ADR-0119 cardinality from `#[actor(singleton|instanced)]`, mapped to
    /// the resolver per transport — native: `One` / `Many`; FFI: `Embedded`
    /// (default) / `EmbeddedMany`. `None` where the transport supplies a
    /// default (FFI ⇒ `Embedded`).
    pub cardinality: Option<ActorCardinality>,
    /// iamacoffeepot/aether#2330: override the `runtime` feature the split path
    /// gates its `Lifecycle`/`Dispatch`/`NativeActor` impls behind, from
    /// `#[actor(runtime_feature = "name")]`. A media cap whose native half lives
    /// behind a cap-specific feature (`render-runtime` / `audio-runtime` / …)
    /// names it here so the runtime impls gate on that feature rather than the
    /// generic `runtime`. `None` ⇒ the default `feature = "runtime"`.
    pub runtime_feature: Option<String>,
    /// ADR-0123: the runtime module the struct-hosted `#[actor]` reads off
    /// disk, from a bare positional module path — `#[actor(singleton, other)]`
    /// reads `other.rs`; `#[actor(singleton, runtime::headless)]` reads
    /// `runtime/headless.rs`, the headless-companion convention. Resolved
    /// relative to the invoking file. `None` ⇒ the conventional sibling
    /// `runtime`. Only consulted on the struct-hosted path; the impl-hosted
    /// path ignores it.
    pub runtime_module: Option<syn::Path>,
    /// ADR-0166: this actor may be placed at the actor-tree root.
    pub root: bool,
    /// ADR-0166: actor types that may directly parent this actor. Repetition
    /// is intentional so one child identity can be permitted beneath several
    /// logical parents.
    pub child_of: Vec<syn::TypePath>,
    /// ADR-0166: this instanced Wasm actor may be composed beneath any Wasm
    /// parent exported from the same resident module.
    pub composable: bool,
}

pub fn parse_actor_opts(attr: TokenStream2) -> syn::Result<ActorOpts> {
    let mut opts = ActorOpts::default();
    if attr.is_empty() {
        return Ok(opts);
    }
    let parser = meta::parser(|meta| {
        if meta.path.is_ident("singleton") {
            if matches!(opts.cardinality, Some(ActorCardinality::Instanced)) {
                return Err(meta.error("`singleton` and `instanced` are mutually exclusive (ADR-0079)"));
            }
            opts.cardinality = Some(ActorCardinality::Singleton);
            Ok(())
        } else if meta.path.is_ident("instanced") {
            if matches!(opts.cardinality, Some(ActorCardinality::Singleton)) {
                return Err(meta.error("`singleton` and `instanced` are mutually exclusive (ADR-0079)"));
            }
            opts.cardinality = Some(ActorCardinality::Instanced);
            Ok(())
        } else if meta.path.is_ident("runtime_feature") {
            // iamacoffeepot/aether#2330: gate the split runtime impls on a
            // cap-specific feature instead of the default `runtime`.
            let value = meta.value()?;
            let lit: syn::LitStr = value.parse()?;
            opts.runtime_feature = Some(lit.value());
            Ok(())
        } else if meta.path.is_ident("root") {
            if opts.root {
                return Err(meta.error("duplicate `root` declaration in #[actor]"));
            }
            if meta.input.peek(Paren) || meta.input.peek(syn::Token![=]) {
                return Err(meta.error("`root` takes no arguments"));
            }
            opts.root = true;
            Ok(())
        } else if meta.path.is_ident("composable") {
            if meta.input.peek(Paren) || meta.input.peek(syn::Token![=]) {
                return Err(meta.error("`composable` takes no arguments; use `#[actor(instanced, composable)]`"));
            }
            if opts.composable {
                return Err(meta.error("duplicate `composable` declaration in #[actor]"));
            }
            if !opts.child_of.is_empty() {
                return Err(meta.error("`composable` and `child_of(...)` are mutually exclusive (ADR-0166)"));
            }
            opts.composable = true;
            Ok(())
        } else if meta.path.is_ident("child_of") {
            let content;
            syn::parenthesized!(content in meta.input);
            let parent: syn::TypePath = content.parse().map_err(|_| {
                content.error("`child_of` expects exactly one actor type path, for example `child_of(Manager)`")
            })?;
            if !content.is_empty() {
                return Err(content.error(
                    "`child_of` expects exactly one actor type path; repeat `child_of(...)` for another parent",
                ));
            }
            let parent_tokens = parent.to_token_stream().to_string();
            if opts.child_of.iter().any(|existing| existing.to_token_stream().to_string() == parent_tokens) {
                return Err(meta.error("duplicate identical `child_of` declaration in #[actor]"));
            }
            if opts.composable {
                return Err(meta.error("`composable` and `child_of(...)` are mutually exclusive (ADR-0166)"));
            }
            opts.child_of.push(parent);
            Ok(())
        } else if !meta.input.peek(syn::Token![=]) {
            // ADR-0123: a bare positional module path names the runtime module
            // the struct-hosted `#[actor]` reads off disk (default `runtime`) —
            // a lone ident for a sibling file, `runtime::headless` for a nested
            // one. The path locates a file relative to the invocation, so a
            // leading `::` (crate-absolute) has no meaning here.
            if meta.path.leading_colon.is_some() {
                return Err(meta
                    .error("#[actor] runtime module path is resolved relative to this file — drop the leading `::`"));
            }
            if opts.runtime_module.is_some() {
                return Err(meta.error("duplicate runtime module path in #[actor] — name it at most once"));
            }
            opts.runtime_module = Some(meta.path);
            Ok(())
        } else {
            Err(meta.error(
                "unrecognised #[actor] argument; expected `singleton`, `instanced`, \
                 `root`, `child_of(TypePath)`, `composable`, `runtime_feature = \"name\"`, \
                 or a bare runtime module path",
            ))
        }
    });
    Parser::parse2(parser, attr)?;
    Ok(opts)
}

/// Cardinality declaration from `#[actor(singleton|instanced)]` (ADR-0119),
/// mapped to the `Addressable::Resolver`. The `Singleton` / `Instanced`
/// markers derive from the resolver by blanket impl, so nothing emits a
/// separate marker impl; `None` ⇒ the transport default.
#[derive(Clone, Copy)]
pub enum ActorCardinality {
    Singleton,
    Instanced,
}
