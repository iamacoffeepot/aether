//! `smoke_dir!` — proc-macro that scans a directory at expansion
//! time and emits one `#[test] fn ...` per `.yml` file. Each
//! generated test embeds the script via `include_str!` (so cargo
//! tracks file content changes), boots a fresh `TestBench`, runs
//! the script, and asserts the report passed.
//!
//! The `include_str!` per-file path tracks individual file content,
//! but cargo doesn't track *new* files added to the directory —
//! adding a YAML file requires touching the source that calls
//! `smoke_dir!` to retrigger expansion. That's the v1 trade-off;
//! `proc_macro::tracked_path::path` (unstable) would close the gap
//! once it stabilizes.

use proc_macro::TokenStream;
use quote::{format_ident, quote};
use std::fs;
use std::path::{Path, PathBuf};

/// `smoke_dir!("relative/dir")` — expand to one `#[test]` per
/// `.yml` file under `relative/dir` (relative to `CARGO_MANIFEST_DIR`).
/// Sub-directories are ignored — keep the layout flat in v1.
///
/// Generated test names are `smoke_<file_stem>` with non-identifier
/// characters replaced by underscores. Two files whose stems collapse
/// to the same identifier produce a duplicate-symbol compile error
/// the author resolves by renaming.
#[proc_macro]
pub fn smoke_dir(input: TokenStream) -> TokenStream {
    let dir_arg = match parse_string_literal(input) {
        Ok(s) => s,
        Err(msg) => return compile_error(&msg),
    };
    let manifest_dir = match std::env::var("CARGO_MANIFEST_DIR") {
        Ok(s) => PathBuf::from(s),
        Err(e) => return compile_error(&format!("CARGO_MANIFEST_DIR unavailable: {e}")),
    };
    let dir = manifest_dir.join(&dir_arg);
    let yaml_files = match collect_yaml_files(&dir) {
        Ok(v) => v,
        Err(msg) => return compile_error(&msg),
    };

    let tests = yaml_files.iter().map(|path| emit_test(path, &manifest_dir));
    quote!(#(#tests)*).into()
}

fn parse_string_literal(input: TokenStream) -> Result<String, String> {
    let s = input.to_string();
    let trimmed = s.trim();
    if trimmed.len() < 2 || !trimmed.starts_with('"') || !trimmed.ends_with('"') {
        return Err(format!(
            "smoke_dir!(...) expects a single string literal, got `{s}`"
        ));
    }
    Ok(trimmed[1..trimmed.len() - 1].to_owned())
}

fn collect_yaml_files(dir: &Path) -> Result<Vec<PathBuf>, String> {
    if !dir.exists() {
        return Err(format!(
            "smoke_dir!: directory does not exist: {}",
            dir.display()
        ));
    }
    let entries = fs::read_dir(dir).map_err(|e| format!("read_dir {}: {e}", dir.display()))?;
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_file()
            && path
                .extension()
                .is_some_and(|ext| ext == "yml" || ext == "yaml")
        {
            out.push(path);
        }
    }
    out.sort(); // Stable test order across platforms.
    Ok(out)
}

fn emit_test(path: &Path, manifest_dir: &Path) -> proc_macro2::TokenStream {
    let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("smoke");
    let test_name = format_ident!("smoke_{}", sanitize_ident(stem));
    // Path emitted to `include_str!` is relative to `CARGO_MANIFEST_DIR`
    // (the crate's source tree) so the macro works regardless of where
    // cargo invokes it.
    let rel = path
        .strip_prefix(manifest_dir)
        .map(|p| p.to_string_lossy().into_owned())
        .unwrap_or_else(|_| path.to_string_lossy().into_owned());
    quote! {
        #[test]
        fn #test_name() {
            let yaml = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/", #rel));
            let report = aether_smoke::run_yaml_str(yaml).expect("run smoke");
            assert!(
                report.passed,
                "smoke {:?} failed:\n{:#?}",
                report.script_name, report.steps,
            );
        }
    }
}

/// Replace characters that aren't valid in Rust identifiers with '_'.
/// Doesn't try to be clever about leading digits — the generated name
/// is always prefixed with `smoke_` so it can't start with a digit.
fn sanitize_ident(s: &str) -> String {
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

fn compile_error(msg: &str) -> TokenStream {
    quote! {
        compile_error!(#msg);
    }
    .into()
}
