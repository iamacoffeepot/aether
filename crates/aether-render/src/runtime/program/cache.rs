//! Per-program dispatch cache: everything [`super::record`] derives that
//! is a pure function of the validated plan, the dispatch's reference
//! extent, and the identities of the resources it binds. A steady-state
//! repaint re-sends the same bindings every frame, so rebuilding this
//! per dispatch spent two `create_bind_group` calls per pass on an
//! answer that had not changed since the previous frame.
//!
//! Each piece is invalidated by exactly the event that can change it:
//!
//! - [`PlanLayout`] — the uniform staging arrangement and the
//!   clear/load sequencing — depends only on the plan and the device's
//!   uniform offset alignment, so it is built once per program and
//!   outlives every dispatch.
//! - [`ExtentLayout`] — the transient and depth-slot pool assignments —
//!   depends additionally on the reference extent, so it is keyed on it
//!   and rebuilt when the output binding's size changes (a resize).
//! - A binding's texture view is keyed on the texture id it was created
//!   from. Ids are never recycled by [`super::super::texture::TextureRegistry`],
//!   so a given id names one texture for the session; `ensure_realized`
//!   uploads a dirtied `update_texture` into that same texture, which the
//!   cached view already names.
//! - A pass's input bind group is keyed on the resolved identity of every
//!   slot it samples — the texture id for a binding, the pool class and
//!   physical index for a transient — so rebinding, a resize, or a pool
//!   class change rebuilds it and nothing else does.
//! - The uniform bind groups name the program's staging buffer, so they
//!   are held with it and rebuilt only when it is reallocated to grow.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};

use aether_substrate::render::PROGRAM_DEPTH_FORMAT;

use super::super::texture::{TextureRegistry, wgpu_texture_format};
use super::validate::{ProgramPlan, ResolvedSlot, resolve_extent};
use super::{PassGpu, TransientKey};

/// One transient's physical allocation for a dispatch: which pool class
/// it draws from and which slot within that class it occupies.
pub(super) struct TransientAssignment {
    pub key: TransientKey,
    pub physical: usize,
}

/// The staging arrangement and clear sequencing for one program: a pure
/// function of its plan and the device's uniform offset alignment.
pub(super) struct PlanLayout {
    /// Total bytes one dispatch stages, copy-alignment padding included.
    pub staging_bytes: usize,
    /// Per pass, the aligned staging offset of each repeat iteration.
    pub iteration_offsets: Vec<Vec<u32>>,
    /// Per pass, whether it is the dispatch's first write to its output
    /// slot — the pass that clears rather than loads it.
    pub clears_output: Vec<bool>,
    /// Per pass, whether it is the dispatch's first reference to the
    /// depth slot it names, and so clears it.
    pub clears_depth: Vec<bool>,
}

impl PlanLayout {
    /// Lay the uniform windows out into offset-aligned iterations and
    /// walk the passes once for the clear/load sequencing. Both mirror
    /// the arrangement `check_encode_budget` bounded at register time.
    pub fn build(plan: &ProgramPlan, passes_gpu: &[PassGpu], align: usize) -> Self {
        let mut staging_bytes = 0usize;
        let mut iteration_offsets = Vec::with_capacity(plan.passes.len());
        for (pass_plan, pass_gpu) in plan.passes.iter().zip(passes_gpu) {
            let bound = usize::try_from(pass_gpu.bound_uniform_bytes).expect("bound uniform bytes fit usize");
            let mut offsets = Vec::with_capacity(pass_plan.repeat_count as usize);
            for _ in 0..pass_plan.repeat_count {
                let aligned = staging_bytes.next_multiple_of(align);
                staging_bytes = aligned + bound;
                offsets.push(u32::try_from(aligned).expect("staged uniform offset fits u32"));
            }
            iteration_offsets.push(offsets);
        }
        let copy_alignment = usize::try_from(wgpu::COPY_BUFFER_ALIGNMENT).expect("copy alignment fits usize");
        staging_bytes = staging_bytes.next_multiple_of(copy_alignment);

        let mut written: Vec<ResolvedSlot> = Vec::new();
        let clears_output = plan
            .passes
            .iter()
            .map(|pass| {
                let first = !written.contains(&pass.output);
                if first {
                    written.push(pass.output);
                }
                first
            })
            .collect();

        let mut depth_seen: Vec<u32> = Vec::new();
        let clears_depth = plan
            .passes
            .iter()
            .map(|pass| {
                pass.draw.as_ref().and_then(|draw| draw.depth).is_some_and(|slot| {
                    let first = !depth_seen.contains(&slot);
                    if first {
                        depth_seen.push(slot);
                    }
                    first
                })
            })
            .collect();

        Self { staging_bytes, iteration_offsets, clears_output, clears_depth }
    }
}

