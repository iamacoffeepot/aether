//! What a dispatched lane may inherit from the coordinator that dispatched it
//! (#4714).
//!
//! A lane is a child process of the coordinator daemon, so without this it comes
//! up holding the coordinator's entire environment — and the coordinator's
//! environment *is* its configuration (ADR-0090: every knob resolves argv, then
//! env, then a default). A lane that inherits it is not running beside the
//! coordinator, it is running as a second copy of it: pointed at the same
//! journal, holding the same credential, addressing the same repository,
//! resolving the same control and RPC ports.
//!
//! That is not hypothetical. The store-backed integration tests fork the
//! `bloomery` bin, and a forked bin whose `GITHUB_TOKEN` / `AETHER_GITHUB_OWNER`
//! / `AETHER_GITHUB_REPO` resolve is a *configured* bin: its claim registry goes
//! live against the production ref namespace
//! ([`claims_enabled`](crate::source::SourceCapabilityState)), so a test seal
//! loses to whatever bloom actually holds the mainline-admission ref, and a test
//! seal that wins writes a claim ref into it. The journal is the same story one
//! variable over — a test that does not pin `AETHER_STORE_PATH` opens the live
//! `SQLite` file and is one bug away from writing to it. The ports are the third
//! telling: a suite that forks a chassis under a lane resolves the coordinator's
//! own `AETHER_RPC_PORT` / `AETHER_HTTP_PORT` as the port the fork should bind,
//! so the child either fails to bind or dials the live coordinator, and forty
//! tests fail in a full suite that pass instantly from a clean shell.
//!
//! # Constructed, not filtered
//!
//! So the lane's environment is *constructed* rather than pruned: the child
//! starts from an empty environment ([`Command::env_clear`]) and receives only
//! what [`admits_lane_key`] names. That is the same discipline the engine's own
//! forks run under (`aether_fleet::child_env`, ADR-0162), and it is the one that
//! holds against a knob nobody has added yet. A deny list can only take away the
//! names someone thought of: it answers "which of today's variables hurt", and
//! every knob the coordinator grows afterwards leaks by default until somebody
//! notices. An allow list answers "what does a lane need", which is a short,
//! stable, reviewable question — and a new coordinator knob is denied on the day
//! it is written, by nobody's attention.
//!
//! Everything a lane's *work* needs arrives on its argv (`--subject`,
//! `--harness`, `--model`, `--effort`, `--task`, `--nonce`, `--out`) or is set
//! by the dispatch itself, so what crosses here is host surface rather than work
//! surface: the shell and filesystem essentials, the toolchain and build caches,
//! the platform handles a forked chassis needs, and the harness credential the
//! model arms resolve ambiently (ADR-0150).
//!
//! `CARGO_TARGET_DIR` is not among them, because the dispatch answers it for
//! itself: `export_build_env` sets the lane slot's own target directory (#4912)
//! right after this runs, so whatever the coordinator's boot environment named
//! never reaches a lane — not because it was taken away, but because the
//! dispatch names a better one.

use std::env;
use std::ffi::{OsStr, OsString};
use std::process::Command;

