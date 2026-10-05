//! Session-scoped draw-set registry for the `aether.render` cap
//! (ADR-0246 decisions 1 and 2). A draw set is a retained list of draws,
//! each a run of one geometry's indices drawn once per record of a run
//! of one instance buffer. Every draw is checked when the set is made or
//! patched (`check`), so a pass that walks a set has nothing left to
//! check and no lookup that can miss.
//!
//! A set holds what it draws. It keeps one table per registry of the
//! distinct buffers it names (`rows`), takes one registry hold when a
//! buffer first appears in the table and releases it when the buffer's
//! last draw goes, and stores each draw with row numbers in place of
//! ids. A registry's hold count is therefore the number of sets naming a
//! buffer, and a patch costs what it carries.
//!
//! A set owns no device resource, so a render device replacement leaves
//! it as it is: the registries re-upload what its rows name.

use std::collections::HashMap;

use aether_substrate::session_ids::SessionIds;

use self::check::DrawCheck;
pub use self::rows::DrawSetRows;
use super::geometry::GeometryRegistry;
use super::instances::InstancesRegistry;
use crate::kinds::{
    CreateDrawSet, CreateDrawSetResult, DestroyDrawSet, DrawSpec, IndexRange, InstanceRange, UpdateDrawSet,
    UpdateDrawSetResult, VertexAttribute,
};

mod check;
mod rows;
#[cfg(test)]
mod tests;

/// One stored draw. `geometry` and `instances` are row numbers into the
/// owning set's [`DrawSet::geometries`] and [`DrawSet::instances`]
/// tables, never registry ids; the two ranges are the ones the sender
/// gave, already checked against the buffers those rows name.
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct HeldDraw {
    pub geometry: u32,
    pub indices: IndexRange,
    pub instances: u32,
    pub records: InstanceRange,
}

/// A retained list of draws and the buffers it holds.
pub struct DrawSet {
    vertex_layout: Vec<VertexAttribute>,
    instance_layout: Vec<VertexAttribute>,
    draws: Vec<HeldDraw>,
    geometries: DrawSetRows,
    instances: DrawSetRows,
}

impl DrawSet {
    /// The layout every geometry the set draws was created with.
    #[must_use]
    pub fn vertex_layout(&self) -> &[VertexAttribute] {
        &self.vertex_layout
    }

    /// The layout every instance buffer the set draws was created with.
    #[must_use]
    pub fn instance_layout(&self) -> &[VertexAttribute] {
        &self.instance_layout
    }

    /// The draws, in the order a pass issues them.
    #[must_use]
    pub fn draws(&self) -> &[HeldDraw] {
        &self.draws
    }

    /// The distinct geometries the draws name; [`HeldDraw::geometry`]
    /// indexes it. Each `Some` id is held, so
    /// [`GeometryRegistry::held_mut`] cannot miss on it.
    #[must_use]
    pub const fn geometries(&self) -> &DrawSetRows {
        &self.geometries
    }

    /// The distinct instance buffers the draws name;
    /// [`HeldDraw::instances`] indexes it. Each `Some` id is held, so
    /// [`InstancesRegistry::held_mut`] cannot miss on it.
    #[must_use]
    pub const fn instances(&self) -> &DrawSetRows {
        &self.instances
    }

    /// Write already-checked `draws` over the entries from `first`,
    /// extending past the end, or truncate to `first` when `draws` is
    /// empty. The new draws take their rows and holds before the
    /// replaced ones give theirs up, so a buffer named on both sides is
    /// never released in between.
    fn patch(
        &mut self,
        first: usize,
        draws: &[DrawSpec],
        geometries: &mut GeometryRegistry,
        instances: &mut InstancesRegistry,
    ) {
        let held: Vec<HeldDraw> = draws.iter().map(|draw| self.hold(draw, geometries, instances)).collect();
        let replaced: Vec<HeldDraw> = if held.is_empty() {
            self.draws.drain(first..).collect()
        } else {
            let end = self.draws.len().min(first + held.len());
            self.draws.splice(first..end, held).collect()
        };

        for draw in replaced {
            self.let_go(draw, geometries, instances);
        }
    }

    fn hold(
        &mut self,
        draw: &DrawSpec,
        geometries: &mut GeometryRegistry,
        instances: &mut InstancesRegistry,
    ) -> HeldDraw {
        let geometry = self.geometries.add(draw.geometry_id);
        if geometry.appeared {
            geometries.hold(draw.geometry_id);
        }
        let records = self.instances.add(draw.instances_id);
        if records.appeared {
            instances.hold(draw.instances_id);
        }
        HeldDraw { geometry: geometry.row, indices: draw.indices, instances: records.row, records: draw.instances }
    }

