//! Accept case for the parser-free shapes: a bare numeric and `bool`
//! field (confique native deserialization, no `parse`), a `nonzero`
//! field (resolved `0` → default), and a `String` field with a default
//! (empty → default in `from_layer`). Proves the Layer + Overlay +
//! impls compile with no hand-rolled `parse_env` in sight.
//!
//! (The `csv_set` auto-wire emits a path into `aether_substrate`, which
//! `aether-derive` can't depend on; it is covered by the cap configs and
//! the `parse_csv_set` unit test instead.)

#[derive(aether_derive::Config)]
#[config(env_prefix = "AETHER_AUTO", cli_prefix = "auto")]
pub struct AutoConfig {
    #[config(default = false)]
    pub disabled: bool,
    #[config(default = 30_000)]
    pub max_bytes: usize,
    #[config(default = 2, nonzero)]
    pub max_in_flight: usize,
    #[config(default = "127.0.0.1:8080")]
    pub bind_addr: String,
}

fn main() {}
