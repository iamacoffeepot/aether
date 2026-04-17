// Per-component-instance mapping from guest-visible sender handles
// (opaque `u32`) to the substrate-internal identity of whoever
// originated an inbound mail. Handles are allocated when a component
// receives non-broadcast mail and resolved when the guest calls
// `reply_mail` to answer it.
//
// ADR-0013 covered session origins; ADR-0017 widened the table so
// component-originated mail also produces a handle — a component can
// now reply to a runtime-discovered peer without knowing its name at
// init, using the same `ctx.reply` API regardless of who called.
//
// Handles are monotonically increasing per instance. Exhaustion at
// 2³² dispatches is out of scope for V0; when it becomes real, the
// handle becomes a generational index.
//
// The table lives on `SubstrateCtx` rather than `Component` because
// the host fn touches it via `Caller::data_mut()`. Putting it there
// also means replace/drop on the component naturally clears it — the
// old `Store<SubstrateCtx>` is dropped and the table with it.

use std::collections::HashMap;

use aether_hub_protocol::SessionToken;

use crate::mail::MailboxId;

/// Sentinel passed to the guest's `receive` shim when the inbound
/// mail has no meaningful reply target (broadcast origin — ADR-0013
/// §1). A `reply_mail` call with this handle fails with the "unknown
/// handle" status.
pub const SENDER_NONE: u32 = u32::MAX;

/// What a sender handle resolves to on the substrate side. The guest
/// sees only the opaque `u32` — the variant is purely host-side so
/// `reply_mail` can pick the right outbound route.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum SenderEntry {
    /// Reply routes over `HubOutbound` as a session-addressed frame.
    Session(SessionToken),
    /// Reply routes through the local `MailQueue` as ordinary
    /// component-to-component mail.
    Component(MailboxId),
}

/// Maintains the handle→entry map for one component instance.
#[derive(Debug, Default)]
pub struct SenderTable {
    entries: HashMap<u32, SenderEntry>,
    next: u32,
}

impl SenderTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh handle bound to `entry`. The returned handle
    /// is never `SENDER_NONE` — wraps past it silently.
    pub fn allocate(&mut self, entry: SenderEntry) -> u32 {
        // Skip the sentinel. In practice `next` never hits `u32::MAX`
        // before the instance is replaced/dropped; this is hygiene.
        if self.next == SENDER_NONE {
            self.next = 0;
        }
        let handle = self.next;
        self.next = self.next.wrapping_add(1);
        self.entries.insert(handle, entry);
        handle
    }

    /// Look up the entry for a guest-supplied handle. Returns `None`
    /// for `SENDER_NONE` and for handles that were never allocated.
    pub fn resolve(&self, handle: u32) -> Option<SenderEntry> {
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
    fn allocate_session_and_component_handles_roundtrip() {
        let mut t = SenderTable::new();
        let h_sess = t.allocate(SenderEntry::Session(token(1)));
        let h_comp = t.allocate(SenderEntry::Component(MailboxId(42)));
        assert_ne!(h_sess, h_comp);
        assert_eq!(t.resolve(h_sess), Some(SenderEntry::Session(token(1))));
        assert_eq!(
            t.resolve(h_comp),
            Some(SenderEntry::Component(MailboxId(42))),
        );
    }

    #[test]
    fn resolve_sentinel_is_none() {
        let t = SenderTable::new();
        assert!(t.resolve(SENDER_NONE).is_none());
    }

    #[test]
    fn resolve_unknown_handle_is_none() {
        let mut t = SenderTable::new();
        let _ = t.allocate(SenderEntry::Session(token(7)));
        assert!(t.resolve(9999).is_none());
    }

    #[test]
    fn allocate_skips_sentinel_on_wrap() {
        let mut t = SenderTable::new();
        t.next = SENDER_NONE;
        let h = t.allocate(SenderEntry::Session(token(3)));
        // First handle after the wrap is 0 — the sentinel is never
        // handed out.
        assert_eq!(h, 0);
        assert_ne!(h, SENDER_NONE);
    }
}