    fn let_go(&mut self, draw: HeldDraw, geometries: &mut GeometryRegistry, instances: &mut InstancesRegistry) {
        if let Some(geometry_id) = self.geometries.remove(draw.geometry) {
            geometries.release(geometry_id);
        }
        if let Some(instances_id) = self.instances.remove(draw.instances) {
            instances.release(instances_id);
        }
    }
}

/// Session-scoped draw-set registry. `ids` hands out the `draw_set_id` a
/// `create_draw_set` reply carries, in creation order and never
/// recycled, as geometry and instance ids are.
#[derive(Default)]
pub struct DrawSetRegistry {
    ids: SessionIds<u32>,
    sets: HashMap<u32, DrawSet>,
}

impl DrawSetRegistry {
    #[must_use]
    pub fn new() -> Self {
        Self { ids: SessionIds::new(), sets: HashMap::new() }
    }

    /// The set registered under `draw_set_id`, if it is live.
    #[must_use]
    pub fn get(&self, draw_set_id: u32) -> Option<&DrawSet> {
        self.sets.get(&draw_set_id)
    }

    /// Make a set, checking its layouts and every draw before an id is
    /// consumed or a buffer held. A refused create leaves the id
    /// sequence and both registries untouched.
    pub fn create(
        &mut self,
        mail: CreateDrawSet,
        geometries: &mut GeometryRegistry,
        instances: &mut InstancesRegistry,
    ) -> CreateDrawSetResult {
        if mail.vertex_layout.is_empty() {
            return CreateDrawSetResult::Err { error: "draw set vertex layout declares no attributes".to_owned() };
        }
        if mail.instance_layout.is_empty() {
            return CreateDrawSetResult::Err { error: "draw set instance layout declares no attributes".to_owned() };
        }
        let check = DrawCheck {
            vertex_layout: &mail.vertex_layout,
            instance_layout: &mail.instance_layout,
            geometries: &*geometries,
            instances: &*instances,
        };
        if let Err(error) = check.all(&mail.draws) {
            return CreateDrawSetResult::Err { error };
        }
        let Some(draw_set_id) = self.ids.allocate() else {
            return CreateDrawSetResult::Err {
                error: "this session has run out of draw set ids; destroy_draw_set does not recycle them".to_owned(),
            };
        };

        let mut set = DrawSet {
            vertex_layout: mail.vertex_layout,
            instance_layout: mail.instance_layout,
            draws: Vec::new(),
            geometries: DrawSetRows::default(),
            instances: DrawSetRows::default(),
        };
        set.patch(0, &mail.draws, geometries, instances);
        self.sets.insert(draw_set_id, set);
        CreateDrawSetResult::Ok { draw_set_id }
    }

    /// Patch a set: overwrite or extend from `first`, or truncate to
    /// `first` with no draws. Everything is checked before anything is
    /// written, so a refused patch leaves the draws, the rows and both
    /// registries' holds as they were.
    pub fn update(
        &mut self,
        mail: UpdateDrawSet,
        geometries: &mut GeometryRegistry,
        instances: &mut InstancesRegistry,
    ) -> UpdateDrawSetResult {
        let Some(set) = self.sets.get_mut(&mail.draw_set_id) else {
            return UpdateDrawSetResult::Err { error: format!("unknown draw set id {}", mail.draw_set_id) };
        };
        let first = mail.first as usize;
        if first > set.draws.len() {
            return UpdateDrawSetResult::Err {
                error: format!("first {first} is past the set's {} draws", set.draws.len()),
            };
        }
        let check = DrawCheck {
            vertex_layout: &set.vertex_layout,
            instance_layout: &set.instance_layout,
            geometries: &*geometries,
            instances: &*instances,
        };
        if let Err(error) = check.all(&mail.draws) {
            return UpdateDrawSetResult::Err { error };
        }

        set.patch(first, &mail.draws, geometries, instances);
        UpdateDrawSetResult::Ok
    }

    /// Release a set and every buffer it holds. Fire-and-forget, so an
    /// unknown id warns and drops.
    pub fn destroy(
        &mut self,
        mail: DestroyDrawSet,
        geometries: &mut GeometryRegistry,
        instances: &mut InstancesRegistry,
    ) {
        let Some(set) = self.sets.remove(&mail.draw_set_id) else {
            tracing::warn!(
                target: "aether_render",
                draw_set_id = mail.draw_set_id,
                "destroy_draw_set for unknown draw set id; dropping",
            );
            return;
        };

        for geometry_id in set.geometries.ids().flatten() {
            geometries.release(geometry_id);
        }
        for instances_id in set.instances.ids().flatten() {
            instances.release(instances_id);
        }
    }
}
