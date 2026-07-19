//! Runtime backend selection for `aether.clipboard`.

/// Backend composed by the chassis for [`ClipboardCapability`](super::ClipboardCapability).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardConfig {
    /// Use the operating system clipboard through `arboard`.
    System,
    /// Use deterministic process-local text storage.
    InMemory,
}
