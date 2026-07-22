//! Adapter-facing request DTO shared by the `aether.gemini` component's pure
//! body builder and its request handler (ADR-0050 §4, ADR-0159).
//!
//! The kind is the caller-stable contract; this request is the vendor-compat
//! shape the component converts its capability-owned wire kind into before it
//! builds the provider request body. It is adapter-facing, not a wire kind.

/// Adapter-facing Nano Banana image request. The gemini component reads
/// reference-image bytes from the supplied paths and runs per-model validation
/// before constructing this; the pure body builder turns it into the provider
/// request body. `aspect_ratio` rides as the provider's `W:H` string;
/// `reference_images` are the already-read reference bytes.
#[derive(Clone, Debug, Default)]
pub struct GeminiImageRequest {
    pub model: String,
    pub prompt: String,
    /// Provider `W:H` aspect-ratio string (e.g. `"16:9"`).
    pub aspect_ratio: String,
    /// Provider image-size string (`"512"` / `"1K"` / `"2K"` / `"4K"`),
    /// `None` when the caller left `image_size` unset.
    pub image_size: Option<String>,
    /// Provider `thinkingLevel` string (`"minimal"` / `"high"`), `None`
    /// when the caller left `thinking_level` unset.
    pub thinking_level: Option<String>,
    /// `thinkingConfig.includeThoughts`, `None` when unset.
    pub include_thoughts: Option<bool>,
    /// Whether to add the `google_search` grounding tool.
    pub use_grounding: bool,
    /// Reference-image bytes the cap read from the supplied paths.
    pub reference_images: Vec<Vec<u8>>,
}
