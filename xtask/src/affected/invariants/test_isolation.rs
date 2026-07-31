//! The `tests/`-isolation invariants that make the #4197 narrowing
//! sound.
//!
//! [`crate::affected::test_targets`] drops the reverse-dependency closure
//! for a path under a package's own `tests/` directory, on the premise
//! that such a file compiles into an integration-test binary and nothing
//! links against a test binary. Every way that premise can quietly stop
//! holding is a way for the determinator to under-select — the one
//! failure mode it must never have, and the one that surfaces as a `main`
//! red with no changed-file signal to attribute it to.
//!
//! Three ways exist, and each has a check here:
//!
//! - a source-level path reference into a `tests/` tree
//!   ([`nothing_outside_a_tests_directory_references_a_path_inside_it`]),
//! - a manifest target compiled from a `tests/` tree, or a target reaching
//!   outside its package
//!   ([`every_target_compiles_from_inside_its_own_package`]),
//! - a symlink aliasing one tree into the other, which leaves no textual
//!   reference at all ([`no_symlink_aliases_a_tests_tree`]).
//!
//! `#[path]` spans the first two: it can pull a `tests/` file into a
//! library target and can also reach across a package root, breaking the
//! directory-prefix mapping the determinator resolves changed paths with.

use std::path::{Component, Path, PathBuf};

use cargo_metadata::TargetKind;

use crate::affected::invariants::source::{self, RustSource};
use crate::affected::invariants::workspace::{TEST_DIR, Workspace, package_root};

/// A path reference a Rust source makes to another file on disk.
struct FileReference {
    /// How it was written, for the failure message.
    form: String,
    /// The referenced path, resolved and normalized.
    target: PathBuf,
    /// Byte offset of the reference in the file.
    offset: usize,
}

/// Macros whose argument is a path read at compile time.
const INCLUDE_MACROS: &[&str] = &["include!", "include_str!", "include_bytes!"];

/// Attributes carrying a path read at compile time: `#[path]` names a
/// module file, `#[asset]` (ADR-0163 §2) embeds a file into a component.
const PATH_ATTRIBUTES: &[&str] = &["#[path", "#[asset("];

/// Collect every compile-time file reference `file` makes.
///
/// Two argument forms resolve: a bare string literal, relative to the
/// referring file's directory, and `concat!(env!("CARGO_MANIFEST_DIR"),
/// "…")`, relative to the package root. Anything else is reported rather
/// than skipped — an unresolvable reference is exactly the shape that
/// would let a `tests/` embed through unnoticed.
///
/// The one exception is a `quote!` template (`include_bytes!(#path)` in
/// `aether-actor-derive`), whose `#`-sigil argument is a proc-macro
/// interpolation rather than a path this scan could resolve. The derive's
/// own emitted path is a sibling module file resolved from the invoking
/// `#[actor]` span, never a `tests/` tree; the user-facing generator that
/// *can* name an arbitrary path is `#[asset(path = "…")]`, which is
/// scanned directly.
fn file_references(file: &RustSource, package_root: &Path) -> (Vec<FileReference>, Vec<(usize, String)>) {
    let directory = file.path.parent().unwrap_or(package_root);
    let mut references = Vec::new();
    let mut unresolvable = Vec::new();
    for (offset, form, argument) in path_arguments(&file.code) {
        if argument.trim_start().starts_with('#') {
            continue;
        }
        match resolve_argument(&argument, directory, package_root) {
            Some(target) => references.push(FileReference { form, target, offset }),
            None => unresolvable.push((offset, format!("{form}({argument})"))),
        }
    }
    (references, unresolvable)
}

/// Resolve one compile-time path argument against its two bases.
fn resolve_argument(argument: &str, directory: &Path, package_root: &Path) -> Option<PathBuf> {
    let trimmed = argument.trim();
    let literals = source::string_literals(trimmed);
    let manifest_relative = trimmed.starts_with("concat!") && trimmed.contains("CARGO_MANIFEST_DIR");
    if manifest_relative {
        let tail = literals.last()?.1;
        return Some(normalize(&package_root.join(tail.trim_start_matches('/'))));
    }
    let (_, literal) = *literals.first()?;
    // A bare literal is the whole argument, not one piece of an expression.
    (trimmed.starts_with('"') || trimmed.starts_with('r')).then(|| normalize(&directory.join(literal)))
}

