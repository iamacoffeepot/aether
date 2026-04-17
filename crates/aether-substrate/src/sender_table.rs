// Per-component-instance mapping from guest-visible sender handles
// (opaque `u32`) to the real `SessionToken` the substrate received
// over the hub wire (ADR-0008). Handles are allocated when an inbound
// Claude-originated mail is dispatched to a component and resolved
// when the guest calls `reply_mail` to answer it.
//
// ADR-0013 §1: handles are monotonically increasing per instance.
// Exhaustion at 2³² dispatches is out of scope for V0; when it
// becomes real, the handle becomes a generational index.
//
// The table lives on `SubstrateCtx` rather than `Component` because
// the host fn touches it via `Caller::data_mut()`. Putting it there
// also means replace/drop on the component naturally clears it — the
// old `Store<SubstrateCtx>` is dropped and the table with it.

use std::collections::HashMap;

use aether_hub_protocol::SessionToken;

/// Sentinel passed to the guest's `receive` shim when the inbound
/// mail has no meaningful reply target (component-originated mail, or
/// broadcast origin — ADR-0013 §1). A `reply_mail` call with this
/// handle fails with the "unknown handle" status.
pub const SENDER_NONE: u32 = u32::MAX;

/// Maintains the handle→token map for one component instance.
#[derive(Debug, Default)]
pub struct SenderTable {
    entries: HashMap<u32, SessionToken>,
    next: u32,
}

impl SenderTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh handle bound to `token`. The returned handle
    /// is never `SENDER_NONE` — wraps past it silently.
    pub fn allocate(&mut self, token: SessionToken) -> u32 {
        // Skip the sentinel. In practice `next` never hits `u32::MAX`
        // before the instance is replaced/dropped; this is hygiene.
        if self.next == SENDER_NONE {
            self.next = 0;
        }
        let handle = self.next;
        self.next = self.next.wrapping_add(1);
        self.entries.insert(handle, token);
        handle
    }

    /// Look up the token for a guest-supplied handle. Returns `None`
    /// for `SENDER_NONE`, for handles that were never allocated, and
    /// (once implemented) for handles whose session is known-gone.
    pub fn resolve(&self, handle: u32) -> Option<SessionToken> {
        if handle == SENDER_NONE {
            return None;
        }
        self.entries.get(&handle).copied()
    }
}

#[cfg(test)]
mod tests {
    use aether_hub_protocol::Uuid;

    use super::*;

    fn token(byte: u8) -> SessionToken {
        SessionToken(Uuid::from_bytes([byte; 16]))
    }

    #[test]
    fn allocate_increments_and_roundtrips() {
        let mut t = SenderTable::new();
        let h0 = t.allocate(token(1));
        let h1 = t.allocate(token(2));
        assert_ne!(h0, h1);
        assert_eq!(t.resolve(h0), Some(token(1)));
        assert_eq!(t.resolve(h1), Some(token(2)));
    }

    #[test]
    fn resolve_sentinel_is_none() {
        let t = SenderTable::new();
        assert!(t.resolve(SENDER_NONE).is_none());
    }

    #[test]
    fn resolve_unknown_handle_is_none() {
        let mut t = SenderTable::new();
        let _ = t.allocate(token(7));
        assert!(t.resolve(9999).is_none());
    }

    #[test]
    fn allocate_skips_sentinel_on_wrap() {
        let mut t = SenderTable::new();
        t.next = SENDER_NONE;
        let h = t.allocate(token(3));
        // First handle after the wrap is 0 — the sentinel is never
        // handed out.
        assert_eq!(h, 0);
        assert_ne!(h, SENDER_NONE);
    }
}
