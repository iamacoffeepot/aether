//! Capture-path readback: record an offscreen → buffer copy onto an
//! existing encoder, then map the buffer and encode a PNG. The two
//! halves split because desktop interleaves the capture copy between
//! its main pass and the swapchain blit; test-bench just submits
//! after the copy.

use super::COPY_ROW_ALIGN;
use super::targets::{Readback, Targets, align_up};

/// Dimensions + wire-format info captured at `copy_texture_to_buffer`
/// time and consumed after the buffer is mapped. Owned by the caller
/// across the submit boundary; `finish_capture` reads from the
/// `Targets`' readback buffer using these dims to strip row padding
/// and decide whether to swizzle BGRA → RGBA.
pub struct CaptureMeta {
    pub width: u32,
    pub height: u32,
    pub padded_row_bytes: u32,
    pub unpadded_row_bytes: u32,
    /// Format the offscreen was created with — kept on the meta so
    /// `finish_capture` doesn't need to re-borrow `Targets`.
    pub format: wgpu::TextureFormat,
}

/// Ensure a readback buffer exists sized to the offscreen, then
/// encode a `copy_texture_to_buffer` from offscreen → readback onto
/// `encoder`. The copy command is appended; the caller decides when
/// to submit and what else to encode beforehand.
///
/// Returns the meta dims + format `finish_capture` will need after
/// the GPU work completes.
pub fn prepare_capture_copy(
    device: &wgpu::Device,
    targets: &mut Targets,
    encoder: &mut wgpu::CommandEncoder,
) -> CaptureMeta {
    let width = targets.offscreen.width;
    let height = targets.offscreen.height;
    let bytes_per_pixel = 4u32;
    let unpadded_row_bytes = width * bytes_per_pixel;
    let padded_row_bytes = align_up(unpadded_row_bytes, COPY_ROW_ALIGN);
    let buffer_size = u64::from(padded_row_bytes) * u64::from(height);

    let needs_realloc = match &targets.readback {
        Some(rb) => rb.width != width || rb.height != height,
        None => true,
    };
    if needs_realloc {
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("capture readback"),
            size: buffer_size,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        targets.readback = Some(Readback {
            buffer,
            width,
            height,
        });
    }

    let readback = targets.readback.as_ref().expect("readback just set");

    encoder.copy_texture_to_buffer(
        wgpu::TexelCopyTextureInfo {
            texture: &targets.offscreen.texture,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        wgpu::TexelCopyBufferInfo {
            buffer: &readback.buffer,
            layout: wgpu::TexelCopyBufferLayout {
                offset: 0,
                bytes_per_row: Some(padded_row_bytes),
                rows_per_image: Some(height),
            },
        },
        wgpu::Extent3d {
            width,
            height,
            depth_or_array_layers: 1,
        },
    );

    CaptureMeta {
        width,
        height,
        padded_row_bytes,
        unpadded_row_bytes,
        format: targets.color_format(),
    }
}

/// Map the readback buffer (after the encoder's submit has run),
/// strip row padding, swizzle BGRA → RGBA when the offscreen is in
/// a BGRA format, and PNG-encode the bytes.
///
/// Blocks the calling thread on `device.poll(wait_indefinitely)` —
/// callers that can't tolerate the stall (the desktop render loop)
/// should not call this on the hot path; capture is one frame per
/// MCP request, not per frame.
pub fn finish_capture(
    device: &wgpu::Device,
    targets: &Targets,
    meta: &CaptureMeta,
) -> Result<Vec<u8>, String> {
    let readback = targets
        .readback
        .as_ref()
        .ok_or_else(|| "readback buffer missing".to_owned())?;

    let slice = readback.buffer.slice(..);
    let (tx, rx) = std::sync::mpsc::channel();
    slice.map_async(wgpu::MapMode::Read, move |res| {
        let _ = tx.send(res);
    });
    device
        .poll(wgpu::PollType::wait_indefinitely())
        .map_err(|e| format!("device poll: {e:?}"))?;
    rx.recv()
        .map_err(|e| format!("map channel dropped: {e}"))?
        .map_err(|e| format!("buffer map failed: {e:?}"))?;

    let mapped = slice.get_mapped_range();
    let mut rgba = Vec::with_capacity((meta.unpadded_row_bytes as usize) * (meta.height as usize));
    let swizzle_bgra = matches!(
        meta.format,
        wgpu::TextureFormat::Bgra8Unorm | wgpu::TextureFormat::Bgra8UnormSrgb
    );
    for row in 0..meta.height {
        let start = (row * meta.padded_row_bytes) as usize;
        let end = start + meta.unpadded_row_bytes as usize;
        let src = &mapped[start..end];
        if swizzle_bgra {
            for chunk in src.chunks_exact(4) {
                rgba.extend_from_slice(&[chunk[2], chunk[1], chunk[0], chunk[3]]);
            }
        } else {
            rgba.extend_from_slice(src);
        }
    }
    drop(mapped);
    readback.buffer.unmap();

    encode_png(&rgba, meta.width, meta.height)
}

fn encode_png(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    {
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgba);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder
            .write_header()
            .map_err(|e| format!("png header: {e}"))?;
        writer
            .write_image_data(rgba)
            .map_err(|e| format!("png write: {e}"))?;
    }
    Ok(out)
}
