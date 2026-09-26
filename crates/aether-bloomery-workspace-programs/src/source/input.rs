//! `source.select.input`: the imported source image a source tree is selected from.

use aether_bloomery_kinds::{Ref, Tree};

/// The imported source image a source tree is selected from (ADR-0237 decision 3).
///
/// A typed citation, so the driver's closure walk carries every member of the
/// image tree into the invocation, and the selection reads only its root.
#[derive(Debug, Clone, PartialEq, Eq, aether_data::Storage)]
#[kind(name = "source.select.input")]
pub struct SelectInput {
    /// The whole imported source image: the checkout under `source`, beside Docker's placeholders.
    pub image: Ref<Tree>,
}
