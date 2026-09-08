//! Syn walk that turns a parsed file into stable-path items.

use std::collections::HashMap;

use proc_macro2::{Span, TokenStream};
use quote::ToTokens;
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::token::Comma;
use syn::{
    Attribute, Fields, Ident, ImplItem, Item, ItemEnum, ItemFn, ItemImpl, ItemMod, ItemStruct, ItemTrait, ItemUse,
    TraitItem, UseTree, Variant, Visibility,
};

use crate::tokens;

const MAX_MODULE_DEPTH: usize = 64;
const ANON: &str = "_";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ItemKind {
    Fn,
    Struct,
    Enum,
    Union,
    Trait,
    Type,
    Const,
    Static,
    Macro,
    Field,
    Variant,
    Method,
    TraitMethod,
    Impl,
    Use,
}

#[derive(Clone, Debug)]
pub struct ExtractedItem {
    pub path: String,
    pub ident: String,
    pub file: String,
    pub line: usize,
    pub signature_hash: String,
    pub body_hash: String,
    pub body_token_count: usize,
    pub in_test: bool,
    pub kind: ItemKind,
}

pub fn extract_source(file_path: &str, source: &str) -> Result<Vec<ExtractedItem>, syn::Error> {
    Ok(extract_parsed(file_path, &syn::parse_file(source)?))
}

pub fn extract_parsed(file_path: &str, file: &syn::File) -> Vec<ExtractedItem> {
    let (crate_name, mut module) = crate_and_module(file_path);
    if crate_name.is_empty() {
        module = vec!["crate".into()];
    }
    let in_test = is_tests_dir_file(file_path) || cfg_test(&file.attrs);
    let mut extractor = Extractor {
        file: file_path.to_string(),
        module,
        in_test,
        symbols: Vec::new(),
        used_paths: HashMap::new(),
    };
    extractor.walk_items(&file.items, 0);
    extractor.symbols
}

pub fn crate_and_module(file_path: &str) -> (String, Vec<String>) {
    let path = file_path.replace('\\', "/");
    let (crate_name, rest) = if let Some(rest) = path.strip_prefix("crates/") {
        let mut parts = rest.splitn(2, '/');
        let dir = parts.next().unwrap_or("");
        let crate_name = dir.replace('-', "_");
        (crate_name, parts.next().unwrap_or(""))
    } else if let Some(rest) = path.strip_prefix("xtask/") {
        ("xtask".into(), rest)
    } else {
        return (String::new(), vec!["crate".into()]);
    };

    let mut module = vec![crate_name.clone()];
    if let Some(after_src) = rest.strip_prefix("src/") {
        push_file_modules(&mut module, after_src);
    } else if let Some(after_tests) = rest.strip_prefix("tests/") {
        module.push("tests".into());
        push_file_modules(&mut module, after_tests);
    } else {
        push_file_modules(&mut module, rest);
    }
    (crate_name, module)
}

fn push_file_modules(module: &mut Vec<String>, rel: &str) {
    if rel.is_empty() {
        return;
    }
    let rel = rel.strip_suffix(".rs").unwrap_or(rel);
    let rel = rel.strip_suffix("/mod").unwrap_or(rel);
    if rel == "lib" || rel == "main" || rel.is_empty() {
        return;
    }
    for seg in rel.split('/') {
        if !seg.is_empty() && seg != "mod" {
            module.push(seg.to_string());
        }
    }
}

pub fn is_tests_dir_file(path: &str) -> bool {
    path.replace('\\', "/").split('/').any(|s| s == "tests")
}

pub fn cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        attr.to_token_stream().into_iter().any(|tt| ident_is(&tt, "test"))
            || tokens_contain_ident(&attr.to_token_stream(), "test")
    })
}

pub fn is_test_fn(attrs: &[Attribute]) -> bool {
    attrs
        .iter()
        .any(|attr| attr.path().segments.last().is_some_and(|s| s.ident == "test"))
}

fn ident_is(tt: &proc_macro2::TokenTree, name: &str) -> bool {
    matches!(tt, proc_macro2::TokenTree::Ident(i) if i == name)
}

fn tokens_contain_ident(ts: &TokenStream, name: &str) -> bool {
    for tt in ts.clone() {
        match tt {
            proc_macro2::TokenTree::Ident(i) if i == name => return true,
            proc_macro2::TokenTree::Group(g) if tokens_contain_ident(&g.stream(), name) => return true,
            _ => {}
        }
    }
    false
}