/// The pool assignments for one reference extent. Every declared extent
/// resolves against the output binding's size, so a resize is the one
/// event that moves a transient to a different pool class.
pub(super) struct ExtentLayout {
    pub reference: (u32, u32),
    pub assignments: Vec<Option<TransientAssignment>>,
    pub depth_assignments: Vec<Option<TransientAssignment>>,
}

impl ExtentLayout {
    pub fn build(plan: &ProgramPlan, reference: (u32, u32)) -> Self {
        Self {
            reference,
            assignments: assign_transients(plan, reference),
            depth_assignments: assign_depth_transients(plan, reference),
        }
    }
}

/// What one input slot resolved to, at the granularity that decides
/// which GPU view a bind group entry names. A binding resolves to a
/// texture id — never recycled, and realized once — and a transient to
/// its pool class and physical index, which name a view the pool only
/// ever appends to.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum BoundInput {
    Binding(u32),
    Transient(TransientKey, usize),
}

/// One pass's cached input bind group, with the resolved identities it
/// was built from.
struct PassInputs {
    key: Vec<BoundInput>,
    group: wgpu::BindGroup,
}

/// The program's uniform staging buffer and the per-pass bind groups
/// that name it. Each group binds the buffer's head with its pass's
/// bound window size and is indexed by dynamic offset, so it survives
/// every dispatch the buffer fits — the groups are rebuilt exactly when
/// the buffer is reallocated to grow, because that is the one event that
/// changes the resource they name.
struct UniformCache {
    buffer: wgpu::Buffer,
    groups: Vec<wgpu::BindGroup>,
}

/// Everything one registered program carries across its dispatches.
/// Held on the program's registry entry, so `program_destroy` releases
/// it with the program and a re-register starts empty under a fresh id.
pub(super) struct DispatchCache {
    layout: Option<PlanLayout>,
    extent: Option<ExtentLayout>,
    /// Per binding index, the texture id its cached view was created
    /// from. Sized to the plan's binding count, so the views a dispatch
    /// stops naming are released when that slot is next rebound.
    binding_views: Vec<Option<(u32, wgpu::TextureView)>>,
    pass_inputs: Vec<Option<PassInputs>>,
    uniforms: Option<UniformCache>,
    /// Reused staging bytes, refilled from each dispatch's blob.
    staging: Vec<u8>,
}

impl DispatchCache {
    pub fn new(plan: &ProgramPlan) -> Self {
        Self {
            layout: None,
            extent: None,
            binding_views: (0..plan.bindings.len()).map(|_| None).collect(),
            pass_inputs: (0..plan.passes.len()).map(|_| None).collect(),
            uniforms: None,
            staging: Vec::new(),
        }
    }

    /// Build the plan-only layout on the first dispatch that reaches the
    /// encode; every later dispatch reuses it.
    pub fn ensure_layout(&mut self, plan: &ProgramPlan, passes_gpu: &[PassGpu], align: usize) {
        self.layout.get_or_insert_with(|| PlanLayout::build(plan, passes_gpu, align));
    }

    /// Build the pool assignments for `reference`, reusing the held ones
    /// while the reference extent has not moved since the last dispatch.
    pub fn ensure_extent_layout(&mut self, plan: &ProgramPlan, reference: (u32, u32)) {
        if self.extent.as_ref().is_none_or(|held| held.reference != reference) {
            self.extent = Some(ExtentLayout::build(plan, reference));
        }
    }

