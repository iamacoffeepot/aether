//! Offscreen color + depth target pair plus the lazy readback buffer
//! the capture path uses. Owned per-chassis (the `Gpu` struct holds a
//! `Targets`); resized by the chassis on surface change (desktop) or
//! a control mail (test-bench).

use super::DEPTH_FORMAT;

/// Sized offscreen color + depth pair plus a lazy readback buffer
/// for the capture path. The chassis-owned `Gpu` struct holds one of
/// these; `record_main_pass` and `prepare_capture_copy` borrow it.
pub struct Targets {
    pub(super) offscreen: OffscreenTarget,
    pub(super) depth: DepthTarget,
    /// Lazily-allocated readback buffer. Reallocated on resize or the
    /// first capture after dimensions change.
    pub(super) readback: Option<Readback>,
    /// Format the offscreen texture was created with. Capture-path
    /// readback uses this to decide whether to swizzle BGRA → RGBA
    /// (desktop's surface-derived format may be BGRA on some adapters).
    pub(super) color_format: wgpu::TextureFormat,
}

pub(super) struct OffscreenTarget {
    pub(super) texture: wgpu::Texture,
    pub(super) view: wgpu::TextureView,
    pub(super) width: u32,
    pub(super) height: u32,
}

pub(super) struct DepthTarget {
    /// Keep the texture alive; only the view is bound in the pass.
    #[allow(dead_code)]
    pub(super) texture: wgpu::Texture,
    pub(super) view: wgpu::TextureView,
}

pub(super) struct Readback {
    pub(super) buffer: wgpu::Buffer,
    pub(super) width: u32,
    pub(super) height: u32,
}

impl Targets {
    /// Allocate the offscreen color + depth pair at `width x height`.
    /// `color_format` lets desktop pick whatever the surface negotiated
    /// (typically RGBA, sometimes BGRA on Vulkan); test-bench passes
    /// `Rgba8UnormSrgb` since there's no surface to query. Width and
    /// height are clamped to a minimum of 1 — wgpu rejects zero
    /// dimensions.
    pub fn new(
        device: &wgpu::Device,
        color_format: wgpu::TextureFormat,
        width: u32,
        height: u32,
    ) -> Self {
        let width = width.max(1);
        let height = height.max(1);
        Self {
            offscreen: create_offscreen(device, color_format, width, height),
            depth: create_depth(device, width, height),
            readback: None,
            color_format,
        }
    }

    /// Reallocate the offscreen + depth textures at the new size and
    /// invalidate the readback buffer. No-op on zero dimensions
    /// (matches winit's `Resized(0, 0)` events on minimize).
    pub fn resize(&mut self, device: &wgpu::Device, width: u32, height: u32) {
        if width == 0 || height == 0 {
            return;
        }
        self.offscreen = create_offscreen(device, self.color_format, width, height);
        self.depth = create_depth(device, width, height);
        self.readback = None;
    }

    /// Width of the current offscreen color target.
    pub fn width(&self) -> u32 {
        self.offscreen.width
    }

    /// Height of the current offscreen color target.
    pub fn height(&self) -> u32 {
        self.offscreen.height
    }

    /// The `wgpu::TextureView` the main render pass attaches to.
    /// Exposed for chassis-side passes that want to draw into the
    /// same offscreen (e.g. test-bench's diagnostic clears).
    pub fn color_view(&self) -> &wgpu::TextureView {
        &self.offscreen.view
    }

    /// The offscreen color texture itself. Desktop reaches for this
    /// to encode a `copy_texture_to_texture` blit onto the swapchain.
    pub fn color_texture(&self) -> &wgpu::Texture {
        &self.offscreen.texture
    }

    /// Format the offscreen was created with. Capture's BGRA-vs-RGBA
    /// decision keys on this.
    pub fn color_format(&self) -> wgpu::TextureFormat {
        self.color_format
    }
}

pub(super) fn align_up(value: u32, alignment: u32) -> u32 {
    value.div_ceil(alignment) * alignment
}

fn create_offscreen(
    device: &wgpu::Device,
    format: wgpu::TextureFormat,
    width: u32,
    height: u32,
) -> OffscreenTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("offscreen color target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format,
        // RENDER_ATTACHMENT: the triangle pass writes here.
        // COPY_SRC: both desktop's swapchain blit and the readback
        // copy read from this texture.
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    OffscreenTarget {
        texture,
        view,
        width,
        height,
    }
}

fn create_depth(device: &wgpu::Device, width: u32, height: u32) -> DepthTarget {
    let texture = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("depth target"),
        size: wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: DEPTH_FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    });
    let view = texture.create_view(&wgpu::TextureViewDescriptor::default());
    DepthTarget { texture, view }
}
