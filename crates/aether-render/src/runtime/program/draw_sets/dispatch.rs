//! What a dispatch owes its `DrawSets` passes before anything is
//! recorded (ADR-0246 decision 4): the lists it carries are the number
//! the program declared, every id in them names a live draw set, and
//! each set has the layouts of every pass that draws it. A draw inside
//! a set is not looked at; it was checked when the set was made.
//!
//! The cost is one layout comparison per listed set per pass that draws
//! its list, each frame, and none per draw.

use super::validate::DrawSetsPlan;
use crate::ProgramDispatch;
use crate::runtime::draw_set::{DrawSet, DrawSetRegistry};
use crate::runtime::geometry::GeometryRegistry;
use crate::runtime::instances::InstancesRegistry;
use crate::runtime::pipeline::RenderGpu;
use crate::runtime::program::validate::ProgramPlan;

impl DrawSetsPlan {
    /// Whether `set` was made with the two layouts this pass's vertex
    /// buffers are built from, which is what lets the pass draw it.
    fn takes(&self, set: &DrawSet) -> bool {
        set.vertex_layout() == self.vertex_layout && set.instance_layout() == self.instance_layout
    }
}

/// Run the draw-set checks, warn-dropping on the first mismatch. `true`
/// means every list is present and every listed set can be drawn by
/// every pass that names its list.
pub fn check(plan: &ProgramPlan, draw_sets: &DrawSetRegistry, dispatch: &ProgramDispatch) -> bool {
    let program_id = dispatch.program_id;
    if dispatch.draw_sets.len() != plan.draw_set_lists as usize {
        tracing::warn!(
            target: "aether_render",
            program_id,
            declared = plan.draw_set_lists,
            supplied = dispatch.draw_sets.len(),
            "program dispatch draw-set list count disagrees with the registered graph; dropping the dispatch",
        );
        return false;
    }

    for (pass, pass_plan) in plan.passes.iter().enumerate() {
        let Some(stage) = pass_plan.stage.draw_sets() else {
            continue;
        };
        for &draw_set_id in &dispatch.draw_sets[stage.list as usize] {
            let Some(set) = draw_sets.get(draw_set_id) else {
                tracing::warn!(
                    target: "aether_render",
                    program_id,
                    pass,
                    list = stage.list,
                    draw_set_id,
                    "program dispatch lists an unknown draw set id; dropping the dispatch",
                );
                return false;
            };
            if !stage.takes(set) {
                tracing::warn!(
                    target: "aether_render",
                    program_id,
                    pass,
                    list = stage.list,
                    draw_set_id,
                    pass_vertex_layout = ?stage.vertex_layout,
                    pass_instance_layout = ?stage.instance_layout,
                    set_vertex_layout = ?set.vertex_layout(),
                    set_instance_layout = ?set.instance_layout(),
                    "program dispatch lists a draw set whose layouts are not the pass's; dropping the dispatch",
                );
                return false;
            }
        }
    }
    true
}

