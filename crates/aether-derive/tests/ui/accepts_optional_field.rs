//! `Option<String>` — type-driven empty-string ≡ unset behavior. The
//! `env = "..."` per-field override pins an unprefixed env key (the
//! shape `GEMINI_API_KEY` / `ANTHROPIC_API_KEY` use in the real caps).

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_OPT", cli_prefix = "opt")]
pub struct OptionalConfig {
    #[config(env = "SOME_BARE_KEY")]
    pub api_key: Option<String>,
}

fn main() {}
