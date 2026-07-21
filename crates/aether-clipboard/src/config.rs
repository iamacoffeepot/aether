//! Runtime backend selection for `aether.clipboard`.

/// Composer-supplied backend selection for
/// [`ClipboardCapability`](super::ClipboardCapability) — the ADR-0156 §3
/// `Params` channel. The chassis picks the variant at compose (desktop wires
/// `System`, the substrate-harness `InMemory`); it is a composer choice, not an
/// operator-resolvable knob, so it is `Params`, not `Config`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardParams {
    /// Use the operating system clipboard through `arboard`.
    System,
    /// Use deterministic process-local text storage.
    InMemory,
}
