//! The one place a draw is checked (ADR-0246 decision 2). A draw that
//! passes here is one a pass can issue without looking at it again: its
//! geometry and instance buffer exist, were created with the set's
//! layouts, and contain the ranges it names. Each failure class has its
//! own reason.

use crate::kinds::{DrawSpec, VertexAttribute};
use crate::runtime::geometry::GeometryRegistry;
use crate::runtime::instances::InstancesRegistry;

/// The layouts a set fixes at create and the registries its draws name
/// into.
pub(super) struct DrawCheck<'a> {
    pub(super) vertex_layout: &'a [VertexAttribute],
    pub(super) instance_layout: &'a [VertexAttribute],
    pub(super) geometries: &'a GeometryRegistry,
    pub(super) instances: &'a InstancesRegistry,
}

impl DrawCheck<'_> {
    /// Check every draw in order. The first failure is the answer, with
    /// the failing draw's position in `draws` in front of its reason.
    pub(super) fn all(&self, draws: &[DrawSpec]) -> Result<(), String> {
        for (position, draw) in draws.iter().enumerate() {
            self.one(draw).map_err(|reason| format!("draw {position}: {reason}"))?;
        }
        Ok(())
    }

    fn one(&self, draw: &DrawSpec) -> Result<(), String> {
        let Some(geometry) = self.geometries.entries.get(&draw.geometry_id) else {
            return Err(format!("unknown geometry id {}", draw.geometry_id));
        };
        if geometry.layout != self.vertex_layout {
            return Err(format!(
                "geometry {} was created with a layout that is not the set's vertex layout",
                draw.geometry_id,
            ));
        }
        let index_count = geometry.index_count();
        if !fits(draw.indices.first, draw.indices.count, index_count) {
            return Err(format!(
                "{} indices from index {} run past geometry {}'s {index_count} indices",
                draw.indices.count, draw.indices.first, draw.geometry_id,
            ));
        }

        let Some(instances) = self.instances.get(draw.instances_id) else {
            return Err(format!("unknown instances id {}", draw.instances_id));
        };
        if instances.layout() != self.instance_layout {
            return Err(format!(
                "instance buffer {} was created with a layout that is not the set's instance layout",
                draw.instances_id,
            ));
        }
        let capacity = instances.capacity();
        if !fits(draw.instances.first, draw.instances.count, capacity) {
            return Err(format!(
                "{} records from record {} run past instance buffer {}'s capacity of {capacity} records",
                draw.instances.count, draw.instances.first, draw.instances_id,
            ));
        }
        Ok(())
    }
}

/// Whether `count` items from `first` end at or before `limit`. The end
/// is summed checked, so a `first` near the top of the range cannot wrap
/// back inside.
fn fits(first: u32, count: u32, limit: u32) -> bool {
    first.checked_add(count).is_some_and(|end| end <= limit)
}