/// Exact environment-variable names a lane inherits from the coordinator.
///
/// Every entry is host surface, and every one is here because taking it away
/// breaks a lane rather than because it is harmless:
///
/// - `PATH` is what resolves the lane's own program (`cargo`) and everything it
///   forks — `git`, `gh`, `rustc`, the harness CLI. An empty one fails the spawn
///   itself, so this is the entry whose absence is loudest.
/// - `HOME` roots cargo's registry, rustup's toolchains, git's global
///   configuration, and the harness CLI's ambient login (ADR-0150).
/// - `TMPDIR`, `USER`, `LOGNAME`, `SHELL` are the shell and filesystem
///   essentials the toolchain and the gates' own subprocesses read.
/// - `LANG` and `TZ` set the locale and timezone a gate's output and a run's
///   evidence timestamps are rendered in.
/// - `RUST_BACKTRACE` is a host asking for backtraces; a lane's failure evidence
///   is exactly where it wanted them.
/// - `CARGO_HOME`, `RUSTUP_HOME`, `RUSTUP_TOOLCHAIN` locate the toolchain and
///   the registry cache. Clearing them costs every lane a cold download and a
///   cold compile for nothing.
/// - `CARGO_BUILD_JOBS` is the host's build-parallelism cap, which stands when
///   the dispatch states none of its own.
/// - `SSL_CERT_FILE` / `SSL_CERT_DIR` and the proxy variables in both cases are
///   how a crates.io fetch and the harness CLI's own API calls reach the network
///   on a host that does not use the system defaults.
/// - `DISPLAY`, `WAYLAND_DISPLAY`, `XAUTHORITY` are the windowing handles: a
///   lane runs the workspace's own suite, which forks chassis children, and
///   `aether_fleet::child_env` grants a forked engine exactly these.
/// - `AETHER_LANE_SCRATCH` is the one `AETHER_*` name that is *lane* contract
///   rather than coordinator configuration — the volume a model lane's child
///   builds its throwaway target trees on (`xtask`'s `transform::scratch`),
///   which the operator sets in the coordinator's environment file precisely so
///   it reaches the lane. The prefix scrub this replaced dropped it, which put
///   every lane's build tree back on the root filesystem it was pointed away
///   from.
/// - `ANTHROPIC_API_KEY`, `CLAUDE_CODE_OAUTH_TOKEN`, `GROK_CODE_XAI_API_KEY` are
///   the ambient harness credential (ADR-0150): the lane holds no secret of its
///   own, and a host that authenticates by key rather than by login resolves it
///   from here. A logged-in host resolves its own under `HOME` instead.
///
/// `GITHUB_TOKEN` is deliberately absent, and is the one omission worth stating
/// out loud: it is the coordinator's credential, ADR-0152 keeps the child out of
/// staging, committing, and holding one, and it is half of what arms the live
/// claim registry.
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
    "CARGO_HOME",
    "RUSTUP_HOME",
    "RUSTUP_TOOLCHAIN",
    "CARGO_BUILD_JOBS",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "http_proxy",
    "https_proxy",
    "no_proxy",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "DISPLAY",
    "WAYLAND_DISPLAY",
    "XAUTHORITY",
    "AETHER_LANE_SCRATCH",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "GROK_CODE_XAI_API_KEY",
];

/// Name prefixes whose whole family a lane inherits.
///
/// A family rather than a name where the vocabulary belongs to somebody else and
/// enumerating it would be a guess that goes stale: the locale categories
/// (`LC_`), the base directories cargo, rustup, and `gh` read their state from
/// (`XDG_`), the compiler cache's own knobs (`SCCACHE_` — where the cache lives
/// and how large it may grow stay the host's, which `xtask`'s
/// `transform::sccache` says in as many words), and the graphics driver families
/// a forked chassis selects its adapter through (`VK_`, `MESA_`, `LIBGL_`,
/// `__GL_`, `__EGL_`, `DRI_`).
///
/// No `AETHER_` prefix appears here, and none can: the one lane-contract
/// `AETHER_*` name is spelled out in `ALLOWLISTED_NAMES`, so a family match
/// can never admit a coordinator knob by accident.
const ALLOWLISTED_PREFIXES: &[&str] = &["LC_", "XDG_", "SCCACHE_", "VK_", "MESA_", "LIBGL_", "__GL_", "__EGL_", "DRI_"];

/// Whether a lane child may inherit `key` from the coordinator — an exact match
/// in `ALLOWLISTED_NAMES` or a member of an `ALLOWLISTED_PREFIXES` family.
///
/// A non-UTF-8 name matches nothing (the allow list is all ASCII) and is
/// therefore denied, which is the safe direction.
///
/// Public because it is the lane boundary's stated contract: a scenario that
/// reads what a real lane child came up holding asserts against this rather than
/// against a second copy of the list.
#[must_use]
pub fn admits_lane_key(key: &OsStr) -> bool {
    key.to_str().is_some_and(|name| {
        ALLOWLISTED_NAMES.contains(&name) || ALLOWLISTED_PREFIXES.iter().any(|family| name.starts_with(family))
    })
}

/// Clear `command`'s environment and rebuild it from `inherited`, keeping only
/// what [`admits_lane_key`] admits.
///
/// Takes the inherited variables as an argument rather than reading the
/// process's own environment, so the policy is exercisable against a stated
/// environment instead of whichever one the test runner happens to carry.
///
/// The caller applies the dispatch's own variables (`CARGO_TARGET_DIR`,
/// `CARGO_BUILD_JOBS`) on top of the constructed base.
pub(super) fn construct_lane_env(command: &mut Command, inherited: impl IntoIterator<Item = (OsString, OsString)>) {
    command.env_clear();
    for (key, value) in inherited.into_iter().filter(|(key, _)| admits_lane_key(key)) {
        command.env(key, value);
    }
}

