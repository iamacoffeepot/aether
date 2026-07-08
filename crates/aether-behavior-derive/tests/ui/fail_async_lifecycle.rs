#![allow(unused)]

use aether_behavior::{Behavior, BehaviorCtx};
use aether_behavior_derive::{behavior, on_attach};
use serde::{Deserialize, Serialize};

#[derive(Default, Serialize, Deserialize)]
struct Hooks;

#[behavior]
impl Behavior for Hooks {
    #[on_attach]
    async fn setup(&mut self, _ctx: &mut BehaviorCtx) {}
}

fn main() {}
