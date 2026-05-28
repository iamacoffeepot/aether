//! `ms_duration` is only valid on a `Duration` field — applying it to
//! a `u32` is a programmer error caught at expansion time.

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_BAD", cli_prefix = "bad")]
pub struct BadConfig {
    #[config(default = 30_000, ms_duration)]
    pub timeout: u32,
}

fn main() {}
