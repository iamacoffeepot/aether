//! The actors a camera sends its view to.
//!
//! A viewer is whoever sent the camera `aether.render.view_subscribe`: the
//! renderer once it is told to follow this camera, or a component that draws
//! through its own view. Each is held as the `ProtocolRef` its views are sent
//! through (ADR-0231 §8), keyed by its erased twin, which is what a removal
//! compares.

use std::collections::BTreeMap;

use aether_actor::{ErasedActorRef, ProtocolRef, Sends, Subscriber};
use aether_render::ViewProjection;

/// A viewer's proven reference: an actor that takes `ViewProjection` and
/// answers nothing.
pub(super) type Viewer = ProtocolRef<Subscriber<ViewProjection>>;

#[derive(Default)]
pub(super) struct Viewers {
    table: BTreeMap<ErasedActorRef, Viewer>,
}

impl Viewers {
    /// Hold `viewer`. One that is already held is held once.
    pub(super) fn add(&mut self, viewer: Viewer) {
        self.table.insert(viewer.erase(), viewer);
    }

    /// Let go of `viewer`. One that is not held changes nothing.
    pub(super) fn remove(&mut self, viewer: ErasedActorRef) {
        self.table.remove(&viewer);
    }

    /// How many viewers are held.
    pub(super) fn len(&self) -> usize {
        self.table.len()
    }

    /// Send `view` to every viewer.
    pub(super) fn send<A>(&self, sends: &mut Sends<'_, A>, view: &ViewProjection) {
        for viewer in self.table.values() {
            sends.send_to(*viewer, view);
        }
    }
}