/// Every `(offset, form, argument)` a compile-time path can arrive
/// through: the `include!` family and the path-carrying attributes.
fn path_arguments(code: &str) -> Vec<(usize, String, String)> {
    let mut arguments = Vec::new();
    for macro_name in INCLUDE_MACROS {
        let mut cursor = 0;
        while let Some(offset) = code[cursor..].find(macro_name) {
            let start = cursor + offset;
            cursor = start + macro_name.len();
            // `include_str!` also matches the `include!` search; keep the
            // longest spelling by requiring the preceding byte to end an
            // identifier boundary.
            if code.as_bytes()[..start].last().is_some_and(|byte| byte.is_ascii_alphanumeric() || *byte == b'_') {
                continue;
            }
            let Some(open) = code[cursor..].find('(').map(|offset| cursor + offset) else {
                continue;
            };
            let Some(close) = balanced_paren_end(code, open + 1) else {
                continue;
            };
            arguments.push((start, (*macro_name).to_string(), code[open + 1..close].to_string()));
        }
    }
    for attribute in PATH_ATTRIBUTES {
        let mut cursor = 0;
        while let Some(offset) = code[cursor..].find(attribute) {
            let start = cursor + offset;
            cursor = start + attribute.len();
            let Some(close) = code[start..].find(']').map(|offset| start + offset) else {
                continue;
            };
            let body = &code[start..close];
            let Some(equals) = body.find('=') else {
                continue;
            };
            arguments.push((start, (*attribute).to_string(), body[equals + 1..].trim_end_matches(')').to_string()));
        }
    }
    arguments.sort_by_key(|(offset, _, _)| *offset);
    arguments
}

fn balanced_paren_end(code: &str, from: usize) -> Option<usize> {
    let bytes = code.as_bytes();
    let mut depth = 1_usize;
    for (offset, byte) in bytes.iter().enumerate().skip(from) {
        match byte {
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(offset);
                }
            }
            _ => {}
        }
    }
    None
}

/// Collapse `.` and `..` textually — the referenced file need not exist
/// (a `.stderr` fixture may be generated), so `canonicalize` is not an
/// option.
fn normalize(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                normalized.pop();
            }
            Component::CurDir => {}
            other => normalized.push(other),
        }
    }
    normalized
}

#[test]
fn nothing_outside_a_tests_directory_references_a_path_inside_it() {
    // Tripwire: a changed path under `crates/<pkg>/tests/` selects that
    // package and takes no reverse-dependency closure (#4197), because an
    // integration-test binary has no dependents. A compile-time reference
    // reaching into that tree from anywhere else makes the premise false:
    // the referenced file is then an input to another target, and editing
    // it changes a build the determinator will not select. Cross-crate
    // embeds pointing the other way — into another crate's `src/` or
    // `assets/` — are fine, and the package graph already covers them.
    let workspace = Workspace::load();
    let mut violations = Vec::new();
    let mut unresolvable = Vec::new();
    for package in workspace.packages() {
        let root = package_root(package);
        for file in source::read_all(&source::walk(root).rust_files) {
            let (references, unresolved) = file_references(&file, root);
            for (offset, form) in unresolved {
                unresolvable.push(format!(
                    "    {path}:{line}\n      {form}",
                    path = workspace.relative(&file.path),
                    line = file.line_of(offset),
                    form = form.trim(),
                ));
            }
            for reference in references {
                let Some(owner) = workspace.tests_dir_owner(&reference.target) else {
                    continue;
                };
                if file.path.starts_with(package_root(owner).join(TEST_DIR)) {
                    continue;
                }
                violations.push(format!(
                    "  {path}:{line} references {target} via {form}.\n    \
                     Only a file inside {owner}'s own `tests/` directory may do that. `cargo xtask affected` \
                     narrows a changed `tests/` path to its owning package and takes no reverse-dependency \
                     closure (#4197); this reference makes editing {target} change a build the determinator \
                     will not select.\n    \
                     Fix: move the shared file under `src/` or `assets/` of the package that owns it — an \
                     inbound reference into another crate's non-test tree is already handled by the package \
                     graph.",
                    path = workspace.relative(&file.path),
                    line = file.line_of(reference.offset),
                    target = workspace.relative(&reference.target),
                    form = reference.form,
                    owner = owner.name,
                ));
            }
        }
    }
    // Asserted first: an unresolvable reference means the violation list
    // below is incomplete, so reporting the violations alone would give
    // false confidence.
    assert!(
        unresolvable.is_empty(),
        "a compile-time path reference could not be resolved, so it cannot be proven to stay out of a \
         `tests/` tree:\n\n{}\n\n  \
         Write the path as a bare string literal (resolved against the referring file) or as \
         `concat!(env!(\"CARGO_MANIFEST_DIR\"), \"/…\")` (resolved against the package root) so this check \
         can follow it.\n",
        unresolvable.join("\n\n")
    );
    assert!(
        violations.is_empty(),
        "a compile-time reference reaches into a package's integration-test tree:\n\n{}\n",
        violations.join("\n\n")
    );
}

