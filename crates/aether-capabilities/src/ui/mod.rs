//! `aether.ui` cap and widget kinds (ADR-0107).
//!
//! The five widget kinds (`UiPanel`, `UiBar`, `UiLabel`, `UiButton`,
//! `UiClicked`) live here — cycle-free from `aether-kinds` — and the
//! `UiCapability` implementation that translates them into render + text
//! sends.

pub mod kinds;
mod widgets;

pub use kinds::{UiBar, UiButton, UiClicked, UiLabel, UiPanel};
pub use widgets::UiCapability;