struct Extractor {
    file: String,
    module: Vec<String>,
    in_test: bool,
    symbols: Vec<ExtractedItem>,
    used_paths: HashMap<String, usize>,
}

impl Extractor {
    fn module_path(&self) -> String {
        self.module.join("::")
    }

    fn qualify(&self, name: &str) -> String {
        if self.module.is_empty() {
            name.to_string()
        } else {
            format!("{}::{name}", self.module_path())
        }
    }

    fn unique(&mut self, path: String) -> String {
        let n = self.used_paths.entry(path.clone()).or_insert(0);
        *n += 1;
        if *n == 1 { path } else { format!("{path}#{}", *n) }
    }

    fn push(
        &mut self,
        path: String,
        ident: String,
        line: usize,
        sig: TokenStream,
        body: TokenStream,
        in_test: bool,
        kind: ItemKind,
    ) {
        if ident == ANON {
            return;
        }
        let path = self.unique(path);
        let (signature_hash, _) = tokens::hash_tokens(&sig);
        let (body_hash, body_token_count) = tokens::hash_tokens(&body);
        self.symbols.push(ExtractedItem {
            path,
            ident,
            file: self.file.clone(),
            line,
            signature_hash,
            body_hash,
            body_token_count,
            in_test,
            kind,
        });
    }

    fn walk_items(&mut self, items: &[Item], depth: usize) {
        if depth > MAX_MODULE_DEPTH {
            return;
        }
        for item in items {
            self.walk_item(item, depth);
        }
    }

    fn walk_item(&mut self, item: &Item, depth: usize) {
        match item {
            Item::Fn(item) => self.record_fn(item),
            Item::Struct(item) => self.record_struct(item),
            Item::Enum(item) => self.record_enum(item),
            Item::Union(item) => self.record_union(item),
            Item::Trait(item) => self.record_trait(item),
            Item::Impl(item) => self.record_impl(item),
            Item::Mod(item) => self.walk_mod(item, depth),
            Item::Use(item) => self.record_use(item),
            Item::Const(item) => {
                let test = self.in_test || cfg_test(&item.attrs);
                let (sig, body) = tokens::const_parts(item);
                self.push(
                    self.qualify(&item.ident.to_string()),
                    item.ident.to_string(),
                    line_of(&item.ident.span()),
                    sig,
                    body,
                    test,
                    ItemKind::Const,
                );
            }
            Item::Static(item) => {
                let test = self.in_test || cfg_test(&item.attrs);
                let (sig, body) = tokens::static_parts(item);
                self.push(
                    self.qualify(&item.ident.to_string()),
                    item.ident.to_string(),
                    line_of(&item.ident.span()),
                    sig,
                    body,
                    test,
                    ItemKind::Static,
                );
            }
            Item::Type(item) => {
                let test = self.in_test || cfg_test(&item.attrs);
                let (sig, body) = tokens::type_parts(item);
                self.push(
                    self.qualify(&item.ident.to_string()),
                    item.ident.to_string(),
                    line_of(&item.ident.span()),
                    sig,
                    body,
                    test,
                    ItemKind::Type,
                );
            }
            Item::Macro(item) => {
                let Some(ident) = &item.ident else {
                    return;
                };
                let test = self.in_test || cfg_test(&item.attrs);
                self.push(
                    self.qualify(&ident.to_string()),
                    ident.to_string(),
                    line_of(&ident.span()),
                    item.to_token_stream(),
                    TokenStream::new(),
                    test,
                    ItemKind::Macro,
                );
            }
            Item::TraitAlias(item) => {
                let test = self.in_test || cfg_test(&item.attrs);
                self.push(
                    self.qualify(&item.ident.to_string()),
                    item.ident.to_string(),
                    line_of(&item.ident.span()),
                    item.to_token_stream(),
                    TokenStream::new(),
                    test,
                    ItemKind::Type,
                );
            }
            _ => {}
        }
    }

