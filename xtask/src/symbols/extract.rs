//! Syn walk that turns a parsed file into symbol rows.

use std::path::{Path, PathBuf};

use syn::punctuated::Punctuated;
use syn::{
    Attribute, Expr, Fields, FnArg, GenericArgument, ImplItem, Item, ItemFn, ItemImpl, ItemMod, ItemStruct, ItemTrait,
    Lit, Meta, PathArguments, ReturnType, Token, Type, Visibility,
};

#[cfg(test)]
use syn::parse_file;

use crate::symbols::table::{Signature, Symbol, SymbolKind};

/// Nested `mod` depth at which we stop rather than walk off the stack.
const MAX_MODULE_DEPTH: usize = 64;

/// Parse `source` as one file and extract its inventoried items.
#[cfg(test)]
pub fn extract_source(
    crate_name: &str,
    rel_path: &str,
    source: &str,
    file_is_test: bool,
) -> Result<Vec<Symbol>, syn::Error> {
    Ok(extract_parsed(crate_name, rel_path, &parse_file(source)?, file_is_test))
}

pub fn extract_parsed(crate_name: &str, rel_path: &str, file: &syn::File, file_is_test: bool) -> Vec<Symbol> {
    let mut extractor = Extractor {
        crate_name,
        path: rel_path,
        module: module_segments(crate_name, rel_path),
        test: file_is_test || cfg_test(&file.attrs),
        symbols: Vec::new(),
    };
    extractor.walk_items(&file.items, 0);
    extractor.symbols
}

/// File-backed `mod name;` declarations in this file, with their inherited test flag.
pub fn file_mod_children(parent_file: &Path, items: &[Item], inherited_test: bool) -> Vec<FileModChild> {
    let mut children = Vec::new();
    collect_file_mods(parent_file, items, inherited_test, 0, &mut children);
    children
}

pub struct FileModChild {
    pub path: PathBuf,
    pub test: bool,
}

struct Extractor<'a> {
    crate_name: &'a str,
    path: &'a str,
    module: Vec<String>,
    test: bool,
    symbols: Vec<Symbol>,
}

impl Extractor<'_> {
    fn module_path(&self) -> String {
        self.module.join("::")
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
            Item::Trait(item) => self.record_trait(item),
            Item::Impl(item) => self.record_impl(item),
            Item::Mod(item) => self.walk_mod(item, depth),
            _ => {}
        }
    }

    fn walk_mod(&mut self, item: &ItemMod, depth: usize) {
        let Some((_, items)) = &item.content else {
            return;
        };
        let test = self.test || cfg_test(&item.attrs);
        let saved_test = self.test;
        self.module.push(item.ident.to_string());
        self.test = test;
        self.walk_items(items, depth + 1);
        self.test = saved_test;
        self.module.pop();
    }

    fn record_fn(&mut self, item: &ItemFn) {
        let test = self.test || cfg_test(&item.attrs);
        self.symbols.push(self.symbol(
            item.sig.ident.to_string(),
            SymbolKind::Fn,
            visibility(&item.vis),
            fn_signature(&item.sig),
            first_doc_line(&item.attrs),
            test,
        ));
    }

    fn record_struct(&mut self, item: &ItemStruct) {
        let test = self.test || cfg_test(&item.attrs);
        self.symbols.push(self.symbol(
            item.ident.to_string(),
            SymbolKind::Struct,
            visibility(&item.vis),
            struct_signature(&item.fields),
            first_doc_line(&item.attrs),
            test,
        ));
    }

    fn record_trait(&mut self, item: &ItemTrait) {
        let test = self.test || cfg_test(&item.attrs);
        self.symbols.push(self.symbol(
            item.ident.to_string(),
            SymbolKind::Trait,
            visibility(&item.vis),
            Signature { arity: 0, inputs: Vec::new(), output: String::new() },
            first_doc_line(&item.attrs),
            test,
        ));
    }

    fn record_impl(&mut self, item: &ItemImpl) {
        let impl_test = self.test || cfg_test(&item.attrs);
        let self_name = impl_self_name(&item.self_ty);
        for impl_item in &item.items {
            let ImplItem::Fn(method) = impl_item else {
                continue;
            };
            let test = impl_test || cfg_test(&method.attrs);
            let name = format!("{self_name}::{}", method.sig.ident);
            self.symbols.push(self.symbol(
                name,
                SymbolKind::ImplMethod,
                visibility(&method.vis),
                fn_signature(&method.sig),
                first_doc_line(&method.attrs),
                test,
            ));
        }
    }

    fn symbol(
        &self,
        name: String,
        kind: SymbolKind,
        visibility: &'static str,
        signature: Signature,
        doc: Option<String>,
        test: bool,
    ) -> Symbol {
        Symbol {
            crate_name: self.crate_name.to_string(),
            name,
            kind,
            module: self.module_path(),
            visibility: visibility.to_string(),
            signature,
            doc,
            test,
            path: self.path.to_string(),
        }
    }
}

