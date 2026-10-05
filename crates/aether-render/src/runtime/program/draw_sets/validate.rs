//! Register-time validation of a `PassStage::DrawSets` pass (ADR-0246
//! decision 4). Pure CPU, as the rest of program validation is; each
//! failure class has its own reason.

use naga::Module;

use crate::runtime::program::validate::{
    PassPlan, RasterPass, check_depth, check_depth_only, check_vertex_interface, vertex_entry,
};
use crate::{Cull, DepthUse, DepthWrite, DrawSetsPass, PassLoad, ProgramRegister, VertexAttribute};

/// One validated draw-sets pass: the authored vertex entry, the two
/// layouts its vertex buffers are built from and a drawn set must have,
/// the dispatch list it draws, and its cull, depth and color-load
/// declarations.
#[derive(Debug)]
pub struct DrawSetsPlan {
    pub vertex_entry_point: String,
    pub vertex_layout: Vec<VertexAttribute>,
    pub instance_layout: Vec<VertexAttribute>,
    /// Index into `ProgramDispatch.draw_sets`.
    pub list: u32,
    pub cull: Cull,
    pub depth: Option<DepthUse>,
    pub load: PassLoad,
}

/// A validated draw-sets declaration plus the naga entry index of its
/// vertex stage, which the caller needs for the shared uniform-window
/// check.
pub struct ValidatedDrawSets {
    pub plan: DrawSetsPlan,
    pub vertex_entry_index: usize,
}

/// Validate one `PassStage::DrawSets` declaration, in check order: the
/// vertex entry exists, the two layouts can each be a vertex buffer and
/// share no location, the vertex stage's interface agrees with the two
/// together, the depth declaration is coherent with the pass, and a
/// pass with no color output is a well-formed depth-only pass that
/// writes the depth slot it names.
pub fn validate_draw_sets(
    mail: &ProgramRegister,
    module: &Module,
    pass: &DrawSetsPass,
    raster: &RasterPass<'_>,
) -> Result<ValidatedDrawSets, String> {
    let index = raster.index;
    let vertex_entry_index = vertex_entry(module, index, &pass.vertex_entry_point)?;
    check_layouts(index, &pass.vertex_layout, &pass.instance_layout)?;

    let attributes: Vec<VertexAttribute> = pass.vertex_layout.iter().chain(&pass.instance_layout).copied().collect();
    let declared_by = "the pass's vertex or instance layout";
    check_vertex_interface(module, index, vertex_entry_index, &attributes, declared_by)?;
    check_depth(mail, module, pass.depth.map(|depth| depth.slot), raster)?;
    check_depth_only(module, pass.load, raster)?;
    check_depth_only_writes(pass.depth, raster)?;

    Ok(ValidatedDrawSets {
        plan: DrawSetsPlan {
            vertex_entry_point: pass.vertex_entry_point.clone(),
            vertex_layout: pass.vertex_layout.clone(),
            instance_layout: pass.instance_layout.clone(),
            list: pass.draw_sets,
            cull: pass.cull,
            depth: pass.depth,
            load: pass.load,
        },
        vertex_entry_index,
    })
}

/// A depth-only pass writes the depth slot it names, since that slot is
/// all it attaches: `DepthWrite::TestOnly`, which only this stage can
/// declare, would leave a pass that draws and writes nothing.
fn check_depth_only_writes(depth: Option<DepthUse>, raster: &RasterPass<'_>) -> Result<(), String> {
    let depth_only = raster.attached.is_none();
    let test_only = depth.is_some_and(|depth| depth.write == DepthWrite::TestOnly);
    if depth_only && test_only {
        return Err(format!(
            "pass {}: a depth-only pass declaring DepthWrite::TestOnly would write nothing — its depth slot is all \
             it attaches, so it declares DepthWrite::Write",
            raster.index,
        ));
    }
    Ok(())
}