    fn walk_mod(&mut self, item: &ItemMod, depth: usize) {
        let Some((_, items)) = &item.content else {
            return;
        };
        let test = self.in_test || cfg_test(&item.attrs);
        let saved = self.in_test;
        self.module.push(item.ident.to_string());
        self.in_test = test;
        self.walk_items(items, depth + 1);
        self.in_test = saved;
        self.module.pop();
    }

    fn record_fn(&mut self, item: &ItemFn) {
        let test = self.in_test || cfg_test(&item.attrs) || is_test_fn(&item.attrs);
        let (sig, body) = tokens::fn_parts(item);
        self.push(
            self.qualify(&item.sig.ident.to_string()),
            item.sig.ident.to_string(),
            line_of(&item.sig.ident.span()),
            sig,
            body,
            test,
            ItemKind::Fn,
        );
    }

    fn record_struct(&mut self, item: &ItemStruct) {
        let test = self.in_test || cfg_test(&item.attrs);
        let name = item.ident.to_string();
        let mut sig = TokenStream::new();
        for a in &item.attrs {
            a.to_tokens(&mut sig);
        }
        item.vis.to_tokens(&mut sig);
        item.struct_token.to_tokens(&mut sig);
        item.ident.to_tokens(&mut sig);
        item.generics.to_tokens(&mut sig);
        item.generics.where_clause.to_tokens(&mut sig);
        self.push(
            self.qualify(&name),
            name.clone(),
            line_of(&item.ident.span()),
            sig,
            tokens::fields_tokens(&item.fields),
            test,
            ItemKind::Struct,
        );
        self.record_fields(&name, &item.fields, &item.vis, test);
    }

    fn record_union(&mut self, item: &syn::ItemUnion) {
        let test = self.in_test || cfg_test(&item.attrs);
        let name = item.ident.to_string();
        self.push(
            self.qualify(&name),
            name.clone(),
            line_of(&item.ident.span()),
            item.to_token_stream(),
            TokenStream::new(),
            test,
            ItemKind::Union,
        );
        let fields = Fields::Named(item.fields.clone());
        self.record_fields(&name, &fields, &item.vis, test);
    }

    fn record_enum(&mut self, item: &ItemEnum) {
        let test = self.in_test || cfg_test(&item.attrs);
        let name = item.ident.to_string();
        let mut sig = TokenStream::new();
        for a in &item.attrs {
            a.to_tokens(&mut sig);
        }
        item.vis.to_tokens(&mut sig);
        item.enum_token.to_tokens(&mut sig);
        item.ident.to_tokens(&mut sig);
        item.generics.to_tokens(&mut sig);
        item.generics.where_clause.to_tokens(&mut sig);
        let mut body = TokenStream::new();
        item.variants.to_tokens(&mut body);
        self.push(
            self.qualify(&name),
            name.clone(),
            line_of(&item.ident.span()),
            sig,
            body,
            test,
            ItemKind::Enum,
        );
        for variant in &item.variants {
            self.record_variant(&name, variant, test);
        }
    }

    fn record_variant(&mut self, enum_name: &str, variant: &Variant, test: bool) {
        let vname = variant.ident.to_string();
        let path = self.qualify(&format!("{enum_name}::{vname}"));
        let mut sig = TokenStream::new();
        variant.ident.to_tokens(&mut sig);
        variant.discriminant.iter().for_each(|(eq, expr)| {
            eq.to_tokens(&mut sig);
            expr.to_tokens(&mut sig);
        });
        self.push(
            path,
            vname.clone(),
            line_of(&variant.ident.span()),
            sig,
            tokens::fields_tokens(&variant.fields),
            test,
            ItemKind::Variant,
        );
        self.record_fields(
            &format!("{enum_name}::{vname}"),
            &variant.fields,
            &Visibility::Inherited,
            test,
        );
    }

