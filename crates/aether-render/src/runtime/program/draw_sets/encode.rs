//! The walk a `DrawSets` pass makes inside its open render pass
//! (ADR-0246 decision 4): every listed set in order, every draw of a set
//! in order, nothing sorted and nothing checked. The substrate opens the
//! pass and binds what is the pass's; the walk stays here because the
//! render cap owns `DrawSet`.
//!
//! A stored draw names its buffers by row number. The walk resolves a
//! set's rows to realized buffers once, into two tables indexed by row,
//! and then issues each draw with two indexed reads and no lookup.

use crate::runtime::draw_set::{DrawSet, DrawSetRegistry};
use crate::runtime::geometry::{GeometryRegistry, RealizedGeometry};
use crate::runtime::instances::InstancesRegistry;

/// The three registries a walk reads, all shared: the sets, and the two
/// that own the buffers a set's rows name.
#[derive(Copy, Clone)]
pub struct DrawSetSources<'a> {
    pub draw_sets: &'a DrawSetRegistry,
    pub geometries: &'a GeometryRegistry,
    pub instances: &'a InstancesRegistry,
}

/// The realized buffers behind one set's rows, indexed by row number. A
/// freed row no draw names is `None`. Refilled per set and kept by the
/// caller across passes, so a steady frame allocates nothing here.
#[derive(Default)]
pub struct RowBuffers<'a> {
    geometries: Vec<Option<&'a RealizedGeometry>>,
    instances: Vec<Option<&'a wgpu::Buffer>>,
}

impl<'a> RowBuffers<'a> {
    /// Resolve `set`'s rows through the shared held lookups, which reach
    /// a buffer destroyed under the set as well as a live one.
    fn fill(&mut self, set: &DrawSet, sources: &DrawSetSources<'a>) {
        let geometries = sources.geometries;
        self.geometries.clear();
        self.geometries.extend(set.geometries().ids().map(|row| row.map(|id| realized_geometry(geometries, id))));

        let instances = sources.instances;
        self.instances.clear();
        self.instances.extend(set.instances().ids().map(|row| row.map(|id| realized_records(instances, id))));
    }
}

fn realized_geometry(geometries: &GeometryRegistry, geometry_id: u32) -> &RealizedGeometry {
    geometries.held(geometry_id).realized.as_ref().expect("a listed set's geometries are realized before the encode")
}

fn realized_records(instances: &InstancesRegistry, instances_id: u32) -> &wgpu::Buffer {
    instances.held(instances_id).realized().expect("a listed set's instance buffers are realized before the encode")
}

/// Issue every draw of every set in `list`, in order, into `pass`, whose
/// pipeline and bind groups are already set. Vertex buffer 0 and the
/// index buffer are rebound when a draw's geometry row differs from the
/// last draw's, and vertex buffer 1 when its instance row does. The
/// last-bound rows are forgotten at each set: row numbers are per set,
/// so row 0 of one set is a different buffer from row 0 of the next. A
/// draw of no indices or no records is skipped before anything is bound.
///
/// # Panics
/// Panics on a listed id that names no set or a row that was not
/// realized; the dispatch check and realize step rule both out before a
/// pass is opened.
pub fn draw_list<'a>(
    pass: &mut wgpu::RenderPass<'_>,
    sources: &DrawSetSources<'a>,
    list: &[u32],
    rows: &mut RowBuffers<'a>,
) {
    for &draw_set_id in list {
        let set = sources.draw_sets.get(draw_set_id).expect("the dispatch check found every listed draw set");
        rows.fill(set, sources);

        let mut bound_geometry = None;
        let mut bound_instances = None;
        for draw in set.draws() {
            let no_indices = draw.indices.count == 0;
            let no_records = draw.records.count == 0;
            if no_indices || no_records {
                continue;
            }

            if bound_geometry != Some(draw.geometry) {
                let geometry = rows.geometries[draw.geometry as usize].expect("a draw's geometry row is in use");
                pass.set_vertex_buffer(0, geometry.vertex_buffer.slice(..));
                pass.set_index_buffer(geometry.index_buffer.slice(..), wgpu::IndexFormat::Uint32);
                bound_geometry = Some(draw.geometry);
            }
            if bound_instances != Some(draw.instances) {
                let records = rows.instances[draw.instances as usize].expect("a draw's instance row is in use");
                pass.set_vertex_buffer(1, records.slice(..));
                bound_instances = Some(draw.instances);
            }

            let indices = draw.indices.first..draw.indices.first + draw.indices.count;
            let records = draw.records.first..draw.records.first + draw.records.count;
            pass.draw_indexed(indices, 0, records);
        }
    }
}
