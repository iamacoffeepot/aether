// Per-component-instance mapping from guest-visible reply handles
// (opaque `u32`) to the substrate-internal reply destination. Handles
// are allocated when a component receives mail that carries a reply
// target and resolved when the guest calls `reply_mail` to answer.
//
// ADR-0013 covered session-bound replies; ADR-0017 widened the table
// so component-originated mail also produces a handle — a component
// can now reply to a runtime-discovered peer without knowing its
// name at init, using the same `ctx.reply` API regardless of who
// called. ADR-0037 widened it again with a remote-engine variant.
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

use aether_data::SessionToken;

use crate::mail::{MailboxId, ReplyTarget};

/// Sentinel passed to the guest's `receive` shim when the inbound
/// mail has no reply target (broadcast origin — ADR-0013 §1). A
/// `reply_mail` call with this handle fails with the "unknown
/// handle" status.
pub const NO_REPLY_HANDLE: u32 = u32::MAX;

/// What a reply handle resolves to on the substrate side. The guest
/// sees only the opaque `u32` — the `target` variant lets `reply_mail`
/// pick the right outbound route, and `correlation_id` carries the
/// ADR-0042 correlation from the inbound mail so the reply's echo
/// happens automatically when `reply_mail` constructs the outbound
/// `ReplyTo`.
///
/// Invariant: `target` is never `ReplyTarget::None` — the table only
/// allocates entries for mail that had a meaningful reply target.
/// The shared enum stays convenient (same shape as envelope-level
/// `ReplyTo.target`), at the cost of a dead `None` variant here.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub struct ReplyEntry {
    pub target: ReplyTarget,
    pub correlation_id: u64,
}

impl ReplyEntry {
    /// Short constructor: `target` + `correlation_id`.
    pub fn new(target: ReplyTarget, correlation_id: u64) -> Self {
        Self {
            target,
            correlation_id,
        }
    }

    /// Back-compat shim for call sites that used the pre-correlation
    /// `ReplyEntry::Session(token)` form. Builds an entry with no
    /// correlation.
    pub fn session(token: SessionToken) -> Self {
        Self::new(ReplyTarget::Session(token), 0)
    }

    /// Back-compat shim for `ReplyEntry::Component(mailbox)`.
    pub fn component(mailbox: MailboxId) -> Self {
        Self::new(ReplyTarget::Component(mailbox), 0)
    }
}

/// Maintains the handle→entry map for one component instance.
#[derive(Debug, Default)]
pub struct ReplyTable {
    entries: HashMap<u32, ReplyEntry>,
    next: u32,
}

impl ReplyTable {
    pub fn new() -> Self {
        Self::default()
    }

    /// Allocate a fresh handle bound to `entry`. The returned handle
    /// is never `NO_REPLY_HANDLE` — wraps past it silently.
    pub fn allocate(&mut self, entry: ReplyEntry) -> u32 {
        // Skip the sentinel. In practice `next` never hits `u32::MAX`
        // before the instance is replaced/dropped; this is hygiene.
        if self.next == NO_REPLY_HANDLE {
            self.next = 0;
        }
        let handle = self.next;
        self.next = self.next.wrapping_add(1);
        self.entries.insert(handle, entry);
        handle
    }

    /// Look up the entry for a guest-supplied handle. Returns `None`
    /// for `NO_REPLY_HANDLE` and for handles that were never
    /// allocated.
    pub fn resolve(&self, handle: u32) -> Option<ReplyEntry> {
        if handle == NO_REPLY_HANDLE {
            return None;
        }
        self.entries.get(&handle).copied()
    }
}

#[cfg(test)]
mod tests {
    use aether_data::Uuid;

    use super::*;

    fn token(byte: u8) -> SessionToken {
        SessionToken(Uuid::from_bytes([byte; 16]))
    }

    #[test]
    fn allocate_session_and_component_handles_roundtrip() {
        let mut t = ReplyTable::new();
        let h_sess = t.allocate(ReplyEntry::session(token(1)));
        let h_comp = t.allocate(ReplyEntry::component(MailboxId(42)));
        assert_ne!(h_sess, h_comp);
        assert_eq!(t.resolve(h_sess), Some(ReplyEntry::session(token(1))));
        assert_eq!(
            t.resolve(h_comp),
            Some(ReplyEntry::component(MailboxId(42)))
        );
    }

    #[test]
    fn resolve_sentinel_is_none() {
        let t = ReplyTable::new();
        assert!(t.resolve(NO_REPLY_HANDLE).is_none());
    }

    #[test]
    fn resolve_unknown_handle_is_none() {
        let mut t = ReplyTable::new();
        let _ = t.allocate(ReplyEntry::session(token(7)));
        assert!(t.resolve(9999).is_none());
    }

    #[test]
    fn allocate_skips_sentinel_on_wrap() {
        let mut t = ReplyTable::new();
        t.next = NO_REPLY_HANDLE;
        let h = t.allocate(ReplyEntry::session(token(3)));
        // First handle after the wrap is 0 — the sentinel is never
        // handed out.
        assert_eq!(h, 0);
        assert_ne!(h, NO_REPLY_HANDLE);
    }

    #[test]
    fn allocate_preserves_correlation_id() {
        let mut t = ReplyTable::new();
        let entry = ReplyEntry::new(ReplyTarget::Session(token(5)), 0xCAFEBABE);
        let h = t.allocate(entry);
        let got = t.resolve(h).expect("resolves");
        assert_eq!(got.correlation_id, 0xCAFEBABE);
    }
}
