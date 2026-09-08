//! The headless chassis CLI root (ADR-0090 unit d, issue 1258). [`HeadlessCli`]
//! composes the shared [`CommonOverlay`] full-stack cap bundle with the
//! headless-only tick knob and the source-selecting [`ChassisMeta`] flags. The
//! shared staging / flag-naming / help-forwarding machinery lives in
//! `aether_chassis::cli`.

use aether_chassis::boot::env_only_after_help;
use aether_chassis::chassis_cli;
use aether_chassis::cli::{ChassisMeta, CommonOverlay};
use aether_chassis::tick::TickOverlay;
use clap::Parser;

/// Headless chassis CLI root.
#[derive(Parser, Debug, Default, Clone, aether_substrate::StageArgv)]
#[command(
    name = "aether-headless",
    about = "Headless chassis — std-timer tick driver, nop render. ADR-0035 / ADR-0090.",
    long_about = "Headless chassis — std-timer tick driver, nop render. ADR-0035 / ADR-0090.\n\n\
        Each flag below carries its resolved env key and default in brackets; unset flags fall \
        through to env then the default. For the full source-resolved value of every knob use \
        --print-config, and for this binary's linked caps and build provenance use --describe.",
    after_help = env_only_after_help()
)]
pub struct HeadlessCli {
    #[command(flatten)]
    pub common: CommonOverlay,
    /// Headless tick knob: `--tick-hz`.
    #[command(flatten)]
    pub tick: TickOverlay,

    /// The source-selecting meta flags (`--config` / `--print-config` /
    /// `--describe`); see [`ChassisMeta`].
    #[command(flatten)]
    #[stage(skip)]
    pub meta: ChassisMeta,
}

chassis_cli!(HeadlessCli { CommonOverlay, TickOverlay });
