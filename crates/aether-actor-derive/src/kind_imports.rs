//! Lifting the runtime module's `use` items onto the identity markers
//! `#[actor]` emits for them (ADR-0123).
//!
//! The struct-hosted `#[actor]` harvests each handler's `(kind, reply)` types
//! out of a *different* file and re-emits them as `impl HandlesKind<K>` at the
//! identity file's scope, spelled exactly as the runtime file spelled them. So
//! every kind named in a runtime signature had to be imported a second time, by
//! hand, in the identity file — a list that drifts silently until the next
//! handler is added.
//!
//! This module closes that: the same harvest reads the runtime file's top-level
//! `use` items, flattens them to one leaf per bound name, and keeps the leaves
//! the emitted markers actually name. `#[actor]` then puts the markers in a
//! private module carrying those imports, so the identity file no longer
//! restates them.
//!
//! Two shapes are deliberately dropped rather than translated:
//!
//! - A `self`- or `super`-rooted `use` names something *inside* the runtime
//!   tree, which is `#[cfg]`-stripped in a transport-only build and sits at a
//!   different depth from the emitted module anyway.
//! - A glob (`use foo::*`) binds names this flatten cannot enumerate, and a
//!   `#[cfg]`-gated `use` is conditional in a way the cfg-blind harvest cannot
//!   evaluate.
//!
//! Both leave the author exactly where they were before — an import in the
//! identity file — rather than emitting something that resolves differently
//! than it did in the runtime module.

use std::collections::BTreeSet;

use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use quote::quote;
use syn::{File, Ident, ItemUse, UseTree};

/// One flattened `use` leaf from the runtime module: the name it binds and the
/// `use` item that re-binds it in the emitted markers module.
pub struct KindImport {
    /// The identifier the leaf brings into scope — matched against the names
    /// the emitted markers actually spell.
    bound: String,
    /// The re-emitted `use` item, already carrying any `as` rename.
    item: TokenStream2,
}

/// Flatten every top-level `use` item in `parsed` into one [`KindImport`] per
/// bound name, dropping the shapes that cannot be re-rooted (see the module
/// header).
pub fn harvest_kind_imports(parsed: &File) -> Vec<KindImport> {
    let mut imports = Vec::new();
    for item in &parsed.items {
        let syn::Item::Use(use_item) = item else {
            continue;
        };
        if is_conditional(use_item) || is_relatively_rooted(use_item) {
            continue;
        }
        let root = if use_item.leading_colon.is_some() {
            quote! { :: }
        } else {
            quote! {}
        };
        flatten(&root, &use_item.tree, &mut Vec::new(), &mut imports);
    }
    imports
}

/// One place the emitted markers spell names, and the `#[cfg]`s that place
/// rides under.
///
/// The markers are not uniformly always-on: an `impl HandlesKind<K>` carries
/// its handler's own `#[cfg]`s, and the `HandlerEntry` inventory row naming the
/// *reply* kind additionally rides `not(target_family = "wasm")`. An import
/// emitted more broadly than the only place that names it is an
/// `unused_imports` error on the build where that place is stripped — which is
/// exactly why the identity files being retired here gated some of their manual
/// imports by hand.
pub struct ImportDemand {
    /// The whole `#[cfg(...)]` attributes guarding this place, copied onto the
    /// `use` item so both strip together.
    pub cfgs: Vec<TokenStream2>,
    /// The marker tokens themselves — read only for the names they spell.
    pub tokens: TokenStream2,
}

