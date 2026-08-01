//! Confique fixtures shared by the sibling modules' unit tests: a plain
//! `#[derive(confique::Config)]` type per resolution scenario, plus the
//! hand-registered knob record and error helper the discovery-surface tests
//! read.

use std::num::ParseIntError;

use confique::meta::Meta;

use super::{FromArgvThenEnv, KnobKind, KnobRecord};

// Plain `#[derive(confique::Config)]` fixture (not the aether
// `Config` derive — that emits a clap `Overlay` whose `#[arg]`
// attrs need clap in scope, which `aether-substrate` doesn't
// carry). This gives a real `META` + `Layer` to walk; the strict
// `parse_env` reproduces the ADR-0090 §4 hard-error path.
#[derive(Clone, Debug, confique::Config)]
#[allow(dead_code)] // fields exercised via META / load, not read directly
pub(super) struct FixtureConfig {
    #[config(env = "AETHER_TEST_COUNT", parse_env = parse_count, default = 7)]
    count: u32,
    #[config(env = "AETHER_TEST_FLAG", default = false)]
    enabled: bool,
}

#[derive(Clone, Debug, confique::Config)]
#[allow(dead_code)]
pub(super) struct PrecedenceConfig {
    #[config(env = "AETHER_PRECEDENCE_COUNT", default = 7)]
    pub(super) count: u32,
}

impl FromArgvThenEnv for PrecedenceConfig {
    type Layer = Self;

    fn from_layer(layer: Self::Layer) -> Self {
        layer
    }
}

// A unique-keyed fixture so the hermetic env-skip test never races the
// `AETHER_PRECEDENCE_COUNT` mutation in `try_resolve_orders_*`.
#[derive(Clone, Debug, confique::Config)]
#[allow(dead_code)]
pub(super) struct HermeticFixture {
    #[config(env = "AETHER_HERMETIC_FIXTURE_COUNT", default = 7)]
    pub(super) count: u32,
}

impl FromArgvThenEnv for HermeticFixture {
    type Layer = Self;

    fn from_layer(layer: Self::Layer) -> Self {
        layer
    }
}

pub(super) fn precedence_file_layer(value: u32) -> <PrecedenceConfig as confique::Config>::Layer {
    use confique::Layer as _;
    let mut file = <PrecedenceConfig as confique::Config>::Layer::empty();
    file.count = Some(value);
    file
}

pub(super) fn precedence_file_table(value: u32) -> toml::Table {
    format!("[precedence]\ncount = {value}\n").parse::<toml::Table>().expect("parse precedence table")
}

fn parse_count(raw: &str) -> Result<u32, ParseIntError> {
    raw.trim().parse()
}

/// A real `ParseIntError` for the `ConfigError` constructor tests
/// (clippy forbids `unwrap_err()` — a parse of a non-number is the
/// honest way to obtain one).
pub(super) fn an_int_error() -> ParseIntError {
    match "x".parse::<u32>() {
        Ok(_) => unreachable!("\"x\" is not a u32"),
        Err(e) => e,
    }
}

pub(super) const FIXTURE_KNOBS: &[KnobRecord] = &[KnobRecord {
    env_key: "AETHER_FIXTURE_KNOB",
    doc: "a hand-registered fixture knob",
    default: Some("42"),
    kind: KnobKind::HandRegistered,
}];

pub(super) fn fixture_meta() -> &'static Meta {
    &<FixtureConfig as confique::Config>::META
}