/// The coordinator's own environment — the production call's argument to
/// [`construct_lane_env`].
///
/// Enumerating the environment is not the naked-env-read the derive-`Config`
/// path exists to replace: nothing here is read *as* configuration, and the
/// variables are collected only to decide which of them to hand on.
pub(super) fn inherited_env() -> impl Iterator<Item = (OsString, OsString)> {
    env::vars_os()
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    use std::ffi::{OsStr, OsString};
    use std::process::Command;

    use super::{admits_lane_key, construct_lane_env};

    /// The environment a lane would come up holding, given a coordinator holding
    /// `inherited`. `Command`'s override map is keyed, so this reads back sorted
    /// rather than in the caller's order.
    fn constructed(inherited: &[(&str, &str)]) -> BTreeMap<String, String> {
        let mut command = Command::new("cargo");
        construct_lane_env(
            &mut command,
            inherited.iter().map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );
        command
            .get_envs()
            .filter_map(|(key, value)| {
                Some((key.to_string_lossy().into_owned(), value?.to_string_lossy().into_owned()))
            })
            .collect()
    }

    #[test]
    fn the_coordinators_own_configuration_never_reaches_the_lane() {
        // Tripwire: the whole defect (#4714) and its escalation. A lane that
        // keeps `AETHER_STORE_PATH` runs the store-backed tests against the live
        // journal; one that keeps the GitHub connection arms the forked bin's
        // claim registry against the production ref namespace — which
        // `GITHUB_TOKEN` alone is half of, and which a prefix-only rule could
        // never have caught, since that name carries no `AETHER_` prefix; and
        // one that keeps the coordinator's ports hands every chassis a suite
        // forks under it the live control ports as its own to bind.
        assert!(
            constructed(&[
                ("AETHER_STORE_PATH", "/var/bloomery/bloomery.db"),
                ("AETHER_GITHUB_OWNER", "iamacoffeepot"),
                ("AETHER_ARTIFACTS_ROOT", "/var/bloomery/artifacts"),
                ("AETHER_RPC_PORT", "8909"),
                ("AETHER_HTTP_PORT", "8910"),
                ("GITHUB_TOKEN", "gho_live"),
            ])
            .is_empty(),
        );
    }

    #[test]
    fn a_knob_nobody_has_written_yet_is_denied_without_anybody_noticing() {
        // Tripwire: the property a deny list cannot have, and the reason this is
        // an allow list. A coordinator knob invented next month must not reach a
        // lane on the strength of nobody having added it to a scrub — and
        // neither must the unrelated junk a supervisor, a shell profile, or an
        // outer test runner leaves in the coordinator's environment.
        assert!(
            constructed(&[
                ("AETHER_BLOOMERY_KNOB_NOBODY_HAS_WRITTEN_YET", "1"),
                ("BLOOMERY_HOME", "/home/operator/aether"),
                ("CARGO_MANIFEST_DIR", "/home/operator/aether/crates/aether-chassis-bloomery"),
            ])
            .is_empty(),
        );
    }

    #[test]
    fn the_environment_the_lane_runs_on_survives_with_its_values() {
        // Tripwire: the allow list has to be a scalpel in the other direction
        // too. A lane spawns `cargo`, which needs `PATH`, `HOME`, and its
        // toolchain variables to run at all, so an over-broad clear turns every
        // dispatch into a spawn failure — a louder bug than the one being fixed,
        // but a bug either way. The build caches are the deliberate keep: a
        // cache is not durable coordinator state, and taking `CARGO_HOME` or the
        // toolchain away costs every lane a cold compile for nothing. Values
        // travel, not just names — a `PATH` that arrived empty would resolve no
        // program.
        let lane = constructed(&[
            ("PATH", "/home/operator/.cargo/bin:/usr/bin"),
            ("HOME", "/home/operator"),
            ("CARGO_HOME", "/home/operator/.cargo"),
            ("RUSTUP_TOOLCHAIN", "1.90.0"),
            ("SCCACHE_DIR", "/mnt/scratch/sccache"),
            ("LC_ALL", "en_US.UTF-8"),
            ("GROK_CODE_XAI_API_KEY", "xai-ambient"),
        ]);

        assert_eq!(lane.get("PATH").map(String::as_str), Some("/home/operator/.cargo/bin:/usr/bin"));
        assert_eq!(lane.len(), 7, "every host variable a lane builds and authenticates with crossed: {lane:?}");
    }

    #[test]
    fn the_lanes_own_scratch_root_crosses_where_the_coordinators_knobs_do_not() {
        // Tripwire: the one `AETHER_*` name that is lane contract rather than
        // coordinator configuration. The prefix scrub this replaced took it
        // away, which silently put every model lane's throwaway build tree back
        // on the root filesystem the operator had pointed it off of — the exact
        // failure `AETHER_LANE_SCRATCH` exists to prevent.
        assert!(admits_lane_key(OsStr::new("AETHER_LANE_SCRATCH")));
        assert!(!admits_lane_key(OsStr::new("AETHER_LANE_SCRATCH_SOMETHING_ELSE")), "it is a name, not a family");
        assert!(!admits_lane_key(OsStr::new("AETHER_STORE_PATH")));
    }
}
