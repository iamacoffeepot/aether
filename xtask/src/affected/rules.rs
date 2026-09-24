//! The "what forces `run_all`" policy: the exact paths and prefixes whose
//! change invalidates the selection premise, plus the determinator path
//! rules applied before the graph analysis runs.

/// Paths whose change invalidates the selection premise: they shape the
/// dependency graph, the toolchain, the test configuration, or the
/// selection machinery itself. Any hit forces `run_all` before the
/// package-graph analysis runs — which is also what makes the
/// same-graph-twice determinator call in [`crate::affected::select::select`]
/// sound: a path that could change the graph never reaches it.
///
/// The xtask entries are the crate's manifest, the binary entry that
/// dispatches every command, and the two helpers [`crate::affected`] and
/// [`crate::dist`] share when they compute `run_all` / `wasm_needed` and
/// produce the artifacts the suite reads. `build_wasm.rs` is the last of
/// that class: it is [`crate::dist`] with the chassis binaries dropped, so
/// it builds the component wasm every `require_wasm` gate opens by path and
/// belongs here beside the `xtask/src/dist/` prefix below (#6055). A change
/// elsewhere in xtask resolves through the package graph like any other
/// crate (#5928).
const RUN_ALL_EXACT: &[&str] = &[
    "Cargo.toml",
    "Cargo.lock",
    "rust-toolchain.toml",
    "clippy.toml",
    ".github/workflows/ci.yml",
    "xtask/Cargo.toml",
    "xtask/src/main.rs",
    "xtask/src/cargo.rs",
    "xtask/src/inventory.rs",
    "xtask/src/build_wasm.rs",
];

/// Directory prefixes with the same run-everything force as
/// [`RUN_ALL_EXACT`]: cargo config, nextest config, and the xtask modules
/// that compute the selection or feed the suite.
const RUN_ALL_PREFIXES: &[&str] = &[".cargo/", ".config/", "xtask/src/affected/", "xtask/src/dist/"];

/// Custom determinator path rules, applied before the crate's bundled
/// defaults (which already ignore `README*` / `LICENSE*` / `.gitignore`
/// and mark-all on the root manifest).
///
/// The ignore list is paths that provably cannot change a Rust build or
/// test outcome: prose, agent/pipeline state, non-`ci.yml` workflows
/// (`ci.yml` itself is screened to `run_all` before rules run), and the
/// `fuzz/` tree, which is its own cargo workspace built only by
/// fuzz-nightly.
pub(super) const PATH_RULES_TOML: &str = r#"
[[path-rule]]
globs = ["docs/**", "scripts/**", ".claude/**", ".agents/**", ".codex/**", ".github/**", "fuzz/**", ".mcp.json", "CLAUDE.md", "AGENTS.md"]
mark-changed = []
"#;

/// Screen for paths that force the full suite, returning the first hit.
pub fn global_screen(changed: &[String]) -> Option<&str> {
    changed
        .iter()
        .map(String::as_str)
        .find(|path| RUN_ALL_EXACT.contains(path) || RUN_ALL_PREFIXES.iter().any(|prefix| path.starts_with(prefix)))
}

#[cfg(test)]
mod tests {
    use super::global_screen;

    fn strings(names: &[&str]) -> Vec<String> {
        names.iter().map(|name| (*name).to_string()).collect()
    }

    #[test]
    fn global_screen_catches_graph_shaping_paths() {
        // A missed screen entry would let a graph-reshaping or
        // config-reshaping change run a stale subset.
        for path in [
            "Cargo.lock",
            "Cargo.toml",
            "rust-toolchain.toml",
            ".config/nextest.toml",
            ".cargo/config.toml",
            ".github/workflows/ci.yml",
            "xtask/Cargo.toml",
            "xtask/src/main.rs",
            "xtask/src/cargo.rs",
            "xtask/src/inventory.rs",
            "xtask/src/affected/rules.rs",
            "xtask/src/dist/mod.rs",
            "xtask/src/build_wasm.rs",
        ] {
            assert!(global_screen(&strings(&[path])).is_some(), "{path} must force run_all");
        }

        for path in [
            "crates/aether-kit/src/lib.rs",
            "crates/aether-kit/Cargo.toml",
            "docs/guide/testing.md",
            "xtask/src/bloom/roll/coverage.rs",
        ] {
            assert!(global_screen(&strings(&[path])).is_none(), "{path} must not force run_all");
        }
    }
}