    fn record_fields(&mut self, owner: &str, fields: &Fields, vis: &Visibility, test: bool) {
        match fields {
            Fields::Named(named) => {
                for field in &named.named {
                    let Some(ident) = &field.ident else {
                        continue;
                    };
                    let fname = ident.to_string();
                    let mut sig = TokenStream::new();
                    field.vis.to_tokens(&mut sig);
                    ident.to_tokens(&mut sig);
                    field.ty.to_tokens(&mut sig);
                    let body = field.ty.to_token_stream();
                    self.push(
                        self.qualify(&format!("{owner}::{fname}")),
                        fname,
                        line_of(&ident.span()),
                        sig,
                        body,
                        test,
                        ItemKind::Field,
                    );
                }
            }
            Fields::Unnamed(unnamed) => {
                for (i, field) in unnamed.unnamed.iter().enumerate() {
                    let fname = format!("{i}");
                    let mut sig = TokenStream::new();
                    vis.to_tokens(&mut sig);
                    field.ty.to_tokens(&mut sig);
                    self.push(
                        self.qualify(&format!("{owner}::{fname}")),
                        fname,
                        line_of(&field.ty.span()),
                        sig,
                        field.ty.to_token_stream(),
                        test,
                        ItemKind::Field,
                    );
                }
            }
            Fields::Unit => {}
        }
    }

    fn record_trait(&mut self, item: &ItemTrait) {
        let test = self.in_test || cfg_test(&item.attrs);
        let name = item.ident.to_string();
        let (sig, body) = tokens::trait_parts(item);
        self.push(
            self.qualify(&name),
            name.clone(),
            line_of(&item.ident.span()),
            sig,
            body,
            test,
            ItemKind::Trait,
        );
        for trait_item in &item.items {
            match trait_item {
                TraitItem::Fn(method) => {
                    let mname = method.sig.ident.to_string();
                    let (msig, mbody) = tokens::trait_fn_parts(method);
                    self.push(
                        self.qualify(&format!("{name}::{mname}")),
                        mname,
                        line_of(&method.sig.ident.span()),
                        msig,
                        mbody,
                        test || cfg_test(&method.attrs) || is_test_fn(&method.attrs),
                        ItemKind::TraitMethod,
                    );
                }
                TraitItem::Const(c) => {
                    let cname = c.ident.to_string();
                    self.push(
                        self.qualify(&format!("{name}::{cname}")),
                        cname,
                        line_of(&c.ident.span()),
                        c.to_token_stream(),
                        TokenStream::new(),
                        test,
                        ItemKind::Const,
                    );
                }
                TraitItem::Type(t) => {
                    let tname = t.ident.to_string();
                    self.push(
                        self.qualify(&format!("{name}::{tname}")),
                        tname,
                        line_of(&t.ident.span()),
                        t.to_token_stream(),
                        TokenStream::new(),
                        test,
                        ItemKind::Type,
                    );
                }
                _ => {}
            }
        }
    }

    fn record_impl(&mut self, item: &ItemImpl) {
        let test = self.in_test || cfg_test(&item.attrs);
        let self_ty = compact_tokens(&item.self_ty.to_token_stream());
        let impl_label = match &item.trait_ {
            Some((_, trait_path, _)) => {
                let tr = compact_tokens(&trait_path.to_token_stream());
                format!("impl {tr} for {self_ty}")
            }
            None => format!("impl {self_ty}"),
        };
        let (sig, body) = tokens::impl_parts(item);
        self.push(
            self.qualify(&impl_label),
            impl_label.clone(),
            line_of(&item.impl_token.span),
            sig,
            body,
            test,
            ItemKind::Impl,
        );
        for impl_item in &item.items {
            match impl_item {
                ImplItem::Fn(method) => {
                    let mname = method.sig.ident.to_string();
                    let (msig, mbody) = tokens::impl_fn_parts(method);
                    let path = match &item.trait_ {
                        Some((_, trait_path, _)) => {
                            let tr = compact_tokens(&trait_path.to_token_stream());
                            self.qualify(&format!("impl {tr} for {self_ty}::{mname}"))
                        }
                        None => self.qualify(&format!("{self_ty}::{mname}")),
                    };
                    self.push(
                        path,
                        mname,
                        line_of(&method.sig.ident.span()),
                        msig,
                        mbody,
                        test || cfg_test(&method.attrs) || is_test_fn(&method.attrs),
                        ItemKind::Method,
                    );
                }
                ImplItem::Const(c) => {
                    let cname = c.ident.to_string();
                    self.push(
                        self.qualify(&format!("{self_ty}::{cname}")),
                        cname,
                        line_of(&c.ident.span()),
                        c.to_token_stream(),
                        TokenStream::new(),
                        test,
                        ItemKind::Const,
                    );
                }
                ImplItem::Type(t) => {
                    let tname = t.ident.to_string();
                    self.push(
                        self.qualify(&format!("{self_ty}::{tname}")),
                        tname,
                        line_of(&t.ident.span()),
                        t.to_token_stream(),
                        TokenStream::new(),
                        test,
                        ItemKind::Type,
                    );
                }
                _ => {}
            }
        }
    }

