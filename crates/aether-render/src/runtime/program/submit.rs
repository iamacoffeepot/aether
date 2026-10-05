//! The frame's program passes, counted against the backend's command-buffer
//! ceiling and spread over as many queue submissions as they need.
//!
//! Every pass costs the backend two command buffers (the pass and the
//! resource-transition pass wgpu-core inserts ahead of it), and Metal holds
//! 4,096 command buffers created and not yet submitted, losing the device
//! at the 4,097th. A submission releases what it carries, so the executor
//! submits the frame's encoder after a fixed number of passes and goes on
//! in a fresh one. Submissions execute in order, so dispatch arrival order
//! and the rule that programs run before the passes that sample their
//! outputs hold across a boundary.

use std::{iter, mem};

use super::super::pipeline::RenderGpu;

/// Program passes one queue submission carries. Each pass costs two backend
/// command buffers and Metal holds 4,096 unsubmitted, so this leaves half of
/// them for the passes the frame records after the programs.
const PASSES_PER_SUBMISSION: u32 = 1024;

/// How many program passes the frame's current encoder holds.
#[derive(Default)]
pub(super) struct FramePasses {
    recorded: u32,
}

impl FramePasses {
    /// Called before each pass is recorded: when the encoder already holds
    /// a full submission's passes, submit it and continue in a fresh one,
    /// replaced in the caller's binding.
    pub(super) fn admit(&mut self, gpu: &RenderGpu, encoder: &mut wgpu::CommandEncoder) {
        if self.recorded == PASSES_PER_SUBMISSION {
            let fresh = gpu
                .device
                .create_command_encoder(&wgpu::CommandEncoderDescriptor { label: Some("aether program passes") });
            let full = mem::replace(encoder, fresh);
            gpu.queue.submit(iter::once(full.finish()));
            self.recorded = 0;
        }
        self.recorded += 1;
    }
}
