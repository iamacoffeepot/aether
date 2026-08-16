//! What a dispatched lane may inherit from the coordinator that dispatched it
//! (#4714).
//!
//! A lane is a child process of the coordinator daemon, so without this it comes
//! up holding the coordinator's entire environment — and the coordinator's
//! environment *is* its configuration (ADR-0090: every knob resolves argv, then
//! env, then a default). A lane that inherits it is not running beside the
//! coordinator, it is running as a second copy of it: pointed at the same
//! journal, holding the same credential, addressing the same repository.
//!
//! That is not hypothetical. The store-backed integration tests fork the
//! `bloomery` bin, and a forked bin whose `GITHUB_TOKEN` / `AETHER_GITHUB_OWNER`
//! / `AETHER_GITHUB_REPO` resolve is a *configured* bin: its claim registry goes
//! live against the production ref namespace
//! ([`claims_enabled`](crate::source::SourceCapabilityState)), so a test seal
//! loses to whatever bloom actually holds the mainline-admission ref, and a test
//! seal that wins writes a claim ref into it. The journal is the same story one
//! variable over — a test that does not pin `AETHER_STORE_PATH` opens the live
//! `SQLite` file and is one bug away from writing to it.
//!
//! So the lane's environment is scrubbed of the coordinator's own configuration
//! surface rather than of one variable known to have hurt: the surface is what
//! the daemon exports, and enumerating today's damage would leave the next knob
//! to leak on its own. Everything the lane genuinely needs arrives on its argv
//! (`--subject`, `--harness`, `--model`, `--effort`, `--task`, `--nonce`,
//! `--out`, `--seeded`) or is set by the gate itself, so nothing scrubbed here is load-
//! bearing for the run.
//!
//! Build caches are deliberately not scrubbed. None of them names durable
//! coordinator state — a cache's worst case is a rebuild, not a mutation of
//! production — and clearing `CARGO_HOME`, the toolchain vars, or the host's
//! `SCCACHE_*` knobs would cost every lane a cold compile for nothing.
//!
//! `CARGO_TARGET_DIR` is the one the dispatch answers for itself rather than by
//! policy here: `export_build_env` overwrites it with the lane slot's own target
//! directory (#4912) right after this scrub runs, so whatever the coordinator's
//! boot environment named never reaches a lane — not because it was taken away,
//! but because the dispatch names a better one.

use std::env;
use std::ffi::OsString;
use std::process::Command;

/// The prefix every knob the bloomery chassis resolves is spelled under
/// (ADR-0090 `env_prefix`) — `AETHER_STORE_PATH`, `AETHER_ARTIFACTS_ROOT`,
/// `AETHER_GITHUB_*`, `AETHER_HTTP_PORT`, and the rest.
const COORDINATOR_ENV_PREFIX: &str = "AETHER_";

/// Coordinator configuration spelled outside the prefix. `GITHUB_TOKEN` is the
/// conventional name `GithubConnectionConfig::token` pins rather than renaming under
/// `AETHER_GITHUB_`, so a prefix scrub alone would leave the one variable that
/// is both a live credential and half of what arms the claim registry.
const COORDINATOR_ENV_KEYS: [&str; 1] = ["GITHUB_TOKEN"];

/// Whether `key` names a knob the coordinator configures itself with, and so
/// must not reach a lane.
fn is_coordinator_env(key: &str) -> bool {
    key.starts_with(COORDINATOR_ENV_PREFIX) || COORDINATOR_ENV_KEYS.contains(&key)
}

/// Remove every coordinator-owned variable in `inherited` from `command`'s
/// environment.
///
/// Takes the inherited keys as an argument rather than reading the process's own
/// environment, so the policy is exercisable against a stated environment
/// instead of whichever one the test runner happens to carry.
pub(super) fn scrub_coordinator_env(command: &mut Command, inherited: impl Iterator<Item = OsString>) {
    for key in inherited.filter(|key| key.to_str().is_some_and(is_coordinator_env)) {
        command.env_remove(key);
    }
}

/// The keys the coordinator's *own* environment contributes to a scrub — the
/// production call's argument to [`scrub_coordinator_env`].
///
/// Enumerating the environment is not the naked-env-read the derive-`Config`
/// path exists to replace: nothing here is read *as* configuration, and the keys
/// are collected only to take them away.
pub(super) fn inherited_keys() -> impl Iterator<Item = OsString> {
    env::vars_os().map(|(key, _)| key)
}

#[cfg(test)]
mod tests {
    use std::ffi::OsString;
    use std::process::Command;

    use super::scrub_coordinator_env;

    /// The keys a scrubbed command removes, sorted — `Command`'s override map is
    /// keyed, so the removals come back in its order rather than the caller's.
    fn removed(inherited: &[&str]) -> Vec<String> {
        let mut command = Command::new("cargo");
        scrub_coordinator_env(&mut command, inherited.iter().map(OsString::from));
        let mut keys: Vec<String> = command
            .get_envs()
            .filter(|(_, value)| value.is_none())
            .map(|(key, _)| key.to_string_lossy().into_owned())
            .collect();
        keys.sort();
        keys
    }

    #[test]
    fn the_coordinators_own_configuration_never_reaches_the_lane() {
        // Tripwire: the whole defect (#4714). A lane that keeps
        // `AETHER_STORE_PATH` runs the store-backed tests against the live
        // journal, and one that keeps the GitHub connection arms the forked
        // bin's claim registry against the production ref namespace — which is
        // what actually failed six tests, and which a prefix-only scrub would
        // still let through because `GITHUB_TOKEN` carries no `AETHER_` prefix.
        assert_eq!(
            removed(&["AETHER_STORE_PATH", "AETHER_GITHUB_OWNER", "AETHER_ARTIFACTS_ROOT", "GITHUB_TOKEN"]),
            ["AETHER_ARTIFACTS_ROOT", "AETHER_GITHUB_OWNER", "AETHER_STORE_PATH", "GITHUB_TOKEN"],
        );
    }

    #[test]
    fn the_environment_the_lane_runs_on_survives() {
        // Tripwire: the scrub has to be a scalpel. A lane spawns `cargo`, which
        // needs `PATH`, `HOME`, and its own toolchain vars to run at all, so a
        // broader sweep would turn every dispatch into a spawn failure — a
        // louder bug than the one being fixed, but a bug either way. The build
        // caches are the deliberate keep: a cache is not durable coordinator
        // state, and taking `CARGO_HOME` or the toolchain vars away would cost
        // every lane a cold compile for nothing.
        assert!(
            removed(&["PATH", "HOME", "CARGO_TARGET_DIR", "CARGO_HOME", "RUSTUP_TOOLCHAIN", "GITHUB_REPOSITORY"])
                .is_empty()
        );
    }
}
