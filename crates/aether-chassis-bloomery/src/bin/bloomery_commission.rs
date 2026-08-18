//! The `bloomery-commission` binary: author and query commissions through the
//! coordinator control API. A sibling of `bloomery`, never a subcommand of it —
//! a bare `bloomery` still starts the daemon.

use std::io::{self, Write as _};

fn main() -> anyhow::Result<()> {
    io::stdout().write_all(aether_chassis_bloomery::commission::main_output()?.as_bytes())?;
    Ok(())
}