#[test]
fn no_path_attribute_reaches_across_a_package_root() {
    // Tripwire: `cargo xtask affected` maps a changed file to a package by
    // directory prefix, and nothing else. A `#[path]` reaching across a
    // package root breaks that mapping outright — the file compiles into
    // one package while the determinator attributes every edit to
    // another, so the compiling package goes untested. The workspace has
    // no `#[path]` at all today; this keeps the first one honest.
    let workspace = Workspace::load();
    let mut violations = Vec::new();
    for package in workspace.packages() {
        let root = package_root(package);
        for file in source::read_all(&source::walk(root).rust_files) {
            let (references, _) = file_references(&file, root);
            for reference in references.iter().filter(|reference| reference.form.starts_with("#[path")) {
                if reference.target.starts_with(root) {
                    continue;
                }
                violations.push(format!(
                    "  {path}:{line}: #[path] resolves to {target}, outside {name}'s own package root.\n    \
                     `cargo xtask affected` attributes a changed file to a package by directory prefix, so \
                     an edit to {target} would select the package that owns that directory while the file \
                     actually compiles into {name}.\n    \
                     Fix: keep the module file inside its own package.",
                    path = workspace.relative(&file.path),
                    line = file.line_of(reference.offset),
                    target = workspace.relative(&reference.target),
                    name = package.name,
                ));
            }
        }
    }
    assert!(violations.is_empty(), "a #[path] attribute crosses a package boundary:\n\n{}\n", violations.join("\n\n"));
}

#[test]
fn every_target_compiles_from_inside_its_own_package() {
    // Tripwire: the narrowing assumes a `tests/` tree feeds test targets
    // and nothing else. A manifest can break that with one `path = …` —
    // a `[[bin]]` or `[lib]` pointed at `tests/` makes a `tests/` edit
    // change the package's public surface while #4197 deliberately drops
    // the closure for it, so every dependent goes untested. Reading
    // `src_path` back from cargo covers manifest-declared and
    // autodiscovered targets alike, and the same pass catches a target
    // reaching outside its package root — which would break the
    // directory-prefix attribution the same way a stray `#[path]` does.
    // It also pins the wasm inventory's premise: `discover_components`
    // keys on cdylib lib and `[[example]]` targets, and no such target
    // can sit under `tests/` while this holds.
    let workspace = Workspace::load();
    let mut violations = Vec::new();
    for package in workspace.packages() {
        let root = package_root(package);
        let tests_dir = root.join(TEST_DIR);
        for target in &package.targets {
            let source_path = target.src_path.as_std_path();
            if !source_path.starts_with(root) {
                violations.push(format!(
                    "  {name} target `{target_name}` compiles from {path}, outside its own package root.\n    \
                     `cargo xtask affected` attributes a changed file to a package by directory prefix, so \
                     an edit there would select a different package than the one that compiles it.",
                    name = package.name,
                    target_name = target.name,
                    path = workspace.relative(source_path),
                ));
                continue;
            }
            let kinds = &target.kind;
            if source_path.starts_with(&tests_dir) && kinds.as_slice() != [TargetKind::Test] {
                violations.push(format!(
                    "  {name} target `{target_name}` (kind {kinds:?}) compiles from {path}, under the \
                     package's `tests/` directory.\n    \
                     Only `test`-kind targets may live there. `cargo xtask affected` drops the \
                     reverse-dependency closure for a changed `tests/` path (#4197) on the premise that \
                     nothing links against a test binary; a non-test target built from that tree makes the \
                     edit change {name}'s public surface, and every dependent goes untested.",
                    name = package.name,
                    target_name = target.name,
                    path = workspace.relative(source_path),
                ));
            }
        }
    }
    assert!(
        violations.is_empty(),
        "a cargo target compiles from somewhere the determinator will not attribute to it:\n\n{}\n",
        violations.join("\n\n")
    );
}

#[test]
fn no_symlink_aliases_a_tests_tree() {
    // Tripwire: a symlink is the one way to pull a `tests/` file into a
    // compiled target while leaving no textual reference for the checks
    // above to find — and equally, to make one package's directory tree
    // appear under another's root. The workspace has no symlinks at all;
    // flagging the first one is cheap, and the alternative is a hole the
    // other checks cannot see into.
    let workspace = Workspace::load();
    let mut violations = Vec::new();
    for package in workspace.packages() {
        for link in source::walk(package_root(package)).symlinks {
            violations.push(format!(
                "  {path} is a symlink.\n    \
                 A symlink can alias a `tests/` file into a compiled target, or one package's tree under \
                 another's root, with no source-level reference for the `tests/`-isolation checks to \
                 follow.\n    Fix: replace it with a real file, or move the shared content into the \
                 package that needs it.",
                path = workspace.relative(&link),
            ));
        }
    }
    assert!(violations.is_empty(), "a symlink sits inside a workspace package:\n\n{}\n", violations.join("\n\n"));
}