/// Realize every buffer the listed sets hold, once per row. Rows are
/// resolved every frame and no buffer handle is kept between frames: a
/// geometry re-creates its buffers when an update dirtied it, and an
/// instance buffer uploads its dirty byte range only here, so each row
/// has to be visited each frame in any case. The lookups go through
/// `held_mut`, which also reaches a buffer destroyed under a set.
///
/// # Panics
/// Panics on a listed id that names no set, which [`check`] refuses
/// before this runs.
pub fn realize(
    gpu: &RenderGpu,
    draw_sets: &DrawSetRegistry,
    geometries: &mut GeometryRegistry,
    instances: &mut InstancesRegistry,
    dispatch: &ProgramDispatch,
) {
    for &draw_set_id in dispatch.draw_sets.iter().flatten() {
        let set = draw_sets.get(draw_set_id).expect("the dispatch check found every listed draw set");
        for geometry_id in set.geometries().ids().flatten() {
            geometries.held_mut(geometry_id).ensure_realized(&gpu.device, &gpu.queue);
        }
        for instances_id in set.instances().ids().flatten() {
            instances.held_mut(instances_id).ensure_realized(&gpu.device, &gpu.queue);
        }
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Blob;

    use super::*;
    use crate::runtime::program::validate::validate;
    use crate::{
        CreateDrawSet, CreateDrawSetResult, CreateGeometry, CreateGeometryResult, CreateInstances,
        CreateInstancesResult, Cull, DrawSetsPass, DrawSpec, IndexRange, InstanceRange, Mips, OutputSlot, PassLoad,
        PassStage, ProgramPass, ProgramRegister, Sampling, SlotExtent, SlotShape, SlotSpec, TextureFormat,
        VertexAttribute, VertexFormat, Wrap,
    };

    const MODULE: &str = r"
@vertex
fn vs_placed(@location(0) position: vec3<f32>, @location(4) offset: vec2<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position.xy + offset, position.z, 1.0);
}

@fragment
fn fs_white() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}
";

    fn vertex_layout() -> Vec<VertexAttribute> {
        vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
    }

    /// One offset per instance: stride 8.
    fn instance_layout() -> Vec<VertexAttribute> {
        vec![VertexAttribute { location: 4, format: VertexFormat::Float32x2 }]
    }

    fn pass(list: u32) -> ProgramPass {
        ProgramPass {
            stage: PassStage::DrawSets(DrawSetsPass {
                vertex_entry_point: "vs_placed".to_owned(),
                vertex_layout: vertex_layout(),
                instance_layout: instance_layout(),
                draw_sets: list,
                cull: Cull::None,
                depth: None,
                load: PassLoad::Load,
            }),
            entry_point: "fs_white".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    /// A program whose two passes draw lists 0 and 1.
    fn two_list_plan() -> ProgramPlan {
        let mail = ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![SlotSpec {
                format: TextureFormat::Rgba8,
                shape: SlotShape::Target(SlotExtent::Full),
                sampling: Sampling::Filtered { wrap: Wrap::Clamp, mips: Mips::Base },
            }],
            transients: Vec::new(),
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![pass(0), pass(1)],
        };
        validate(&mail).expect("the two-list program validates")
    }

    /// The registries a draw set lives across, staged on the CPU.
    #[derive(Default)]
    struct Registries {
        geometries: GeometryRegistry,
        instances: InstancesRegistry,
        draw_sets: DrawSetRegistry,
    }

    impl Registries {
        /// A set of one draw, its instance buffer made with
        /// `instance_layout`.
        fn set_with(&mut self, instance_layout: Vec<VertexAttribute>) -> u32 {
            let indices: Vec<u8> = [0u32, 1, 2].iter().flat_map(|index| index.to_le_bytes()).collect();
            let geometry = CreateGeometry {
                layout: vertex_layout(),
                vertices: Blob::from(vec![0u8; 36]),
                indices: Blob::from(indices),
            };
            let CreateGeometryResult::Ok { geometry_id } = self.geometries.create(geometry) else {
                panic!("geometry create must be accepted");
            };
            let records =
                CreateInstances { layout: instance_layout.clone(), capacity: 1, records: Blob::from(Vec::new()) };
            let CreateInstancesResult::Ok { instances_id } = self.instances.create(records) else {
                panic!("instances create must be accepted");
            };

            let mail = CreateDrawSet {
                vertex_layout: vertex_layout(),
                instance_layout,
                draws: vec![DrawSpec {
                    geometry_id,
                    indices: IndexRange { first: 0, count: 3 },
                    instances_id,
                    instances: InstanceRange { first: 0, count: 1 },
                }],
            };
            match self.draw_sets.create(mail, &mut self.geometries, &mut self.instances) {
                CreateDrawSetResult::Ok { draw_set_id } => draw_set_id,
                CreateDrawSetResult::Err { error } => panic!("draw set create must be accepted; got {error}"),
            }
        }
    }

    fn dispatch(draw_sets: Vec<Vec<u32>>) -> ProgramDispatch {
        ProgramDispatch { program_id: 0, bindings: vec![0], geometries: Vec::new(), draw_sets, uniforms: Vec::new() }
    }

    /// The bug: a dispatch one list short is accepted, and the encode
    /// indexes `dispatch.draw_sets` past its end for the second pass.
    #[test]
    fn a_dispatch_one_list_short_is_dropped() {
        let plan = two_list_plan();
        let mut registries = Registries::default();
        let set = registries.set_with(instance_layout());

        assert!(check(&plan, &registries.draw_sets, &dispatch(vec![vec![set], vec![set]])));
        assert!(!check(&plan, &registries.draw_sets, &dispatch(vec![vec![set]])));
    }

    /// The bug: a set made with another instance layout is accepted, and
    /// the pass reads its records at the pipeline's stride, not theirs.
    /// The mismatched set sits in the second list, so a check that reads
    /// only the first pass's list misses it.
    #[test]
    fn a_set_with_another_instance_layout_is_dropped() {
        let plan = two_list_plan();
        let mut registries = Registries::default();
        let matching = registries.set_with(instance_layout());
        let wider = registries.set_with(vec![VertexAttribute { location: 4, format: VertexFormat::Float32x3 }]);

        assert!(!check(&plan, &registries.draw_sets, &dispatch(vec![vec![matching], vec![matching, wider]])));
    }
}