pub fn cfg_test(attrs: &[Attribute]) -> bool {
    attrs.iter().any(|attr| {
        if !attr.path().is_ident("cfg") {
            return false;
        }
        attr.parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .ok()
            .is_some_and(|preds| preds.iter().any(meta_is_test))
    })
}

fn meta_is_test(meta: &Meta) -> bool {
    match meta {
        Meta::Path(path) => path.is_ident("test"),
        Meta::List(list) if list.path.is_ident("any") || list.path.is_ident("all") => list
            .parse_args_with(Punctuated::<Meta, Token![,]>::parse_terminated)
            .ok()
            .is_some_and(|preds| preds.iter().any(meta_is_test)),
        _ => false,
    }
}

fn visibility(vis: &Visibility) -> &'static str {
    match vis {
        Visibility::Public(_) => "public",
        Visibility::Restricted(res) if res.path.is_ident("crate") => "crate",
        Visibility::Restricted(_) => "restricted",
        Visibility::Inherited => "private",
    }
}

fn first_doc_line(attrs: &[Attribute]) -> Option<String> {
    for attr in attrs {
        if !attr.path().is_ident("doc") {
            continue;
        }
        let Meta::NameValue(nv) = &attr.meta else {
            continue;
        };
        let Expr::Lit(syn::ExprLit { lit: Lit::Str(text), .. }) = &nv.value else {
            continue;
        };
        let raw = text.value();
        let line = raw.strip_prefix(' ').unwrap_or(&raw).trim_end();
        if !line.is_empty() {
            return Some(line.to_string());
        }
    }
    None
}

fn fn_signature(sig: &syn::Signature) -> Signature {
    let inputs: Vec<String> = sig.inputs.iter().map(render_fn_arg).collect();
    let output = match &sig.output {
        ReturnType::Default => "()".to_string(),
        ReturnType::Type(_, ty) => render_type(ty),
    };
    Signature { arity: inputs.len(), inputs, output }
}

fn struct_signature(fields: &Fields) -> Signature {
    let inputs: Vec<String> = match fields {
        Fields::Named(named) => named.named.iter().map(|field| render_type(&field.ty)).collect(),
        Fields::Unnamed(unnamed) => unnamed.unnamed.iter().map(|field| render_type(&field.ty)).collect(),
        Fields::Unit => Vec::new(),
    };
    Signature { arity: inputs.len(), inputs, output: String::new() }
}

fn render_fn_arg(arg: &FnArg) -> String {
    match arg {
        FnArg::Receiver(receiver) => {
            let mut rendered = String::new();
            if receiver.reference.is_some() {
                rendered.push('&');
                if receiver.mutability.is_some() {
                    rendered.push_str("mut ");
                }
            } else if receiver.mutability.is_some() {
                rendered.push_str("mut ");
            }
            rendered.push_str("self");
            rendered
        }
        FnArg::Typed(pat) => render_type(&pat.ty),
    }
}

fn impl_self_name(ty: &Type) -> String {
    match ty {
        Type::Path(path) => path.path.segments.last().map_or_else(|| render_type(ty), |seg| seg.ident.to_string()),
        Type::Reference(reference) => impl_self_name(&reference.elem),
        Type::Paren(paren) => impl_self_name(&paren.elem),
        Type::Group(group) => impl_self_name(&group.elem),
        other => render_type(other),
    }
}

