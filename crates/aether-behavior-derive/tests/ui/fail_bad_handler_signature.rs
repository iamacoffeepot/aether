// A `#[on]` handler whose kind parameter is by-value rather than a
// reference. The macro must point at the parameter and explain the
// `&K` / `&mut K` intent encoding rather than emitting an opaque error.
#![allow(unused)]

use aether_behavior::{Behavior, BehaviorCtx};
use aether_behavior_derive::{behavior, on};
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize, aether_data::Kind, aether_data::Schema)]
#[kind(name = "test.behavior.slider")]
struct Slider {
    value: u32,
}

#[derive(Default, Serialize, Deserialize)]
struct Clamp;

#[behavior]
impl Behavior for Clamp {
    #[on]
    fn clamp(&mut self, _ctx: &mut BehaviorCtx, slider: Slider) {
        let _ = slider;
    }
}

fn main() {}
