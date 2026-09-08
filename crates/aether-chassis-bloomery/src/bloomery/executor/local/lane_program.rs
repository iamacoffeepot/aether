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
//! arguments, now declared as `[entrypoint]` in the sealed manifest (ADR-0215).
//! A path-only knob could not express that. Words are split on whitespace; a
//! program whose path contains a space needs a wrapper script, which is the
//! same bargain `PATH` itself strikes. Empty means "use the sealed entrypoint";
//! a non-empty value replaces it.
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

use aether_bloomery::{LaneEntrypoint, PipelineManifest};

/// The program a lane dispatch spawns, plus the arguments that precede the
/// transform's own argv.
///
/// A dispatch appends `<command> --out <dir> --nonce <n>` (and the model-lane
/// axes) after [`leading_args`](Self::leading_args), so a stand-in binary sees
/// exactly the argv the real lane does — its own leading words, then the
/// coordinator's. Production reads those leading words from the bloom's sealed
/// [`PipelineManifest`] entrypoint (ADR-0215); [`parse_override`] is the host
/// knob that replaces them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LaneProgram {
    program: String,
    leading_args: Vec<String>,
}

impl Default for LaneProgram {
    fn default() -> Self {
        Self::from_entrypoint(&PipelineManifest::compiled().entrypoint)
    }
}

impl LaneProgram {
    /// The sealed manifest's `[entrypoint]`: the program and the arguments that
    /// precede the work order's own argv.
    #[must_use]
    pub fn from_entrypoint(entrypoint: &LaneEntrypoint) -> Self {
        Self { program: entrypoint.program.clone(), leading_args: entrypoint.args.clone() }
    }

    /// Parse a configured host override — whitespace-separated words, the first
    /// the program and the rest its leading arguments.
    ///
    /// Empty (or all-whitespace) is *no override*: the dispatch reads the sealed
    /// manifest rather than a compiled string. A non-empty value replaces the
    /// entrypoint wholesale, which is what #4727 built this knob for — a test
    /// pointing the real spawn at a stand-in binary.
    #[must_use]
    pub fn parse_override(configured: &str) -> Option<Self> {
        let mut words = configured.split_whitespace().map(str::to_owned);
        let program = words.next()?;
        Some(Self { program, leading_args: words.collect() })
    }

    /// Parse a configured invocation that is known to be non-empty — tests and
    /// a host that has already decided to override.
    ///
    /// An empty value is [`Default`] (the compiled entrypoint) rather than an
    /// unspawnable program, so a caller that meant to override and passed `""`
    /// still spawns something. Production empty-vs-set lives on
    /// [`parse_override`].
    #[must_use]
    pub fn parse(configured: &str) -> Self {
        Self::parse_override(configured).unwrap_or_default()
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
    use aether_bloomery::PipelineManifest;

    use super::LaneProgram;

    #[test]
    fn a_configured_invocation_splits_into_a_program_and_its_leading_arguments() {
        let program = LaneProgram::parse("/tmp/mock-lane --script /tmp/script.json");

        assert_eq!(program.program(), "/tmp/mock-lane");
        assert_eq!(program.leading_args(), ["--script", "/tmp/script.json"]);
    }

    #[test]
    fn the_compiled_entrypoint_is_the_argv_the_deleted_default_spawned() {
        // Tripwire: `DEFAULT_LANE_PROGRAM` used to be `"cargo xtask transform"`,
        // and pre-manifest blooms fold against `PipelineManifest::compiled()`.
        // Those two have to stay the same argv so a record sealed before the
        // file existed still dispatches the program this binary always ran.
        let compiled = LaneProgram::from_entrypoint(&PipelineManifest::compiled().entrypoint);
        assert_eq!(compiled, LaneProgram::default());
        assert_eq!(compiled.program(), "cargo");
        assert_eq!(compiled.leading_args(), ["xtask", "transform"]);
    }

    #[test]
    fn a_cleared_knob_is_no_override_rather_than_a_compiled_string() {
        // Tripwire: empty is the production default, and it must mean "the
        // sealed manifest" rather than silently resurrecting the deleted
        // compiled spawn line. A non-empty value is still the host override
        // a test uses to point the real spawn at a stand-in.
        assert_eq!(LaneProgram::parse_override("   "), None);
        assert_eq!(LaneProgram::parse_override(""), None);
        assert!(LaneProgram::parse_override("/tmp/mock-lane --script /tmp/script.json").is_some());
    }
}