    /// Refresh the cached view for every binding whose texture id has
    /// changed since the last dispatch, leaving the rest untouched.
    /// Runs after realization, so every id names a realized texture.
    pub fn refresh_binding_views(&mut self, textures: &TextureRegistry, bindings: &[u32]) {
        for (slot, &texture_id) in self.binding_views.iter_mut().zip(bindings) {
            if slot.as_ref().is_some_and(|(held, _)| *held == texture_id) {
                continue;
            }
            let view = textures.entries[&texture_id]
                .realized
                .as_ref()
                .expect("realized before encode")
                .texture()
                .create_view(&wgpu::TextureViewDescriptor::default());
            *slot = Some((texture_id, view));
        }
    }

    /// Split the cache into the pieces the encode borrows disjointly:
    /// the layouts and views it only reads, and the bind-group and
    /// staging stores it writes.
    pub fn split(&mut self) -> CacheParts<'_> {
        CacheParts {
            layout: self.layout.as_ref().expect("layout built before the encode"),
            extent: self.extent.as_ref().expect("extent layout built before the encode"),
            binding_views: &self.binding_views,
            pass_inputs: &mut self.pass_inputs,
            uniforms: &mut self.uniforms,
            staging: &mut self.staging,
        }
    }
}

/// The cache's fields borrowed apart, so the encode can hold a binding's
/// view and both of a pass's bind groups at once — the shape
/// `record_program_pass` takes.
pub(super) struct CacheParts<'a> {
    pub layout: &'a PlanLayout,
    pub extent: &'a ExtentLayout,
    binding_views: &'a [Option<(u32, wgpu::TextureView)>],
    pass_inputs: &'a mut [Option<PassInputs>],
    uniforms: &'a mut Option<UniformCache>,
    staging: &'a mut Vec<u8>,
}

impl<'a> CacheParts<'a> {
    /// The cached view for a binding index. Its lifetime is the cache's,
    /// not this borrow's, so an entry list built from it survives the
    /// `&mut` that stores the bind group it feeds.
    pub fn binding_view(&self, binding: u32) -> &'a wgpu::TextureView {
        let (_, view) =
            self.binding_views[binding as usize].as_ref().expect("every binding's view refreshed before the encode");
        view
    }

    /// Refill the staging bytes from this dispatch's blob at the
    /// layout's fixed offsets, then upload them. The buffer is
    /// reallocated only when the plan's staging bytes no longer fit,
    /// and that reallocation is what rebuilds the uniform bind groups —
    /// they name the buffer, so nothing else can invalidate them.
    pub fn upload_uniforms(
        &mut self,
        gpu_device: &wgpu::Device,
        queue: &wgpu::Queue,
        plan: &ProgramPlan,
        passes_gpu: &[PassGpu],
        uniforms: &[u8],
    ) {
        self.staging.clear();
        self.staging.resize(self.layout.staging_bytes, 0);
        for (pass_plan, offsets) in plan.passes.iter().zip(&self.layout.iteration_offsets) {
            let length = pass_plan.uniform_length as usize;
            if length == 0 {
                continue;
            }
            for (iteration, &offset) in offsets.iter().enumerate() {
                let start = pass_plan.uniform_offset as usize + iteration * pass_plan.uniform_stride as usize;
                let offset = offset as usize;
                self.staging[offset..offset + length].copy_from_slice(&uniforms[start..start + length]);
            }
        }

        let staging_bytes = self.staging.len() as u64;
        if self.uniforms.as_ref().is_none_or(|held| held.buffer.size() < staging_bytes) {
            let buffer = gpu_device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("aether program uniform staging"),
                size: staging_bytes,
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            let groups = passes_gpu
                .iter()
                .map(|pass_gpu| {
                    gpu_device.create_bind_group(&wgpu::BindGroupDescriptor {
                        label: Some("aether program uniform bind group"),
                        layout: &pass_gpu.uniform_layout,
                        entries: &[wgpu::BindGroupEntry {
                            binding: 0,
                            resource: wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                                buffer: &buffer,
                                offset: 0,
                                size: wgpu::BufferSize::new(pass_gpu.bound_uniform_bytes),
                            }),
                        }],
                    })
                })
                .collect();
            *self.uniforms = Some(UniformCache { buffer, groups });
        }

        let held = self.uniforms.as_ref().expect("uniform buffer ensured above");
        queue.write_buffer(&held.buffer, 0, self.staging);
    }

    /// Whether a pass's cached input bind group was built from a
    /// different set of resolved slots than `key` — the only condition
    /// under which it has to be rebuilt.
    pub fn inputs_stale(&self, pass: usize, key: &[BoundInput]) -> bool {
        self.pass_inputs[pass].as_ref().is_none_or(|held| held.key != key)
    }

    /// Adopt a freshly built input bind group as a pass's cached one.
    pub fn store_inputs(&mut self, pass: usize, key: &[BoundInput], group: wgpu::BindGroup) {
        self.pass_inputs[pass] = Some(PassInputs { key: key.to_vec(), group });
    }

    /// The uniform bind group for a pass, valid once
    /// [`Self::upload_uniforms`] has run for this dispatch.
    pub fn uniform_group(&self, pass: usize) -> &wgpu::BindGroup {
        &self.uniforms.as_ref().expect("uniforms uploaded before the encode").groups[pass]
    }

    /// The input bind group for a pass, valid once a miss has been
    /// stored through [`Self::store_inputs`].
    pub fn inputs_group(&self, pass: usize) -> &wgpu::BindGroup {
        &self.pass_inputs[pass].as_ref().expect("a stale inputs group is rebuilt before the encode").group
    }
}