/// Pick one `use` item per name the markers spell, gated to match.
///
/// A name wanted in several places is emitted once, under the least-gated of
/// them: demands are visited in ascending `#[cfg]` count, and the first to claim
/// a name wins. So a kind that is both an always-on handler argument and some
/// other handler's reply lands ungated, and one that is only a reply lands under
/// `not(wasm)`.
///
/// Selection at all is what keeps the emitted module warning-clean under
/// `-D warnings`: a runtime module imports far more than its handler signatures
/// name, and every leaf that rode along unused would be an `unused_imports`
/// error at the identity file.
pub fn select_for_demands(imports: &[KindImport], demands: &[ImportDemand]) -> Vec<TokenStream2> {
    let mut ordered: Vec<&ImportDemand> = demands.iter().collect();
    ordered.sort_by_key(|demand| demand.cfgs.len());

    let mut claimed: BTreeSet<String> = BTreeSet::new();
    let mut selected = Vec::new();
    for demand in ordered {
        let named = root_idents(&demand.tokens);
        for import in imports.iter().filter(|import| named.contains(&import.bound)) {
            if !claimed.insert(import.bound.clone()) {
                continue;
            }
            let cfgs = &demand.cfgs;
            let item = &import.item;
            selected.push(quote! { #(#cfgs)* #item });
        }
    }
    selected
}

/// A `use` the cfg-blind harvest cannot evaluate, so it is not re-emitted.
fn is_conditional(use_item: &ItemUse) -> bool {
    use_item.attrs.iter().any(|attr| attr.path().is_ident("cfg"))
}

/// A `use` rooted at `self` / `super` — relative to the runtime module's own
/// position, which the emitted module does not share.
fn is_relatively_rooted(use_item: &ItemUse) -> bool {
    if use_item.leading_colon.is_some() {
        return false;
    }
    let mut tree = &use_item.tree;
    while let UseTree::Group(group) = tree {
        // A one-element group (`use {self::x};`) still has a single root.
        let mut trees = group.items.iter();
        match (trees.next(), trees.next()) {
            (Some(only), None) => tree = only,
            _ => return false,
        }
    }
    match tree {
        UseTree::Path(path) => path.ident == "self" || path.ident == "super",
        UseTree::Name(name) => name.ident == "self" || name.ident == "super",
        UseTree::Rename(rename) => rename.ident == "self" || rename.ident == "super",
        UseTree::Glob(_) | UseTree::Group(_) => false,
    }
}

/// Walk one `use` tree, accumulating `prefix` segments until each leaf, and
/// push the leaf as a standalone `use` item bound to the name it introduces.
fn flatten(root: &TokenStream2, tree: &UseTree, prefix: &mut Vec<Ident>, out: &mut Vec<KindImport>) {
    match tree {
        UseTree::Path(path) => {
            prefix.push(path.ident.clone());
            flatten(root, &path.tree, prefix, out);
            prefix.pop();
        }
        UseTree::Name(name) => {
            // `use foo::{self};` binds the last prefix segment, not `self`.
            let (bound, path) = if name.ident == "self" {
                let Some(last) = prefix.last() else {
                    return;
                };
                (last.clone(), quote! { #root #(#prefix)::* })
            } else {
                let leaf = &name.ident;
                (leaf.clone(), path_tokens(root, prefix, leaf))
            };
            out.push(KindImport { bound: bound.to_string(), item: quote! { use #path; } });
        }
        UseTree::Rename(rename) => {
            let alias = &rename.rename;
            let path = if rename.ident == "self" {
                quote! { #root #(#prefix)::* }
            } else {
                path_tokens(root, prefix, &rename.ident)
            };
            out.push(KindImport { bound: alias.to_string(), item: quote! { use #path as #alias; } });
        }
        UseTree::Group(group) => {
            for item in &group.items {
                flatten(root, item, prefix, out);
            }
        }
        UseTree::Glob(_) => {}
    }
}

/// `root` + `prefix::leaf`, the fully spelled path of one flattened leaf.
fn path_tokens(root: &TokenStream2, prefix: &[Ident], leaf: &Ident) -> TokenStream2 {
    if prefix.is_empty() {
        quote! { #root #leaf }
    } else {
        quote! { #root #(#prefix)::* :: #leaf }
    }
}

/// The identifiers `tokens` spells in *resolving* position — every ident not
/// preceded by `::`, so `crate::kinds::Read` contributes `crate` (which no
/// import binds) while a bare `Read` contributes itself.
///
/// Deliberately blunt: it over-collects (a type's generic arguments, a const
/// expression's callee) rather than modelling paths, because an extra name that
/// no `use` leaf binds costs nothing and a missed one would drop a needed
/// import.
fn root_idents(tokens: &TokenStream2) -> BTreeSet<String> {
    let mut collected = BTreeSet::new();
    let mut stack = vec![tokens.clone().into_iter()];
    let mut after_colon = false;
    while let Some(mut trees) = stack.pop() {
        let Some(tree) = trees.next() else {
            continue;
        };
        stack.push(trees);
        match tree {
            TokenTree::Ident(ident) => {
                if !after_colon {
                    collected.insert(ident.to_string());
                }
                after_colon = false;
            }
            TokenTree::Punct(punct) => after_colon = punct.as_char() == ':',
            TokenTree::Group(group) => {
                after_colon = false;
                stack.push(group.stream().into_iter());
            }
            TokenTree::Literal(_) => after_colon = false,
        }
    }
    collected
}

#[cfg(test)]
mod tests {
    use super::{ImportDemand, harvest_kind_imports, select_for_demands};
    use quote::quote;

    fn demand(cfgs: Vec<proc_macro2::TokenStream>, tokens: proc_macro2::TokenStream) -> ImportDemand {
        ImportDemand { cfgs, tokens }
    }

    fn select(source: &str, demands: Vec<ImportDemand>) -> Vec<String> {
        let parsed = syn::parse_file(source).expect("test source parses");
        select_for_demands(&harvest_kind_imports(&parsed), &demands).into_iter().map(|item| item.to_string()).collect()
    }

    fn selected(source: &str, markers: proc_macro2::TokenStream) -> Vec<String> {
        select(source, vec![demand(Vec::new(), markers)])
    }

    // Tripwire: the selection is what keeps the emitted module warning-clean.
    // A runtime module imports far more than its handler signatures name, so an
    // over-broad filter turns every unrelated leaf into an `unused_imports`
    // error at the identity file, and an over-narrow one drops a kind the
    // markers spell.
    #[test]
    fn selects_only_the_leaves_the_markers_name() {
        let selected = selected(
            "use crate::kinds::{Read, ReadResult}; use aether_substrate::Chassis; use std::io;",
            quote! { impl HandlesKind<Read> for FsCapability {} },
        );
        assert_eq!(selected, vec!["use crate :: kinds :: Read ;"]);
    }

    // Tripwire: `self`- and `super`-rooted uses name things inside the runtime
    // tree, at a depth the emitted module does not share, and a `#[cfg]`-gated
    // one is conditional in a way the cfg-blind harvest cannot evaluate.
    // Re-emitting either resolves to something other than what the runtime file
    // meant, so both are dropped and the author keeps their manual import.
    #[test]
    fn drops_relative_and_conditional_and_glob_uses() {
        let selected = selected(
            "use super::Local; use self::inner::Nested; use crate::kinds::*; \
             #[cfg(feature = \"runtime\")] use crate::kinds::Gated;",
            quote! { Local Nested Gated },
        );
        assert!(selected.is_empty(), "expected no leaves, got {selected:?}");
    }

    // Tripwire: a renamed leaf binds its alias, not the original ident, so
    // matching on the original would emit an import the markers never name (and
    // miss the one they do).
    #[test]
    fn matches_a_rename_on_the_bound_alias() {
        let source = "use crate::kinds::Read as FsRead;";
        assert!(selected(source, quote! { Read }).is_empty());
        assert_eq!(selected(source, quote! { FsRead }), vec!["use crate :: kinds :: Read as FsRead ;"]);
    }

    // Tripwire: the markers are not uniformly always-on — a reply kind is
    // spelled only by the `not(wasm)` inventory row. Emitting its import
    // ungated is an `unused_imports` error on a wasm build, and emitting a kind
    // wanted in both places under the narrower gate is an unresolved name on
    // the build that strips it. So a name takes the least-gated demand that
    // wants it, and each name is emitted once.
    #[test]
    fn gates_each_name_by_the_least_gated_place_that_names_it() {
        let selected = select(
            "use crate::kinds::{Read, ReadResult, Shared};",
            vec![
                demand(vec![quote! { #[cfg(not(target_family = "wasm"))] }], quote! { ReadResult Shared }),
                demand(Vec::new(), quote! { Read Shared }),
            ],
        );
        assert_eq!(
            selected,
            vec![
                "use crate :: kinds :: Read ;",
                "use crate :: kinds :: Shared ;",
                "# [cfg (not (target_family = \"wasm\"))] use crate :: kinds :: ReadResult ;",
            ]
        );
    }
}