/// Each layout must be one a vertex buffer can be built from, and the
/// two must not meet: an attribute binds at the location its layout
/// declares, so a location both declare would feed one shader input
/// from two buffers. wgpu would refuse any of these as an opaque
/// `pipeline creation failed`.
fn check_layouts(
    index: usize,
    vertex_layout: &[VertexAttribute],
    instance_layout: &[VertexAttribute],
) -> Result<(), String> {
    for (name, layout) in [("vertex", vertex_layout), ("instance", instance_layout)] {
        if layout.is_empty() {
            return Err(format!("pass {index}: the {name} layout declares no attributes"));
        }
        if let Some(location) = repeated_location(layout) {
            return Err(format!("pass {index}: the {name} layout declares location {location} twice"));
        }
    }

    let shared = vertex_layout.iter().find(|attribute| declares(instance_layout, attribute.location));
    if let Some(attribute) = shared {
        return Err(format!(
            "pass {index}: the vertex layout and the instance layout both declare location {} — the two vertex \
             buffers of a draw-sets pass share no location",
            attribute.location,
        ));
    }
    Ok(())
}

fn declares(layout: &[VertexAttribute], location: u32) -> bool {
    layout.iter().any(|attribute| attribute.location == location)
}

/// The first location `layout` declares more than once.
fn repeated_location(layout: &[VertexAttribute]) -> Option<u32> {
    layout
        .iter()
        .enumerate()
        .find(|(position, attribute)| declares(&layout[..*position], attribute.location))
        .map(|(_, attribute)| attribute.location)
}

/// How many draw-set lists a dispatch of this graph supplies: one more
/// than the highest list index a pass names. Every index below that
/// must be named by some pass, so a dispatch never carries a list that
/// nothing draws and whose ids nothing checks. Two passes may name one
/// list.
pub fn list_slots(passes: &[PassPlan]) -> Result<u32, String> {
    let mut named: Vec<u32> = passes.iter().filter_map(|pass| pass.stage.draw_sets()).map(|plan| plan.list).collect();
    named.sort_unstable();
    named.dedup();

    for (expected, &list) in (0u32..).zip(&named) {
        if list != expected {
            return Err(format!(
                "a pass names draw-set list {list}, but no pass names list {expected} — the lists a program's passes \
                 name are numbered from 0 with none left out",
            ));
        }
    }
    Ok(u32::try_from(named.len()).expect("distinct list indices fit u32"))
}

#[cfg(test)]
mod tests {
    use crate::runtime::program::validate::validate;
    use crate::{
        Blend, Cull, DepthExtent, DepthSpec, DepthUse, DepthWrite, DrawSetsPass, Mips, OutputSlot, PassLoad, PassStage,
        ProgramPass, ProgramRegister, Samples, Sampling, SlotExtent, SlotShape, SlotSpec, TextureFormat,
        VertexAttribute, VertexFormat, Wrap,
    };

    /// A vertex stage reading one per-vertex location (0) and two
    /// per-instance ones (4 and 5).
    const MODULE: &str = r"
struct Placed {
    @builtin(position) position: vec4<f32>,
    @location(0) color: vec4<f32>,
}

@vertex
fn vs_placed(@location(0) position: vec3<f32>, @location(4) offset: vec2<f32>, @location(5) color: vec4<f32>) -> Placed {
    return Placed(vec4<f32>(position.xy + offset, position.z, 1.0), color);
}

@fragment
fn fs_color(@location(0) color: vec4<f32>) -> @location(0) vec4<f32> {
    return color;
}

@fragment
fn fs_nothing() {}
";

