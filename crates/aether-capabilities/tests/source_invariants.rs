//! Source-shape invariants for `aether-capabilities`, enforced through the
//! run-everywhere `cargo test` harness so CI covers them (issue 2529). This
//! replaced two CI-only regex greps
//! (`check-no-scoped-visibility.sh`, `check-no-inline-runtime-mod.sh`) that
//! could false-positive on the same tokens appearing in a string, comment, or
//! macro body. Parsing each source file and walking its syntax tree makes the
//! check AST-exact instead.
//!
//! This is a lint expressed as a test, not a unit test of runtime logic: it
//! reads the crate's own `src/**/*.rs`, so the checked property is computed
//! from the AST rather than restating a declaration. It lives here because
//! `cargo test` is the harness CI's workspace test job runs.

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

use syn::visit::{Visit, visit_impl_item_const, visit_item_mod, visit_visibility};

/// Collects invariant violations while walking one file's syntax tree. `file`
/// is the crate-relative path used in failure messages; `violations` pairs
/// that path with a description of each offending item. `namespace_decls`
/// records every `const NAMESPACE = "<literal>"` this file declares (value,
/// file) for the cross-file duplicate check the caller runs after every file
/// is walked.
struct InvariantVisitor {
    file: PathBuf,
    violations: Vec<(PathBuf, String)>,
    namespace_decls: Vec<(String, PathBuf)>,
}

impl<'ast> Visit<'ast> for InvariantVisitor {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        // Tripwire (issue 2479, runtime-half-is-a-file): a cap's runtime half
        // is a sibling `runtime.rs` / `runtime/` declared with a bare
        // `mod runtime;` (which parses with `content == None`), never an
        // inline `mod runtime { … }` block (`content == Some(_)`). The
        // compiler is indifferent to the difference, so nothing but this
        // check stops the inline form from regressing back in.
        if item.ident == "runtime" && item.content.is_some() {
            self.violations.push((
                self.file.clone(),
                "inline `mod runtime { … }` block — move the runtime half to a sibling `runtime.rs` / `runtime/` behind a bare `mod runtime;`".to_string(),
            ));
        }
        // Recurse so a violation nested inside another inline `mod` is still
        // reached.
        visit_item_mod(self, item);
    }

    fn visit_visibility(&mut self, vis: &'ast syn::Visibility) {
        // Tripwire (issue 2471, pub-or-private visibility): this crate
        // expresses reach with exactly `pub` or no modifier and lets module
        // privacy plus curated re-exports carry the rest. The scoped forms
        // `pub(crate)` / `pub(super)` / `pub(in …)` all parse to
        // `Visibility::Restricted`, so matching that one variant is the exact
        // banned set — plain `pub` is `Visibility::Public` and private is
        // `Visibility::Inherited`, both left untouched.
        if let syn::Visibility::Restricted(restricted) = vis {
            let form = if restricted.in_token.is_some() {
                "pub(in …)".to_string()
            } else {
                let seg = restricted.path.segments.last().map(|s| s.ident.to_string()).unwrap_or_default();
                format!("pub({seg})")
            };
            self.violations.push((
                self.file.clone(),
                format!("scoped visibility `{form}` — use plain `pub` or privatize/restructure the module"),
            ));
        }
        visit_visibility(self, vis);
    }

    fn visit_impl_item_const(&mut self, item: &'ast syn::ImplItemConst) {
        // Record every `const NAMESPACE: &str = "<literal>"` an `impl` declares.
        // An actor's `NAMESPACE` is its mailbox identity (ADR-0098); when two
        // actors share an identity — a cap's GPU/headless or real/test-double
        // variants — the literal must live in one const both reference (see
        // `trampoline`'s `const NAMESPACE = EMBEDDED_SCOPE`), never be re-typed
        // at each site where the two spellings could silently drift. A
        // `const NAMESPACE` that references another const rather than a string
        // literal is already deduplicated, so it is not `Lit::Str` and is
        // skipped. The duplicate detection is cross-file, so this only records;
        // the caller flags any value declared as a literal in 2+ places.
        if item.ident == "NAMESPACE"
            && let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(literal), .. }) = &item.expr
        {
            self.namespace_decls.push((literal.value(), self.file.clone()));
        }
        visit_impl_item_const(self, item);
    }
}

#[test]
fn capabilities_source_invariants_hold() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");

    let mut violations: Vec<(PathBuf, String)> = Vec::new();
    let mut namespace_decls: Vec<(String, PathBuf)> = Vec::new();

    // Iterative directory walk (explicit work-stack, no recursion) over every
    // `*.rs` under `src/`.
    let mut stack = vec![src_dir];
    while let Some(dir) = stack.pop() {
        let entries = fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("read a directory entry");
            let path = entry.path();
            let file_type = entry.file_type().expect("resolve a directory entry's file type");
            if file_type.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = fs::read_to_string(&path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                let ast = syn::parse_file(&source).unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
                let rel = path.strip_prefix(&manifest_dir).unwrap_or(&path).to_path_buf();
                let mut visitor = InvariantVisitor { file: rel, violations: Vec::new(), namespace_decls: Vec::new() };
                visitor.visit_file(&ast);
                violations.extend(visitor.violations);
                namespace_decls.extend(visitor.namespace_decls);
            }
        }
    }

    // Tripwire (duplicate-`NAMESPACE` declaration): an actor namespace written
    // as a bare string literal in two or more `impl` blocks is a re-typed
    // identity that can silently drift; hoist it to one shared const both sites
    // reference (the `trampoline` / `EMBEDDED_SCOPE` idiom). A `.test.` segment
    // marks throwaway test-double namespaces (e.g. `aether.engine.test.reply_sink`),
    // whose independent fixtures may repeat the name rather than couple through a
    // shared const — that infix is the established test-namespace convention and
    // never appears in a production actor namespace.
    let mut sites_by_value: BTreeMap<String, Vec<PathBuf>> = BTreeMap::new();
    for (value, file) in namespace_decls {
        sites_by_value.entry(value).or_default().push(file);
    }
    for (value, mut sites) in sites_by_value {
        if sites.len() < 2 || value.contains(".test.") {
            continue;
        }
        sites.sort();
        let where_ = sites.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join(", ");
        violations.push((
            sites[0].clone(),
            format!(
                "actor namespace {value:?} declared as a string literal in {} `impl` blocks ({where_}) — \
                 hoist it to one shared const both reference (see `trampoline`'s `const NAMESPACE = EMBEDDED_SCOPE`)",
                sites.len(),
            ),
        ));
    }

    assert!(
        violations.is_empty(),
        "aether-capabilities source-shape invariants violated:\n{}",
        violations
            .iter()
            .map(|(file, description)| format!("  {}: {description}", file.display()))
            .collect::<Vec<_>>()
            .join("\n"),
    );
}