/// Assign each live transient a physical texture in its resolved
/// (extent, format) class, reusing a physical slot once its previous
/// holder's live range has ended — strictly before the next holder's
/// first write, so a pass never samples a transient the same physical
/// texture is attached to. A ping-pong chain of any length settles on
/// two allocations per class.
fn assign_transients(plan: &ProgramPlan, reference: (u32, u32)) -> Vec<Option<TransientAssignment>> {
    let mut order: Vec<usize> =
        (0..plan.transients.len()).filter(|&t| plan.transients[t].first_write.is_some()).collect();
    order.sort_by_key(|&t| plan.transients[t].first_write.expect("filtered to written transients"));

    let mut free: HashMap<TransientKey, BinaryHeap<Reverse<(u32, usize)>>> = HashMap::new();
    let mut allocated: HashMap<TransientKey, usize> = HashMap::new();
    let mut assignments: Vec<Option<TransientAssignment>> = (0..plan.transients.len()).map(|_| None).collect();
    for t in order {
        let live = &plan.transients[t];
        let first_write = live.first_write.expect("filtered to written transients");
        let last_use = live.last_use.expect("a written transient has a live range");
        let (width, height) = resolve_extent(live.spec.extent, reference);
        let key = (width, height, wgpu_texture_format(live.spec.format));

        let heap = free.entry(key).or_default();
        let physical = match heap.peek() {
            Some(Reverse((available_from, physical))) if *available_from <= first_write => {
                let physical = *physical;
                heap.pop();
                physical
            }
            _ => {
                let count = allocated.entry(key).or_insert(0);
                let physical = *count;
                *count += 1;
                physical
            }
        };
        free.entry(key).or_default().push(Reverse((last_use + 1, physical)));
        assignments[t] = Some(TransientAssignment { key, physical });
    }
    assignments
}

