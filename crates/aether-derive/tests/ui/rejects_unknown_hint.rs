//! A field attribute the macro doesn't know about errors with the
//! list of accepted hints so the author sees what's available.

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_UNK", cli_prefix = "unk")]
pub struct UnknownHintConfig {
    #[config(magic)]
    pub value: u32,
}

fn main() {}
