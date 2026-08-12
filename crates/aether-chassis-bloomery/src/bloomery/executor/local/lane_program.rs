//! Which program a lane dispatch spawns (#4727).
//!
//! The spawn is otherwise fully real — the scratch worktree, the environment
//! scrub, the child, its exit status, the `evidence.json` it leaves on disk —
//! and every one of those steps has broken in production. Testing them means
//! running them, which means the *one* thing a test cannot afford is the program
//! at the end of the argv: `cargo xtask transform` compiles a workspace and
//! forks a model. So the program is resolvable, and a test points it at a mock
//! lane binary that writes the same evidence in milliseconds.
//!
//! The knob is a whole invocation rather than a bare path because the production
//! value *is* one — `cargo xtask transform` is a program plus two leading
//! arguments, and a path-only knob could not express its own default. Words are
//! split on whitespace; a program whose path contains a space needs a wrapper
//! script, which is the same bargain `PATH` itself strikes.
//!
//! [`AETHER_HARNESS_FLEET_HEADLESS_BIN`] is the precedent for the *shape* — a
//! harness pointing a real fork at a stand-in binary — but not for the
//! mechanism: this resolves through the ADR-0090 derive-`Config` path
//! ([`CoordinatorConfig::local_lane_program`]) rather than a naked env read, so
//! it is argv-overridable, appears in the coordinator's config surface, and
//! needs no process-global mutation to set from a test.
//!
//! [`AETHER_HARNESS_FLEET_HEADLESS_BIN`]: https://docs.rs/aether-harness-fleet
//! [`CoordinatorConfig::local_lane_program`]: crate::bloomery::CoordinatorConfig::local_lane_program

use std::process::Command;

/// The default lane invocation: the same portable entrypoint the wrapper
/// workflows run.
pub const DEFAULT_LANE_PROGRAM: &str = "cargo xtask transform";

/// The program a lane dispatch spawns, plus the arguments that precede the
/// transform's own argv.
///
/// A dispatch appends `<command> --out <dir> --nonce <n>` (and the model-lane
/// axes) after [`leading_args`](Self::leading_args), so a stand-in binary sees
/// exactly the argv the real lane does — its own leading words, then the
/// coordinator's.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneProgram {
    program: String,
    leading_args: Vec<String>,
}

impl Default for LaneProgram {
    fn default() -> Self {
        Self::parse(DEFAULT_LANE_PROGRAM)
    }
}

impl LaneProgram {
    /// Parse a configured invocation — whitespace-separated words, the first the
    /// program and the rest its leading arguments.
    ///
    /// An empty (or all-whitespace) value resolves to
    /// [`DEFAULT_LANE_PROGRAM`] rather than to an unspawnable empty program: a
    /// deployment that clears the knob means "the normal lane", and a config
    /// layer that renders an unset string as `""` must not turn every dispatch
    /// into a spawn failure.
    #[must_use]
    pub fn parse(configured: &str) -> Self {
        let mut words = configured.split_whitespace().map(str::to_owned);
        let Some(program) = words.next() else {
            return Self::parse(DEFAULT_LANE_PROGRAM);
        };
        Self { program, leading_args: words.collect() }
    }

    /// The program the dispatch spawns.
    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    /// The arguments that precede the transform's own argv.
    #[must_use]
    pub fn leading_args(&self) -> &[String] {
        &self.leading_args
    }

    /// A [`Command`] for this program with its leading arguments already
    /// applied — the point a dispatch starts building its argv from.
    pub(super) fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.leading_args);
        command
    }
}

#[cfg(test)]
mod tests {
    use super::{DEFAULT_LANE_PROGRAM, LaneProgram};

    #[test]
    fn a_configured_invocation_splits_into_a_program_and_its_leading_arguments() {
        let program = LaneProgram::parse("/tmp/mock-lane --script /tmp/script.json");

        assert_eq!(program.program(), "/tmp/mock-lane");
        assert_eq!(program.leading_args(), ["--script", "/tmp/script.json"]);
    }

    #[test]
    fn the_production_default_is_expressible_in_the_knobs_own_vocabulary() {
        // Tripwire: the reason the knob is a word list rather than a path. If
        // the default ever stops round-tripping through `parse`, the knob can no
        // longer state what it resolves to and a deployment cannot restore it.
        assert_eq!(LaneProgram::parse(DEFAULT_LANE_PROGRAM), LaneProgram::default());
        assert_eq!(LaneProgram::default().program(), "cargo");
        assert_eq!(LaneProgram::default().leading_args(), ["xtask", "transform"]);
    }

    #[test]
    fn a_cleared_knob_resolves_to_the_normal_lane_rather_than_an_unspawnable_program() {
        // Tripwire: a config layer that renders an unset string as `""` would
        // otherwise turn every dispatch into a spawn failure — a coordinator
        // that runs no lanes at all, from a knob nobody set.
        assert_eq!(LaneProgram::parse("   "), LaneProgram::default());
    }
}