    fn record_use(&mut self, item: &ItemUse) {
        if !is_public(&item.vis) {
            return;
        }
        let test = self.in_test || cfg_test(&item.attrs);
        let mut names = Vec::new();
        collect_use_paths(&item.tree, &[], &mut names);
        for (name, imported) in names {
            let sig = syn::parse_str::<syn::Path>(&imported)
                .map(|p| p.to_token_stream())
                .unwrap_or_default();
            self.push(
                self.qualify(&name),
                name,
                line_of(&item.use_token.span),
                sig,
                TokenStream::new(),
                test,
                ItemKind::Use,
            );
        }
    }
}

fn is_public(vis: &Visibility) -> bool {
    !matches!(vis, Visibility::Inherited)
}

fn collect_use_paths(tree: &UseTree, prefix: &[String], out: &mut Vec<(String, String)>) {
    match tree {
        UseTree::Path(p) => {
            let mut next = prefix.to_vec();
            next.push(p.ident.to_string());
            collect_use_paths(&p.tree, &next, out);
        }
        UseTree::Name(n) => {
            let mut full = prefix.to_vec();
            full.push(n.ident.to_string());
            out.push((n.ident.to_string(), full.join("::")));
        }
        UseTree::Rename(r) => {
            let mut full = prefix.to_vec();
            full.push(r.ident.to_string());
            out.push((r.rename.to_string(), full.join("::")));
        }
        UseTree::Glob(_) => {}
        UseTree::Group(g) => {
            for tree in &g.items {
                collect_use_paths(tree, prefix, out);
            }
        }
    }
}

fn compact_tokens(ts: &TokenStream) -> String {
    ts.to_string()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace(" :: ", "::")
        .replace(" < ", "<")
        .replace("< ", "<")
        .replace(" >", ">")
        .replace(" ,", ",")
}

fn line_of(span: &Span) -> usize {
    span.start().line
}

/// Walk a parsed file to decide whether `line` sits in a test module / `#[test]` fn.
pub fn test_enclosing(file_path: &str, file: &syn::File, line: usize) -> (bool, Option<String>) {
    if is_tests_dir_file(file_path) {
        let fn_name = enclosing_fn_name(&file.items, line, 0);
        return (true, fn_name);
    }
    let in_cfg = cfg_test(&file.attrs);
    enclosing_test(&file.items, line, 0, in_cfg)
}

fn enclosing_test(items: &[Item], line: usize, depth: usize, in_cfg_test: bool) -> (bool, Option<String>) {
    if depth > MAX_MODULE_DEPTH {
        return (in_cfg_test, None);
    }
    for item in items {
        match item {
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    if span_covers_line(m.span(), line) || items_cover_line(inner, line) {
                        let test = in_cfg_test || cfg_test(&m.attrs);
                        return enclosing_test(inner, line, depth + 1, test);
                    }
                }
            }
            Item::Fn(f) if fn_covers(f, line) => {
                let test = in_cfg_test || cfg_test(&f.attrs) || is_test_fn(&f.attrs);
                let name = if is_test_fn(&f.attrs) || in_cfg_test {
                    Some(f.sig.ident.to_string())
                } else {
                    None
                };
                return (test, name);
            }
            Item::Impl(imp) if span_covers_line(imp.span(), line) => {
                for impl_item in &imp.items {
                    if let ImplItem::Fn(method) = impl_item {
                        if span_covers_line(method.span(), line) {
                            let test = in_cfg_test || cfg_test(&method.attrs) || is_test_fn(&method.attrs);
                            let name = if test { Some(method.sig.ident.to_string()) } else { None };
                            return (test, name);
                        }
                    }
                }
                return (in_cfg_test || cfg_test(&imp.attrs), None);
            }
            _ => {}
        }
    }
    (in_cfg_test, None)
}

