//! Constructed child environments for forked processes (ADR-0162).
//!
//! A hub- or tunnel-spawned child's environment is *constructed*, never
//! inherited. [`isolate_child_environment`] clears the environment
//! ([`Command::env_clear`]) and copies from the parent only the keys the
//! [allowlist](is_allowlisted_key) admits — third-party and platform surface a
//! child legitimately needs (the locale, proxy, cert-bundle, windowing, and
//! GPU/audio driver variables, plus `PATH` / `HOME` / `USER` and friends). The
//! caller then applies the child's addressed config — argv flags and any
//! explicit per-fork `env` injection — on top of the cleared base.
//!
//! Aether config never appears on the allowlist: no `AETHER_*` key is ever
//! copied, so it crosses a process boundary only through argv or an explicit
//! injection. This is the enforcement half of ADR-0162 — nothing rides the
//! ambient channel — and it holds at every fork depth, since the child's
//! environment carries only the platform surface a grandchild would also
//! legitimately inherit.

use std::env;
use std::ffi::{OsStr, OsString};
use std::process::Command;

/// Exact environment-variable names copied from the parent into a forked
/// child's environment. Third-party and platform surface only — the shell /
/// filesystem essentials (`PATH`, `HOME`, `TMPDIR`, `USER`, `LOGNAME`,
/// `SHELL`), locale and timezone (`LANG`, `TZ`), the Rust backtrace toggle,
/// the windowing handles (`DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY`), the
/// system cert bundle (`SSL_CERT_FILE`, `SSL_CERT_DIR`), the proxy variables
/// in both cases, and `GITHUB_TOKEN` (the one named non-`AETHER_` config key
/// in the workspace — the bloomery mirror's derive-declared credential). No
/// `AETHER_*` name appears here by construction.
const ALLOWLISTED_NAMES: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "USER",
    "LOGNAME",
    "SHELL",
    "LANG",
    "TZ",
    "RUST_BACKTRACE",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "GITHUB_TOKEN",
];

/// Environment-variable name prefixes whose whole family is copied from the
/// parent: the locale categories (`LC_`), the XDG base-dir set (`XDG_`), the
/// graphics driver families (`VK_` / `MESA_` / `LIBGL_` / `__GL_` / `__EGL_` /
/// `DRI_`), and the audio driver families (`PULSE_` / `PIPEWIRE_` / `ALSA_`) a
/// windowing or audio child reads. No `AETHER_*` prefix appears here by
/// construction.
const ALLOWLISTED_PREFIXES: &[&str] =
    &["LC_", "XDG_", "VK_", "MESA_", "LIBGL_", "__GL_", "__EGL_", "DRI_", "PULSE_", "PIPEWIRE_", "ALSA_"];