fn render_type(ty: &Type) -> String {
    match ty {
        Type::Path(path) => render_path(&path.path),
        Type::Reference(reference) => {
            let mut rendered = String::from("&");
            if reference.mutability.is_some() {
                rendered.push_str("mut ");
            }
            rendered.push_str(&render_type(&reference.elem));
            rendered
        }
        Type::Tuple(tuple) if tuple.elems.is_empty() => "()".to_string(),
        Type::Tuple(tuple) => {
            let inner = tuple.elems.iter().map(render_type).collect::<Vec<_>>().join(", ");
            format!("({inner})")
        }
        Type::Slice(slice) => format!("[{}]", render_type(&slice.elem)),
        Type::Array(array) => format!("[{}; {}]", render_type(&array.elem), render_array_len(&array.len)),
        Type::Ptr(ptr) => {
            let kind = if ptr.const_token.is_some() {
                "*const "
            } else {
                "*mut "
            };
            format!("{kind}{}", render_type(&ptr.elem))
        }
        Type::Paren(paren) => render_type(&paren.elem),
        Type::Group(group) => render_type(&group.elem),
        Type::Never(_) => "!".to_string(),
        Type::BareFn(_) => "fn".to_string(),
        Type::ImplTrait(_) => "impl Trait".to_string(),
        Type::TraitObject(_) => "dyn Trait".to_string(),
        _ => "_".to_string(),
    }
}

fn render_array_len(len: &Expr) -> String {
    match len {
        Expr::Lit(syn::ExprLit { lit: Lit::Int(int), .. }) => int.base10_digits().to_string(),
        _ => "_".to_string(),
    }
}

fn render_path(path: &syn::Path) -> String {
    let mut rendered = String::new();
    if path.leading_colon.is_some() {
        rendered.push_str("::");
    }
    for (index, segment) in path.segments.iter().enumerate() {
        if index > 0 {
            rendered.push_str("::");
        }
        rendered.push_str(&segment.ident.to_string());
        match &segment.arguments {
            PathArguments::None => {}
            PathArguments::AngleBracketed(args) => {
                let inner: Vec<String> = args.args.iter().filter_map(render_generic_arg).collect();
                if !inner.is_empty() {
                    rendered.push('<');
                    rendered.push_str(&inner.join(", "));
                    rendered.push('>');
                }
            }
            PathArguments::Parenthesized(_) => rendered.push_str("(...)"),
        }
    }
    rendered
}

fn render_generic_arg(arg: &GenericArgument) -> Option<String> {
    match arg {
        GenericArgument::Type(ty) => Some(render_type(ty)),
        GenericArgument::Const(_) => Some("_".to_string()),
        _ => None,
    }
}

/// Module path implied by a crate-relative source path, before inline `mod` names.
pub fn module_segments(crate_name: &str, rel_path: &str) -> Vec<String> {
    let mut segs = vec![crate_name.to_string()];
    let path = rel_path.replace('\\', "/");
    let stripped = path.strip_suffix(".rs").unwrap_or(&path);
    let mut parts: Vec<&str> = stripped.split('/').collect();
    if parts.first().copied() == Some("src") {
        parts.remove(0);
    }
    if matches!(parts.last().copied(), Some("mod" | "lib" | "main")) {
        parts.pop();
    }
    for part in parts {
        if part != "mod" {
            segs.push(part.replace('-', "_"));
        }
    }
    segs
}

fn collect_file_mods(
    parent_file: &Path,
    items: &[Item],
    inherited_test: bool,
    depth: usize,
    out: &mut Vec<FileModChild>,
) {
    if depth > MAX_MODULE_DEPTH {
        return;
    }
    for item in items {
        let Item::Mod(module) = item else {
            continue;
        };
        let test = inherited_test || cfg_test(&module.attrs);
        match &module.content {
            None => {
                if let Some(path) = resolve_mod_file(parent_file, &module.ident.to_string()) {
                    out.push(FileModChild { path, test });
                }
            }
            Some((_, inner)) => {
                collect_file_mods(
                    &virtual_inline_path(parent_file, &module.ident.to_string()),
                    inner,
                    test,
                    depth + 1,
                    out,
                );
            }
        }
    }
}

pub fn resolve_mod_file(parent_file: &Path, name: &str) -> Option<PathBuf> {
    let stem_dir = module_dir(parent_file);
    let file = stem_dir.join(format!("{name}.rs"));
    if file.is_file() {
        return Some(file);
    }
    let dir = stem_dir.join(name).join("mod.rs");
    dir.is_file().then_some(dir)
}