    fn vertex_layout() -> Vec<VertexAttribute> {
        vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }]
    }

    fn instance_layout() -> Vec<VertexAttribute> {
        vec![
            VertexAttribute { location: 4, format: VertexFormat::Float32x2 },
            VertexAttribute { location: 5, format: VertexFormat::Unorm8x4 },
        ]
    }

    fn stage() -> DrawSetsPass {
        DrawSetsPass {
            vertex_entry_point: "vs_placed".to_owned(),
            vertex_layout: vertex_layout(),
            instance_layout: instance_layout(),
            draw_sets: 0,
            cull: Cull::None,
            depth: None,
            load: PassLoad::Clear,
        }
    }

    fn pass(stage: DrawSetsPass) -> ProgramPass {
        ProgramPass {
            stage: PassStage::DrawSets(stage),
            blend: Blend::Alpha,
            entry_point: "fs_color".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    fn program(passes: Vec<ProgramPass>) -> ProgramRegister {
        ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![SlotSpec {
                format: TextureFormat::Rgba8,
                shape: SlotShape::Target(SlotExtent::Full),
                sampling: Sampling::Filtered { wrap: Wrap::Clamp, mips: Mips::Base },
            }],
            transients: Vec::new(),
            geometries: Vec::new(),
            depth_transients: vec![DepthSpec {
                extent: DepthExtent::Output(SlotExtent::Divided { divisor: 2 }),
                samples: Samples::One,
            }],
            passes,
        }
    }

    fn rejection(mail: &ProgramRegister) -> String {
        validate(mail).expect_err("the program must be refused")
    }

    /// The bug: the interface check consults the vertex layout alone, so
    /// a stage reading a per-instance location is refused as reading a
    /// location nothing declares, and no instanced program registers.
    #[test]
    fn a_stage_reading_instance_locations_registers() {
        let plan = validate(&program(vec![pass(stage())])).expect("the two-buffer program validates");

        assert_eq!(plan.draw_set_lists, 1);
    }

    /// The bug: a location both layouts declare registers, and the
    /// pipeline then feeds that one shader input from whichever buffer
    /// wgpu happens to accept, or fails as an opaque pipeline error.
    #[test]
    fn a_location_declared_by_both_layouts_is_refused() {
        let mut instance_layout = instance_layout();
        instance_layout.push(VertexAttribute { location: 0, format: VertexFormat::Float32 });

        let reason = rejection(&program(vec![pass(DrawSetsPass { instance_layout, ..stage() })]));

        assert!(reason.contains("both declare location 0"), "shared-location class: {reason}");
    }

    /// The bug: an instance location's WGSL type is not checked against
    /// its format, so the stage reads the record's bytes as a different
    /// quantity (here four integers read as a normalized colour).
    #[test]
    fn a_wrong_type_at_an_instance_location_is_refused() {
        let instance_layout = vec![
            VertexAttribute { location: 4, format: VertexFormat::Float32x2 },
            VertexAttribute { location: 5, format: VertexFormat::Uint8x4 },
        ];

        let reason = rejection(&program(vec![pass(DrawSetsPass { instance_layout, ..stage() })]));

        assert!(reason.contains("@location(5)"), "names the location: {reason}");
        assert!(reason.contains("consumed as vec4<u32>"), "instance-format class: {reason}");
    }

    /// The bug: passes naming lists 0 and 2 register, so every dispatch
    /// must carry a list 1 that no pass draws and no check reads.
    #[test]
    fn a_list_index_left_unnamed_is_refused() {
        let passes = vec![pass(stage()), pass(DrawSetsPass { draw_sets: 2, ..stage() })];

        let reason = rejection(&program(passes));

        assert!(reason.contains("no pass names list 1"), "unnamed-list class: {reason}");
    }

    /// The bug: a draw-sets pass skips the depth rule a draw pass gets,
    /// so a depth slot of another extent reaches wgpu and comes back as
    /// an opaque `pipeline creation failed` or a lost frame.
    #[test]
    fn a_depth_slot_of_another_extent_is_refused() {
        let depth = Some(DepthUse { slot: 0, write: DepthWrite::Write });

        let reason = rejection(&program(vec![pass(DrawSetsPass { depth, ..stage() })]));

        assert!(reason.contains("depth transient 0 declares extent"), "depth-extent class: {reason}");
    }

    /// A depth-only pass over depth slot 0 under `write`.
    fn depth_only(write: DepthWrite) -> ProgramPass {
        let depth = Some(DepthUse { slot: 0, write });
        ProgramPass {
            blend: Blend::Replace,
            entry_point: "fs_nothing".to_owned(),
            output: OutputSlot::None,
            ..pass(DrawSetsPass { depth, load: PassLoad::Load, ..stage() })
        }
    }

    /// The bugs: a depth-only draw-sets pass refused for having no
    /// output, so no instanced scene casts a shadow; and a `TestOnly`
    /// one registering, which rasterizes every listed draw each
    /// dispatch and writes nothing anywhere.
    #[test]
    fn a_depth_only_pass_must_write_its_depth_slot() {
        let plan = validate(&program(vec![depth_only(DepthWrite::Write), pass(stage())]))
            .expect("a depth-only pass that writes its slot validates");
        assert_eq!(plan.passes[0].output, None);

        let reason = rejection(&program(vec![depth_only(DepthWrite::TestOnly), pass(stage())]));
        assert!(
            reason.contains("pass 0: a depth-only pass declaring DepthWrite::TestOnly"),
            "test-only class: {reason}"
        );
    }
}
