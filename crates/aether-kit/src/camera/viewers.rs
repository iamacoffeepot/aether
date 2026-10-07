//! The actors a camera sends its view to.
//!
//! A viewer is whoever sent the camera `aether.render.view_subscribe`: the
//! renderer once it is told to follow this camera, or a component that draws
//! through its own view. Each is held as the `ProtocolRef` its views are sent
//! through (ADR-0231 §8), keyed by its erased twin, which is what a removal
//! compares.
//!
//! A viewer is held with the watch that removes it (ADR-0079 §8): the camera
//! watches each viewer it adds, and the row keeps the watch's id beside the
//! reference, so the table and the camera's watches say the same thing for
//! as long as the instance lives. A viewer leaves the table by unsubscribing,
//! which ends its watch, or by closing, which is the watch's notice.

use std::collections::BTreeMap;

use aether_actor::{ErasedActorRef, ProtocolRef, Sends, Subscriber, WatchId};
use aether_render::ViewProjection;

/// A viewer's proven reference: an actor that takes `ViewProjection` and
/// answers nothing.
pub(super) type Viewer = ProtocolRef<Subscriber<ViewProjection>>;

/// One held viewer: where its views are sent, and the watch on its close.
struct Held {
    viewer: Viewer,
    watch: WatchId,
}

#[derive(Default)]
pub(super) struct Viewers {
    table: BTreeMap<ErasedActorRef, Held>,
}

impl Viewers {
    /// Hold `viewer` with `watch`, the watch on its close. One that is
    /// already held is held once.
    pub(super) fn add(&mut self, viewer: Viewer, watch: WatchId) {
        self.table.insert(viewer.erase(), Held { viewer, watch });
    }

    /// Let go of `viewer` and answer the watch it was held with. `None` when
    /// it was not held, which changes nothing.
    pub(super) fn remove(&mut self, viewer: ErasedActorRef) -> Option<WatchId> {
        self.table.remove(&viewer).map(|held| held.watch)
    }

    /// How many viewers are held.
    pub(super) fn len(&self) -> usize {
        self.table.len()
    }

    /// Send `view` to every viewer.
    pub(super) fn send<A>(&self, sends: &mut Sends<'_, A>, view: &ViewProjection) {
        for held in self.table.values() {
            sends.send_to(held.viewer, view);
        }
    }
}
