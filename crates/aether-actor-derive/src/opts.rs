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
    /// ADR-0230: actor types this actor depends on, from one
    /// `depends(A, B, …)` list. Each listed type emits `impl DependsOn<R> for
    /// Self` plus one `Dependency` inputs-manifest record, in list order; the
    /// host refuses the load while any entry has no `Live` route. Only
    /// keyless (`One` / `Embedded`) actors are declarable — a keyed `R` is a
    /// trait-bound compile error on the emitted impl, not a macro error here.
    pub depends: Vec<syn::TypePath>,
    /// ADR-0114: the inline children this Wasm actor spawns through the typed
    /// verbs, from one `spawns(A, B, …)` list. Each listed type emits
    /// `unsafe impl Spawns<C> for Self`, which the verbs require, and a
    /// `Rebuildable<M>` bound on the hidden `__aether_listed_children::<M>`,
    /// which every `export!` listing this actor calls for its own module, so
    /// that `export!` must list every declared child.
    pub spawns: Vec<syn::TypePath>,
    /// ADR-0166: this instanced Wasm actor may be composed beneath any Wasm
    /// parent exported from the same resident module.
    pub composable: bool,
    /// ADR-0169: the `#[handler_set]` trait this actor adopts, from
    /// `#[actor(handler_set(T))]`. Its handlers are consulted after the local
    /// dispatch chain misses, and its records join this actor's inputs
    /// manifest. At most one per actor — a set is a block of handlers, not a
    /// chain of them. `None` ⇒ the actor's own handlers are its whole receive
    /// surface.
    pub handler_set: Option<syn::Path>,
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
        } else if meta.path.is_ident("handler_set") {
            // ADR-0169: adopt a `#[handler_set]` trait's handlers. The path is
            // used verbatim in the emitted `<Self as Path>::…` delegation and
            // manifest terms, so any spelling that resolves at the adopter
            // works.
            let content;
            syn::parenthesized!(content in meta.input);
            let set: syn::Path = content.parse().map_err(|_| {
                content.error("`handler_set` expects one trait path, for example `handler_set(Chrome)`")
            })?;
            if !content.is_empty() {
                return Err(content.error(
                    "`handler_set` takes exactly one trait path — an actor adopts at most one set (ADR-0169 §4)",
                ));
            }
            if opts.handler_set.is_some() {
                return Err(meta.error("duplicate `handler_set` — an actor adopts at most one set (ADR-0169 §4)"));
            }
            opts.handler_set = Some(set);
            Ok(())
        } else if meta.path.is_ident("child_of") {
            push_actor_type_entry(&meta, &mut opts.child_of)?;
            if opts.composable {
                return Err(meta.error("`composable` and `child_of(...)` are mutually exclusive (ADR-0166)"));
            }
            Ok(())
        } else if meta.path.is_ident("depends") {
            // ADR-0230 (issue 6557): one list per actor.
            parse_type_list_once(&meta, &mut opts.depends, "depends", "RenderCapability", "FsCapability")
        } else if meta.path.is_ident("spawns") {
            // ADR-0114 (issue 6583): one list per actor, like `depends`.
            parse_type_list_once(&meta, &mut opts.spawns, "spawns", "Label", "Button")
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
                 `root`, `child_of(TypePath)`, `depends(TypePath)`, `spawns(TypePath)`, `composable`, \
                 `handler_set(TraitPath)`, \
                 `runtime_feature = \"name\"`, or a bare runtime module path",
            ))
        }
    });
    Parser::parse2(parser, attr)?;
    Ok(opts)
}

/// Parse one entry of the repeatable `child_of(P)` option into `slot`:
/// exactly one type path, rejecting a repeated identical type.
fn push_actor_type_entry(meta: &meta::ParseNestedMeta, slot: &mut Vec<syn::TypePath>) -> syn::Result<()> {
    let content;
    syn::parenthesized!(content in meta.input);
    let target: syn::TypePath = content.parse().map_err(|_| {
        content.error("`child_of` expects exactly one actor type path, for example `child_of(Manager)`")
    })?;
    if !content.is_empty() {
        return Err(
            content.error("`child_of` expects exactly one actor type path; repeat `child_of(...)` for another parent")
        );
    }
    if contains_type(slot, &target) {
        return Err(meta.error("duplicate identical `child_of` declaration in #[actor]"));
    }
    slot.push(target);
    Ok(())
}

/// Parse a `depends` / `spawns` list into `slot`, refusing a second list for
/// the same option. Empty lists are refused, so a non-empty slot means the
/// option was already written.
fn parse_type_list_once(
    meta: &meta::ParseNestedMeta,
    slot: &mut Vec<syn::TypePath>,
    option: &str,
    first: &str,
    second: &str,
) -> syn::Result<()> {
    if !slot.is_empty() {
        return Err(meta.error(format!(
            "`{option}` is written once, as a list — merge this into the first `{option}(...)`: `{option}(A, B)`"
        )));
    }
    *slot = parse_type_list(meta, option, first, second)?;
    Ok(())
}

/// Parse one `option(A, B, …)` type list — `depends` (ADR-0230) or `spawns`
/// (ADR-0114): at least one actor type path, comma-separated, trailing comma
/// allowed, each type named once. Declaration order is kept, so it is the
/// order of the emitted impls and, for `depends`, the `Dependency` records.
/// `first` and `second` are the example types the error messages show.
fn parse_type_list(
    meta: &meta::ParseNestedMeta,
    option: &str,
    first: &str,
    second: &str,
) -> syn::Result<Vec<syn::TypePath>> {
    let content;
    syn::parenthesized!(content in meta.input);
    if content.is_empty() {
        return Err(
            meta.error(format!("`{option}` expects at least one actor type path, for example `{option}({first})`"))
        );
    }
    let list_error = || {
        content.error(format!(
            "`{option}` expects a comma-separated list of actor type paths, \
             for example `{option}({first}, {second})`"
        ))
    };

    let mut list: Vec<syn::TypePath> = Vec::new();
    while !content.is_empty() {
        let target: syn::TypePath = content.parse().map_err(|_| list_error())?;
        if contains_type(&list, &target) {
            return Err(syn::Error::new_spanned(&target, format!("duplicate identical `{option}` entry in #[actor]")));
        }
        list.push(target);
        if !content.is_empty() {
            content.parse::<syn::Token![,]>().map_err(|_| list_error())?;
        }
    }
    Ok(list)
}

/// Whether `slot` already names `target`, compared by token spelling — the
/// one definition of "identical" `child_of`, `depends` and `spawns` share.
fn contains_type(slot: &[syn::TypePath], target: &syn::TypePath) -> bool {
    let target_tokens = target.to_token_stream().to_string();
    slot.iter().any(|existing| existing.to_token_stream().to_string() == target_tokens)
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
