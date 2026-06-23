//! `nonzero` coerces a resolved `0` to the field default, so it
//! requires a `default` — applying it to a field without one is a
//! programmer error caught at expansion time.

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_BAD", cli_prefix = "bad")]
pub struct BadConfig {
    #[config(nonzero)]
    pub max_in_flight: usize,
}

fn main() {}