/// Assign each depth slot a physical `Depth32Float` texture in its
/// resolved-extent class, skipping slots no pass names. Unlike color
/// transients these are not liveness-packed: sharing a depth buffer is
/// the declaration's whole point (two passes naming one slot is how
/// occlusion agrees between them), so two *distinct* slots must never
/// land on one physical texture however disjoint their use looks.
fn assign_depth_transients(plan: &ProgramPlan, reference: (u32, u32)) -> Vec<Option<TransientAssignment>> {
    let mut allocated: HashMap<TransientKey, usize> = HashMap::new();
    plan.depth_transients
        .iter()
        .enumerate()
        .map(|(slot, extent)| {
            let slot = u32::try_from(slot).expect("depth slot index fits u32");
            let named = plan.passes.iter().any(|pass| pass.draw.as_ref().is_some_and(|draw| draw.depth == Some(slot)));
            named.then(|| {
                let (width, height) = resolve_extent(*extent, reference);
                let key = (width, height, PROGRAM_DEPTH_FORMAT);
                let physical = allocated.entry(key).or_insert(0);
                let assigned = *physical;
                *physical += 1;
                TransientAssignment { key, physical: assigned }
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        DrawPass, GeometrySlotSpec, InputSlot, OutputSlot, PassLoad, PassStage, ProgramPass, ProgramRegister,
        SlotExtent, SlotSpec, TextureFormat, VertexAttribute, VertexFormat,
    };

    const MODULE: &str = r"
@group(1) @binding(0) var source_texture: texture_2d<f32>;
@group(1) @binding(1) var source_sampler: sampler;

@fragment
fn fs_copy(@location(0) uv: vec2<f32>) -> @location(0) vec4<f32> {
    return textureSample(source_texture, source_sampler, uv);
}
";

    fn copy_pass(input: InputSlot, output: OutputSlot) -> ProgramPass {
        ProgramPass {
            stage: PassStage::Fragment,
            entry_point: "fs_copy".to_owned(),
            inputs: vec![input],
            output,
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    /// The interval reuse behind the ADR's pooling claim: a chain of
    /// hops through same-class transients settles on two physical
    /// allocations, and reuse begins strictly after the prior holder's
    /// last read. The named bugs: an `available_from <= first_write`
    /// off-by-one that hands a still-live transient's texture to the
    /// pass sampling it (chain corruption), or a policy that never
    /// reuses and allocates one texture per hop.
    #[test]
    fn ping_pong_chain_settles_on_two_physical_allocations() {
        let full = SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full };
        let mail = ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full, full],
            transients: vec![full; 3],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![
                copy_pass(InputSlot::Binding { index: 0 }, OutputSlot::Transient { index: 0 }),
                copy_pass(InputSlot::Transient { index: 0 }, OutputSlot::Transient { index: 1 }),
                copy_pass(InputSlot::Transient { index: 1 }, OutputSlot::Transient { index: 2 }),
                copy_pass(InputSlot::Transient { index: 2 }, OutputSlot::Binding { index: 1 }),
            ],
        };
        let plan = super::super::validate::validate(&mail).expect("chain validates");

        let assignments = assign_transients(&plan, (64, 48));
        let physicals: Vec<usize> =
            assignments.iter().map(|a| a.as_ref().expect("all transients live").physical).collect();
        // Transient 0 is read at pass 1 while transient 1 is written, so
        // they must differ; transient 0's range ends at pass 1, freeing
        // its texture for transient 2's write at pass 2.
        assert_eq!(physicals, vec![0, 1, 0]);
    }

    /// A resize moves every `Full` transient to a different pool class,
    /// which is what makes the reference extent part of the cache key.
    /// The named bug: keying the pool assignments on the plan alone, so
    /// a resized dispatch keeps sampling the previous size's textures.
    #[test]
    fn transient_pool_class_follows_the_reference_extent() {
        let full = SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full };
        let mail = ProgramRegister {
            wgsl: MODULE.to_owned(),
            bindings: vec![full, full],
            transients: vec![full],
            geometries: Vec::new(),
            depth_transients: Vec::new(),
            passes: vec![
                copy_pass(InputSlot::Binding { index: 0 }, OutputSlot::Transient { index: 0 }),
                copy_pass(InputSlot::Transient { index: 0 }, OutputSlot::Binding { index: 1 }),
            ],
        };
        let plan = super::super::validate::validate(&mail).expect("chain validates");

        let small = ExtentLayout::build(&plan, (64, 48));
        let large = ExtentLayout::build(&plan, (128, 96));
        let class = |layout: &ExtentLayout| layout.assignments[0].as_ref().expect("transient live").key;
        assert_ne!(class(&small), class(&large), "a resized dispatch must not reuse the previous size's pool class");
    }

    const DRAW_MODULE: &str = r"
@vertex
fn vs_flat(@location(0) position: vec3<f32>) -> @builtin(position) vec4<f32> {
    return vec4<f32>(position, 1.0);
}

@fragment
fn fs_opaque() -> @location(0) vec4<f32> {
    return vec4<f32>(1.0, 1.0, 1.0, 1.0);
}
";

    fn draw_pass(depth: Option<u32>) -> ProgramPass {
        ProgramPass {
            stage: PassStage::Draw(DrawPass {
                vertex_entry_point: "vs_flat".to_owned(),
                geometry: 0,
                depth,
                load: PassLoad::Clear,
            }),
            entry_point: "fs_opaque".to_owned(),
            inputs: Vec::new(),
            output: OutputSlot::Binding { index: 0 },
            uniform_offset: 0,
            uniform_length: 0,
            repeat: None,
        }
    }

    /// ADR-0171 depth pooling: two passes naming one depth slot share
    /// one physical texture (that sharing is what makes their occlusion
    /// agree), two *distinct* slots never do however disjoint their use
    /// looks, and a declared slot no pass names allocates nothing. The
    /// named bugs: liveness-packing depth the way color transients are
    /// packed, which would alias two slots onto one buffer and let one
    /// pass's depth occlude another's geometry; and a named slot left
    /// unassigned, which panics the encode path that resolves its view.
    #[test]
    fn depth_slots_share_by_name_and_never_alias() {
        let full = SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full };
        let mail = ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full],
            transients: Vec::new(),
            geometries: vec![GeometrySlotSpec {
                layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }],
            }],
            depth_transients: vec![SlotExtent::Full; 3],
            passes: vec![draw_pass(Some(0)), draw_pass(Some(0)), draw_pass(Some(1))],
        };
        let plan = super::super::validate::validate(&mail).expect("draw graph validates");

        let assignments = assign_depth_transients(&plan, (64, 48));
        let physicals: Vec<Option<usize>> =
            assignments.iter().map(|slot| slot.as_ref().map(|assigned| assigned.physical)).collect();
        assert_eq!(physicals, vec![Some(0), Some(1), None]);
        assert_eq!(
            assignments[0].as_ref().expect("slot 0 named").key,
            assignments[1].as_ref().expect("slot 1 named").key,
            "same-extent depth slots share a pool class, so only the physical index may separate them",
        );
    }

    /// The clear/load sequencing the encode used to rediscover per
    /// dispatch by accumulating a `written` list: the first pass to
    /// write a slot clears it, every later pass loads it, and depth
    /// follows the same rule per named slot. The named bug: hoisting
    /// this out of the per-dispatch loop and clearing on every pass,
    /// which wipes the accumulation a chain is built on.
    #[test]
    fn clear_sequencing_marks_only_the_first_write_of_each_slot() {
        let full = SlotSpec { format: TextureFormat::Rgba8, extent: SlotExtent::Full };
        let mail = ProgramRegister {
            wgsl: DRAW_MODULE.to_owned(),
            bindings: vec![full],
            transients: Vec::new(),
            geometries: vec![GeometrySlotSpec {
                layout: vec![VertexAttribute { location: 0, format: VertexFormat::Float32x3 }],
            }],
            depth_transients: vec![SlotExtent::Full; 2],
            passes: vec![draw_pass(Some(0)), draw_pass(Some(0)), draw_pass(Some(1))],
        };
        let plan = super::super::validate::validate(&mail).expect("draw graph validates");
        // No pass GPU resources: the staging arrangement needs them, the
        // clear sequencing this pins is a function of the plan alone.
        let layout = PlanLayout::build(&plan, &[], 256);

        // All three passes write binding 0, so only the first clears it.
        assert_eq!(layout.clears_output, vec![true, false, false]);
        // Passes 0 and 1 share depth slot 0; pass 2 names slot 1 first.
        assert_eq!(layout.clears_depth, vec![true, false, true]);
    }
}