fn enclosing_fn_name(items: &[Item], line: usize, depth: usize) -> Option<String> {
    if depth > MAX_MODULE_DEPTH {
        return None;
    }
    for item in items {
        match item {
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    if span_covers_line(m.span(), line) || items_cover_line(inner, line) {
                        if let Some(name) = enclosing_fn_name(inner, line, depth + 1) {
                            return Some(name);
                        }
                    }
                }
            }
            Item::Fn(f) if fn_covers(f, line) => return Some(f.sig.ident.to_string()),
            Item::Impl(imp) if span_covers_line(imp.span(), line) => {
                for impl_item in &imp.items {
                    if let ImplItem::Fn(method) = impl_item {
                        if span_covers_line(method.span(), line) {
                            return Some(method.sig.ident.to_string());
                        }
                    }
                }
            }
            _ => {}
        }
    }
    None
}

fn fn_covers(f: &ItemFn, line: usize) -> bool {
    span_covers_line(f.span(), line)
}

fn span_covers_line(span: Span, line: usize) -> bool {
    let a = span.start().line;
    let b = span.end().line.max(a);
    line >= a && line <= b
}

fn items_cover_line(items: &[Item], line: usize) -> bool {
    items.iter().any(|item| span_covers_line(item.span(), line))
}

/// Count `#[test]` functions in a parsed file (plus cfg(test) fns in tests/ files).
pub fn count_test_fns(file_path: &str, file: &syn::File) -> usize {
    let in_test = is_tests_dir_file(file_path) || cfg_test(&file.attrs);
    count_test_fns_items(&file.items, 0, in_test)
}

fn count_test_fns_items(items: &[Item], depth: usize, in_test: bool) -> usize {
    if depth > MAX_MODULE_DEPTH {
        return 0;
    }
    let mut n = 0;
    for item in items {
        match item {
            Item::Fn(f) if is_test_fn(&f.attrs) || (in_test && looks_like_unit_test(f)) => n += 1,
            Item::Mod(m) => {
                if let Some((_, inner)) = &m.content {
                    n += count_test_fns_items(inner, depth + 1, in_test || cfg_test(&m.attrs));
                }
            }
            _ => {}
        }
    }
    n
}

fn looks_like_unit_test(f: &ItemFn) -> bool {
    is_test_fn(&f.attrs)
}

/// Helper used by tests to keep Punctuated in scope.
#[allow(dead_code)]
fn _punctuated_marker() -> Punctuated<Ident, Comma> {
    Punctuated::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn module_path_from_src_file() {
        let (crate_name, module) = crate_and_module("crates/aether-bloomery/src/reduce/view.rs");
        assert_eq!(crate_name, "aether_bloomery");
        assert_eq!(module, vec!["aether_bloomery", "reduce", "view"]);
    }

    #[test]
    fn lib_rs_is_crate_root() {
        let (_, module) = crate_and_module("crates/aether-bloomery/src/lib.rs");
        assert_eq!(module, vec!["aether_bloomery"]);
    }

    #[test]
    fn extracts_struct_field_variant_and_impl() {
        let src = r#"
            pub struct Holder { pub flag: bool }
            pub enum Kind { Alpha, Beta { n: u8 } }
            impl Holder {
                pub fn take(&self) -> bool { self.flag }
            }
            impl std::fmt::Display for Holder {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result { Ok(()) }
            }
            pub fn free() {}
        "#;
        let items = extract_source("crates/demo/src/lib.rs", src).unwrap();
        let paths: Vec<_> = items.iter().map(|i| i.path.as_str()).collect();
        assert!(paths.contains(&"demo::Holder"));
        assert!(paths.contains(&"demo::Holder::flag"));
        assert!(paths.contains(&"demo::Kind::Alpha"));
        assert!(paths.contains(&"demo::Kind::Beta"));
        assert!(paths.contains(&"demo::Holder::take"));
        assert!(
            paths
                .iter()
                .any(|p| p.contains("impl") && p.contains("Display") && p.contains("Holder"))
        );
        assert!(paths.contains(&"demo::free"));
    }

    #[test]
    fn cfg_test_module_marks_items() {
        let src = r#"
            #[cfg(test)]
            mod tests {
                fn helper() {}
                #[test]
                fn pins_holder() {}
            }
        "#;
        let items = extract_source("crates/demo/src/lib.rs", src).unwrap();
        let helper = items.iter().find(|i| i.ident == "helper").unwrap();
        assert!(helper.in_test);
        assert_eq!(helper.path, "demo::tests::helper");
    }
}
