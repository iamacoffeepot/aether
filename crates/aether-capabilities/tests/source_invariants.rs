//! Source-shape invariants for `aether-capabilities`, enforced through the
//! run-everywhere `cargo test` harness so both local preflight and CI cover
//! them (issue 2529). This replaced two CI-only regex greps
//! (`check-no-scoped-visibility.sh`, `check-no-inline-runtime-mod.sh`) that
//! could false-positive on the same tokens appearing in a string, comment, or
//! macro body. Parsing each source file and walking its syntax tree makes the
//! check AST-exact instead.
//!
//! This is a lint expressed as a test, not a unit test of runtime logic: it
//! reads the crate's own `src/**/*.rs`, so the checked property is computed
//! from the AST rather than restating a declaration. It lives here because
//! `cargo test` is the harness that reaches both `scripts/preflight.sh` and
//! CI's workspace test job.

use std::fs;
use std::path::PathBuf;

use syn::visit::{Visit, visit_item_mod, visit_visibility};

/// Collects invariant violations while walking one file's syntax tree. `file`
/// is the crate-relative path used in failure messages; `violations` pairs
/// that path with a description of each offending item.
struct InvariantVisitor {
    file: PathBuf,
    violations: Vec<(PathBuf, String)>,
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
                let seg = restricted
                    .path
                    .segments
                    .last()
                    .map(|s| s.ident.to_string())
                    .unwrap_or_default();
                format!("pub({seg})")
            };
            self.violations.push((
                self.file.clone(),
                format!(
                    "scoped visibility `{form}` — use plain `pub` or privatize/restructure the module"
                ),
            ));
        }
        visit_visibility(self, vis);
    }
}

#[test]
fn capabilities_source_invariants_hold() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let src_dir = manifest_dir.join("src");

    let mut violations: Vec<(PathBuf, String)> = Vec::new();

    // Iterative directory walk (explicit work-stack, no recursion) over every
    // `*.rs` under `src/`.
    let mut stack = vec![src_dir];
    while let Some(dir) = stack.pop() {
        let entries =
            fs::read_dir(&dir).unwrap_or_else(|e| panic!("read_dir {}: {e}", dir.display()));
        for entry in entries {
            let entry = entry.expect("read a directory entry");
            let path = entry.path();
            let file_type = entry
                .file_type()
                .expect("resolve a directory entry's file type");
            if file_type.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                let source = fs::read_to_string(&path)
                    .unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
                let ast = syn::parse_file(&source)
                    .unwrap_or_else(|e| panic!("parse {}: {e}", path.display()));
                let rel = path
                    .strip_prefix(&manifest_dir)
                    .unwrap_or(&path)
                    .to_path_buf();
                let mut visitor = InvariantVisitor {
                    file: rel,
                    violations: Vec::new(),
                };
                visitor.visit_file(&ast);
                violations.extend(visitor.violations);
            }
        }
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
