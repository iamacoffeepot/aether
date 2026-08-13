//! The "what forces `run_all`" policy: the exact paths and prefixes whose
//! change invalidates the selection premise, plus the determinator path
//! rules applied before the graph analysis runs.

/// Paths whose change invalidates the selection premise: they shape the
/// dependency graph, the toolchain, the test configuration, or the
/// selection machinery itself. Any hit forces `run_all` before the
/// package-graph analysis runs — which is also what makes the
/// same-graph-twice determinator call in [`crate::affected::select::select`]
/// sound: a path that could change the graph never reaches it.
const RUN_ALL_EXACT: &[&str] =
    &["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "clippy.toml", ".github/workflows/ci.yml"];

/// Directory prefixes with the same run-everything force as
/// [`RUN_ALL_EXACT`]: cargo config, nextest config, and this tool's own
/// crate.
const RUN_ALL_PREFIXES: &[&str] = &[".cargo/", ".config/", "xtask/"];

/// Custom determinator path rules, applied before the crate's bundled
/// defaults (which already ignore `README*` / `LICENSE*` / `.gitignore`
/// and mark-all on the root manifest).
///
/// The ignore list is paths that provably cannot change a Rust build or
/// test outcome: prose, agent/pipeline state, non-`ci.yml` workflows
/// (`ci.yml` itself is screened to `run_all` before rules run), and the
/// `fuzz/` tree, which is its own cargo workspace built only by
/// fuzz-nightly. `approval-policy.toml` is the opposite case — a
/// cross-boundary test input: the `aether-chassis-bloomery` approve tests
/// read it from the repo root, so a change there marks that package (and
/// its reverse closure) changed.
pub(super) const PATH_RULES_TOML: &str = r#"
[[path-rule]]
globs = ["docs/**", "scripts/**", ".claude/**", ".agents/**", ".codex/**", ".github/**", "fuzz/**", ".mcp.json", "CLAUDE.md", "AGENTS.md"]
mark-changed = []

[[path-rule]]
globs = ["approval-policy.toml"]
mark-changed = ["aether-chassis-bloomery"]
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
            "xtask/src/affected.rs",
            ".github/workflows/ci.yml",
        ] {
            assert!(global_screen(&strings(&[path])).is_some(), "{path} must force run_all");
        }

        for path in
            ["crates/aether-kit-commons/src/lib.rs", "crates/aether-kit-commons/Cargo.toml", "docs/guide/testing.md"]
        {
            assert!(global_screen(&strings(&[path])).is_none(), "{path} must not force run_all");
        }
    }
}
