#![allow(unused)]

use aether_behavior::{Behavior, BehaviorCtx};
use aether_behavior_derive::{behavior, on};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior.gauge")]
struct Gauge {
    value: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Clamp;

#[behavior]
impl Behavior for Clamp {
    #[on]
    async fn clamp(&mut self, _ctx: &mut BehaviorCtx, gauge: &mut Gauge) {
        gauge.value = 0;
    }
}

fn main() {}
