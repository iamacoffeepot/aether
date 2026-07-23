//! Local kind twins for the behavior-script fixtures (issue 2688).
//!
//! A behavior script cannot depend on `aether-kit` for the kinds it
//! transforms — kit pulls `aether-actor`, which would classify the script
//! cdylib as a component. Each twin here re-derives a kit widget kind under
//! the *same* `#[kind(name)]` wire name, so its `KindId` and wire bytes match
//! the real kit kind and it decodes the live traffic byte-for-byte. A drift
//! between a twin and its kit original is a decode mismatch the #2688 scenario
//! trips on loudly (the clamp assertion fails), not a compile error.

use serde::{Deserialize, Serialize};

/// Twin of `aether_kit_widget::SliderChanged` — the value-up event the
/// scripts intercept. Same wire name and field shape as
/// `crates/aether-kit/src/widget/kinds.rs`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.slider.changed")]
pub struct SliderChanged {
    pub value: f32,
    pub committed: bool,
}

/// Twin of `aether_kit_widget::RadioSelected` — carries a `u32` up the panel
/// lane, which the scripts reuse as an observable effect (`ctx.panel().emit`)
/// to surface their authored `count` where the panel logs it. Same wire name
/// and field shape as `crates/aether-kit/src/widget/kinds.rs`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.kit.widget.radio.selected")]
pub struct RadioSelected {
    pub index: u32,
}
