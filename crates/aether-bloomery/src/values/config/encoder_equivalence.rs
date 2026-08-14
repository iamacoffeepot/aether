//! Encoder-equivalence tripwires for every sealed configuration kind.
//!
//! `POST /configs` addresses over `encode_schema` bytes; the typed path
//! (`digest_of` / [`ConfigKind::address`]) addresses over `wire::to_vec`
//! bytes. The two drivers must produce identical bytes — and therefore the
//! same digest — for the same logical value, or a config authored over REST
//! seals at an address no typed re-derivation can reach.

use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::vec;
use core::fmt::Debug;

use aether_codec::encode_schema;
use aether_data::Schema;
use aether_data::wire::to_vec;
use serde::Serialize;

use super::{ConfigKind, config_address};
use crate::digest::digest_of;
use crate::ids::StageId;
use crate::values::{
    AgentProfile, AgentSelection, ApprovalPolicy, ApprovalRule, Harness, LongContextBand, ModelOverride, PriceRates,
    PriceTable, ReasoningEffort, StageBinding, StageCatalog, StageOverride, Tier, ToolPolicy,
};

// Tripwire: the sealed registry address and the typed re-derivation must be
// the same address. A byte-level split (an Option encoded differently, a
// `#[serde(default)]` asymmetry, a skipped field) would mint two addresses
// for one value and fail nothing else.
fn assert_encoders_agree<K>(value: &K)
where
    K: ConfigKind + Schema + Serialize + Debug,
{
    let typed = to_vec(value).expect("a fixture config wire-encodes");
    let json = serde_json::to_value(value).expect("a fixture config has a JSON form");
    let authored = encode_schema(&json, &K::SCHEMA)
        .unwrap_or_else(|error| panic!("{} JSON form must schema-encode: {error}", K::NAME));

    assert_eq!(typed, authored, "{}: schema-driven and serde-driven encodings diverged", K::NAME);
    assert_eq!(
        value.address(),
        config_address(K::NAME, &authored),
        "{}: typed address and sealed registry address diverged",
        K::NAME,
    );
}

#[test]
fn sealed_config_kinds_share_one_address_across_encoders() {
    let price = PriceTable {
        rows: BTreeMap::from([
            (
                String::from("claude-opus-5"),
                PriceRates {
                    input: 5_000_000,
                    cache_read: 500_000,
                    cache_write_5m: 6_250_000,
                    cache_write_1h: 10_000_000,
                    cache_write: 6_250_000,
                    output: 25_000_000,
                    long_context: None,
                },
            ),
            (
                String::from("grok-4.6"),
                PriceRates {
                    input: 2_000_000,
                    cache_read: 200_000,
                    cache_write_5m: 0,
                    cache_write_1h: 0,
                    cache_write: 0,
                    output: 10_000_000,
                    long_context: Some(LongContextBand {
                        prompt_tokens: 200_000,
                        input: 4_000_000,
                        cache_read: 400_000,
                        cache_write_5m: 0,
                        cache_write_1h: 0,
                        cache_write: 0,
                        output: 20_000_000,
                    }),
                },
            ),
        ]),
    };
    assert_encoders_agree(&price);
    assert_eq!(digest_of(&price), price.address());

    assert_encoders_agree(&ModelOverride {
        agent: Some(AgentSelection { harness: Harness::Claude, model: String::from("claude-opus-5") }),
        reasoning_effort: None,
        per_stage: BTreeMap::from([(
            StageId::Refine,
            StageOverride { agent: None, reasoning_effort: Some(ReasoningEffort::Max) },
        )]),
    });

    assert_encoders_agree(&ApprovalPolicy {
        default: Tier::Judge,
        rules: vec![ApprovalRule { glob: String::from("docs/guide/**"), tier: Tier::Auto }],
    });

    let catalog = StageCatalog {
        bindings: vec![StageBinding {
            stage: StageId::Construct,
            consumes: vec![String::from("bloom.ready")],
            produces: vec![String::from("bloom.candidate")],
            profile: AgentProfile {
                harness: Harness::Muse,
                model: String::from("muse-spark-1.2-contributor"),
                effort: ReasoningEffort::High,
                tools: ToolPolicy::Full,
            },
            process: String::from("construct.implement"),
            completion_gate: String::from("pr-open"),
            retry_budget: 2,
            wall_clock_secs: 3_600,
        }],
    };
    assert_encoders_agree(&catalog);
    assert_eq!(digest_of(&catalog), catalog.address());
}
