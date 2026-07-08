// Two handlers can use different spellings for the same kind type. Token
// equality misses this, so the generated const duplicate-id guard must fail.
#![allow(unused)]

use aether_behavior::{Behavior, BehaviorCtx};
use aether_behavior_derive::{behavior, on};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior.duplicate_id.gauge")]
struct Gauge {
    value: u32,
}

type GaugeAlias = Gauge;

#[derive(Default, Serialize, Deserialize)]
struct Clamp;

#[behavior]
impl Behavior for Clamp {
    #[on]
    fn clamp(&mut self, _ctx: &mut BehaviorCtx, gauge: &Gauge) {
        let _ = gauge;
    }

    #[on]
    fn clamp_alias(&mut self, _ctx: &mut BehaviorCtx, gauge: &GaugeAlias) {
        let _ = gauge;
    }
}

fn main() {}
