#![allow(unused)]

use aether_behavior::{Behavior, BehaviorCtx};
use aether_behavior_derive::behavior;
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Hooks;

#[behavior]
impl Behavior for Hooks {
    async fn on_frame(&mut self, _ctx: &mut BehaviorCtx) {}
}

fn main() {}
