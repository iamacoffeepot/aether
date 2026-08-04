//! The drawing's flow as authored passes (iamacoffeepot/aether#4387,
//! ADR-0171): the two pointwise ends of
//! [`image::structure_tensor_flow`](crate::easel::image::structure_tensor_flow),
//! with the blur chain the wash already owns doing the softening and the
//! three poolings between them.
//!
//! The CPU solve stays the oracle. `flow.wgsl` carries the op-by-op
//! mapping; this module owns the entry-point names, the one-word selector
//! both ends read, and the pass builders the coat sequencer composes.

use aether_render::{InputSlot, OutputSlot, PassStage, ProgramPass, SlotExtent, SlotSpec, TextureFormat};

/// The flow WGSL. Never registered alone — the wash program's
/// [`module`](super::wash::module) concatenates it with its siblings.
pub const FLOW_WGSL: &str = include_str!("flow.wgsl");

/// Entry point forming one structure-tensor component.
pub const TENSOR_ENTRY: &str = "fs_flow_tensor";

/// Entry point reading one answer out of the pooled tensor.
pub const RESOLVE_ENTRY: &str = "fs_flow_resolve";

/// The three components of the symmetric tensor, in the order
/// [`Channel`] selects them.
pub const COMPONENTS: [Channel; 3] = [Channel::First, Channel::Second, Channel::Third];

/// Which component a tensor pass forms, or which answer a resolve pass
/// carries out. One selector serves both ends because both are a choice
/// among three off one shared solve.
#[derive(Clone, Copy)]
pub enum Channel {
    /// `xx` forming, `flow.x` resolving.
    First,
    /// `xy` forming, `flow.y` resolving.
    Second,
    /// `yy` forming, `coherence` resolving.
    Third,
}

/// A flow plane's slot: full-extent `R32Float`. The tensor components
/// are products of gradients of a coverage plane — small enough that an
/// eight-bit intermediate would quantize the orientation away before it
/// was ever pooled.
pub fn plane_slot() -> SlotSpec {
    SlotSpec { format: TextureFormat::R32Float, extent: SlotExtent::Full }
}

/// Uniform window for both ends — the WGSL `FlowSelectParams` block.
pub struct SelectUniforms {
    pub channel: Channel,
}

impl SelectUniforms {
    pub const BYTES: u32 = 4;

    pub fn encode(&self) -> [u8; Self::BYTES as usize] {
        (self.channel as u32).to_le_bytes()
    }
}

/// One component of the structure tensor, over the softened coverage.
pub fn tensor_pass(soft: InputSlot, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass(TENSOR_ENTRY, vec![soft], output, uniform_offset)
}

/// One answer read out of the pooled tensor: the minor eigenvector's two
/// components, or the eigenvalue split.
pub fn resolve_pass(pooled: [InputSlot; 3], output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    pass(RESOLVE_ENTRY, pooled.to_vec(), output, uniform_offset)
}

fn pass(entry_point: &str, inputs: Vec<InputSlot>, output: OutputSlot, uniform_offset: u32) -> ProgramPass {
    ProgramPass {
        stage: PassStage::Fragment,
        entry_point: entry_point.to_owned(),
        inputs,
        output,
        uniform_offset,
        uniform_length: SelectUniforms::BYTES,
        repeat: None,
    }
}
