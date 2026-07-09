/// ADR-0016 §3: opt-in state migration payload. The substrate owns the
/// buffer from the moment `save_state` is called on the old instance
/// until the bundle is handed to the new instance via `on_rehydrate`
/// (or discarded if no successor consumes it). Both fields are opaque
/// to the substrate — the component owns versioning and the byte layout.
#[derive(Debug, Clone)]
pub struct StateBundle {
    pub version: u32,
    pub bytes: Vec<u8>,
}