fn virtual_inline_path(parent_file: &Path, name: &str) -> PathBuf {
    module_dir(parent_file).join(format!("{name}.rs"))
}

fn module_dir(parent_file: &Path) -> PathBuf {
    let is_root = parent_file
        .file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "lib.rs" | "main.rs" | "mod.rs"));
    if is_root {
        parent_file.parent().map_or_else(|| parent_file.to_path_buf(), Path::to_path_buf)
    } else {
        parent_file.with_extension("")
    }
}

#[cfg(test)]
mod tests {
    use super::{extract_source, module_segments};
    use crate::symbols::table::{SymbolKind, Table};

    fn names(source: &str, file_is_test: bool) -> Vec<(String, SymbolKind, bool, String)> {
        extract_source("demo", "src/lib.rs", source, file_is_test)
            .expect("parse fixture")
            .into_iter()
            .map(|symbol| (symbol.name, symbol.kind, symbol.test, symbol.visibility))
            .collect()
    }

    #[test]
    fn private_test_helper_appears_and_is_marked_test() {
        // A helper sitting only under `#[cfg(test)]` is the census class this
        // inventory exists to see — rustdoc indexes miss it, and a walker that
        // skipped test modules would too.
        let source = r"
            pub fn live() {}

            #[cfg(test)]
            mod tests {
                fn scratch_dir() {}
            }
        ";
        let rows = names(source, false);
        assert!(
            rows.iter().any(|(name, kind, test, vis)| {
                name == "scratch_dir" && *kind == SymbolKind::Fn && *test && vis == "private"
            }),
            "private cfg(test) helper must appear: {rows:?}"
        );
        assert!(
            rows.iter().any(|(name, _, test, _)| name == "live" && !*test),
            "production fn is not marked test: {rows:?}"
        );
    }

    #[test]
    fn tests_tree_flag_marks_the_whole_file() {
        let source = "fn helper() {}";
        let rows = names(source, true);
        assert_eq!(rows, vec![("helper".to_string(), SymbolKind::Fn, true, "private".to_string())]);
    }

    #[test]
    fn impl_method_and_doc_and_signature_shape() {
        let source = r"
            impl Digest {
                /// Lowercase hex encoding.
                /// Second line is not the inventory line.
                pub fn to_hex(&self) -> String { String::new() }
            }
        ";
        let symbols = extract_source("demo", "src/lib.rs", source, false).expect("parse");
        let method = symbols.iter().find(|symbol| symbol.name == "Digest::to_hex").expect("impl method");
        assert_eq!(method.kind, SymbolKind::ImplMethod);
        assert_eq!(method.visibility, "public");
        assert_eq!(method.signature.arity, 1);
        assert_eq!(method.signature.inputs, vec!["&self"]);
        assert_eq!(method.signature.output, "String");
        assert_eq!(method.doc.as_deref(), Some("Lowercase hex encoding."));
    }

    #[test]
    fn module_segments_cover_lib_nested_and_tests_paths() {
        assert_eq!(module_segments("demo", "src/lib.rs"), vec!["demo"]);
        assert_eq!(module_segments("demo", "src/foo/mod.rs"), vec!["demo", "foo"]);
        assert_eq!(module_segments("demo", "src/foo/bar.rs"), vec!["demo", "foo", "bar"]);
        assert_eq!(module_segments("demo", "tests/integration.rs"), vec!["demo", "tests", "integration"]);
    }

    #[test]
    fn extract_is_order_stable_across_item_reshuffles() {
        let a = r"
            fn digest(byte: u8) -> u8 { byte }
            pub struct Digest { bytes: [u8; 32] }
        ";
        let b = r"
            pub struct Digest { bytes: [u8; 32] }
            fn digest(byte: u8) -> u8 { byte }
        ";
        let left =
            Table::new(extract_source("demo", "src/lib.rs", a, false).expect("parse a")).to_json().expect("json a");
        let right =
            Table::new(extract_source("demo", "src/lib.rs", b, false).expect("parse b")).to_json().expect("json b");
        assert_eq!(left, right, "item order in the file must not change the emitted table");
    }
}
