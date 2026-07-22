//! Similarity-reference image for the `aether.render` cap's `capture_frame`
//! handler (iamacoffeepot/aether#1780). The pumped render runtime resolves
//! the reference PNG synchronously off the render hot path and carries the
//! resulting [`ReferenceCapture`] on its pending capture until `on_frame`
//! scores it against the readback RGBA.

/// Pre-fetched reference image for a similarity check
/// (iamacoffeepot/aether#1780). The `RenderCapability` capture handler reads
/// the reference PNG from the assets directory before parking the capture,
/// keeping filesystem I/O off the render path; `on_frame` decodes the PNG
/// bytes and runs the MAE comparison against the captured frame.
pub struct ReferenceCapture {
    /// Raw PNG bytes read from the assets directory.
    pub png_bytes: Vec<u8>,
    /// Maximum normalised MAE `[0.0, 1.0]` that counts as a pass.
    pub threshold: f32,
}