/// True when `key` names environment a forked child may inherit from the
/// parent — an exact match in [`ALLOWLISTED_NAMES`] or a member of a family in
/// [`ALLOWLISTED_PREFIXES`]. A non-UTF-8 key never matches (the allowlist is
/// all ASCII), so it is dropped, which is the safe default. No `AETHER_*` key
/// matches, so aether config never rides the inherited channel.
#[must_use]
pub fn is_allowlisted_key(key: &OsStr) -> bool {
    let Some(name) = key.to_str() else {
        return false;
    };
    ALLOWLISTED_NAMES.contains(&name) || ALLOWLISTED_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// Clear `command`'s environment and repopulate it from the parent with only
/// the [allowlisted](is_allowlisted_key) keys (ADR-0162). The caller applies
/// the child's addressed config (argv flags, explicit `env` injections) on top
/// of the cleared base.
pub fn isolate_child_environment(command: &mut Command) {
    // Process-level env *enumeration* to construct the child environment — not
    // a capability reading its own config, so it is the sanctioned
    // process-wiring read rather than an `AETHER_*` config pull.
    #[allow(clippy::disallowed_methods)]
    let inherited: Vec<(OsString, OsString)> = env::vars_os().collect();
    build_child_environment(command, inherited);
}

/// Clear `command`'s environment and copy in every allowlisted key from
/// `inherited`. Split from [`isolate_child_environment`] so the allowlist
/// filter is exercised against a synthetic parent environment without touching
/// the process environment.
fn build_child_environment<I>(command: &mut Command, inherited: I)
where
    I: IntoIterator<Item = (OsString, OsString)>,
{
    command.env_clear();
    for (key, value) in inherited {
        if is_allowlisted_key(&key) {
            command.env(key, value);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{build_child_environment, is_allowlisted_key};
    use std::collections::HashMap;
    use std::ffi::{OsStr, OsString};
    use std::process::Command;

    /// Tripwire (ADR-0162): the allowlist admits exactly its two match classes
    /// — exact names and prefix families — and never an `AETHER_*` key. Drifts
    /// if a class stops matching (a child would lose platform surface it needs)
    /// or a stray `AETHER_*`/unrelated key starts matching (aether config or
    /// junk would leak back onto the inherited channel the ADR closes).
    #[test]
    fn allowlist_admits_only_platform_surface() {
        // Exact-name members.
        assert!(is_allowlisted_key(OsStr::new("PATH")));
        assert!(is_allowlisted_key(OsStr::new("HOME")));
        assert!(is_allowlisted_key(OsStr::new("GITHUB_TOKEN")));
        // Prefix-family members.
        assert!(is_allowlisted_key(OsStr::new("LC_ALL")));
        assert!(is_allowlisted_key(OsStr::new("XDG_RUNTIME_DIR")));
        assert!(is_allowlisted_key(OsStr::new("MESA_LOADER_DRIVER_OVERRIDE")));
        // No aether config and no unrelated junk.
        assert!(!is_allowlisted_key(OsStr::new("AETHER_RPC_PORT")));
        assert!(!is_allowlisted_key(OsStr::new("AETHER_BINARY_BOOTSTRAP")));
        assert!(!is_allowlisted_key(OsStr::new("RANDOM_UNRELATED_VAR")));
        // A bare prefix without the `_` boundary is not a family member.
        assert!(!is_allowlisted_key(OsStr::new("LCD")));
    }

    /// Tripwire (ADR-0162): a constructed child environment carries the
    /// allowlisted parent keys and the explicit injection applied on top, and
    /// neither an `AETHER_*` key nor unrelated junk from the parent. Drifts if
    /// the clear-and-copy stops filtering (junk/aether would survive) or drops
    /// an allowlisted key or the injection. Read back through
    /// `Command::get_envs`, whose entries are the constructed override set.
    #[test]
    fn constructed_environment_keeps_allowlisted_and_injected_only() {
        let parent = [
            ("HOME", "/home/tester"),
            ("LC_ALL", "en_US.UTF-8"),
            ("AETHER_RPC_PORT", "8901"),
            ("RANDOM_UNRELATED_VAR", "junk"),
        ]
        .into_iter()
        .map(|(key, value)| (OsString::from(key), OsString::from(value)));

        let mut command = Command::new("true");
        build_child_environment(&mut command, parent);
        // The addressed injection the caller applies on top of the cleared base
        // (the aether-mcp child's port is delivered this way).
        command.env("AETHER_MCP_PORT", "8891");

        let constructed: HashMap<OsString, Option<OsString>> =
            command.get_envs().map(|(key, value)| (key.to_owned(), value.map(ToOwned::to_owned))).collect();

        // Allowlisted parent keys survive with their values.
        assert_eq!(constructed.get(OsStr::new("HOME")), Some(&Some(OsString::from("/home/tester"))));
        assert_eq!(constructed.get(OsStr::new("LC_ALL")), Some(&Some(OsString::from("en_US.UTF-8"))));
        // The explicit injection is present on top of the cleared base.
        assert_eq!(constructed.get(OsStr::new("AETHER_MCP_PORT")), Some(&Some(OsString::from("8891"))));
        // The parent's aether config and unrelated junk were not copied.
        assert!(!constructed.contains_key(OsStr::new("AETHER_RPC_PORT")), "parent AETHER_* must not be inherited");
        assert!(
            !constructed.contains_key(OsStr::new("RANDOM_UNRELATED_VAR")),
            "unrelated parent junk must not survive"
        );
    }
}
