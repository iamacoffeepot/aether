//! The trace rings' export boundary (ADR-0230 §1). A ring holds mailbox
//! positions in memory; everything that leaves it names actors by path.
//!
//! - The render turns a position into the canonical path its route record
//!   holds, in every lifecycle, `Dropped` included, so a retired actor
//!   stays nameable: a closed actor's route keeps its name, and a name is
//!   never reused. The chassis sentinel renders as `aether.chassis`, and a
//!   position with no route record renders `None`.
//! - The receipt proof turns a requested [`TraceMailId`] back into the
//!   ring's [`MailId`] once, where a `TraceTail` arrives (R-0004), so the
//!   ring filters by one id comparison per entry.
//!
//! Both read the registry's route table; neither adds a table of its own.

use aether_data::{ErasedActorPath, MailId, MailboxId};
use aether_kinds::trace::{TraceEvent, TraceMailId, TraceRingEntry, TraceTail};

use crate::mail::registry::{CHASSIS_SENTINEL_NAME, Registry};

use super::ring::{RingEntry, TraceRecord};

/// The chassis sentinel's path, `aether.chassis`: the sender every
/// chassis-originated root names, and the ring the walk reads for it. The
/// in-process harness routes its walk's seed by comparing against it.
///
/// # Panics
///
/// Never: the sentinel name is a one-segment actor path.
#[must_use]
pub fn chassis_host_path() -> ErasedActorPath {
    ErasedActorPath::new(CHASSIS_SENTINEL_NAME).expect("the chassis sentinel name is a one-segment actor path")
}

/// A `TraceTail` request whose root filter has been proven into the ring's
/// own [`MailId`]. Built only by [`Self::prove`].
pub struct TailQuery {
    pub(crate) max: u32,
    pub(crate) since: Option<u64>,
    pub(crate) root: Option<MailId>,
}

impl TailQuery {
    /// Prove `request`'s root once, on receipt. A root whose sender path
    /// names no route this engine ever registered is refused with text
    /// naming it; no root is no filter.
    pub(crate) fn prove(request: &TraceTail, registry: &Registry) -> Result<Self, String> {
        let root = request
            .root
            .as_ref()
            .map(|root| prove_mail_id(registry, root).ok_or_else(|| unproven_root(root)))
            .transpose()?;
        Ok(Self { max: request.max, since: request.since, root })
    }
}

fn unproven_root(root: &TraceMailId) -> String {
    let sender = root.sender.as_ref().map_or("(no route)", ErasedActorPath::as_str);
    format!("aether.trace.tail: root {sender} #{} names no actor this engine registered", root.correlation_id)
}

/// The canonical path a ring position names: `aether.chassis` for the
/// chassis sentinel, the route record's name in any lifecycle otherwise,
/// and `None` for a position with no route record.
pub fn render_position(registry: &Registry, position: MailboxId) -> Option<ErasedActorPath> {
    if position == MailboxId::CHASSIS_MAILBOX_ID {
        Some(chassis_host_path())
    } else {
        registry.route_path(position)
    }
}

/// A ring [`MailId`] rendered for export: its sender position as a path.
pub fn render_mail_id(registry: &Registry, id: MailId) -> TraceMailId {
    TraceMailId { sender: render_position(registry, id.sender), correlation_id: id.correlation_id }
}

/// The ring [`MailId`] an exported identity names: the route standing under
/// exactly its sender path, in any lifecycle, or the chassis sentinel for
/// `aether.chassis`. `None` when the sender is absent or names no route.
pub fn prove_mail_id(registry: &Registry, id: &TraceMailId) -> Option<MailId> {
    let sender = id.sender.as_ref()?;
    let position = if sender.as_str() == CHASSIS_SENTINEL_NAME {
        MailboxId::CHASSIS_MAILBOX_ID
    } else {
        registry.route_position(sender)?
    };
    Some(MailId::new(position, id.correlation_id))
}

/// One ring slot rendered into its wire shape.
pub fn render_entry(registry: &Registry, entry: &RingEntry) -> TraceRingEntry {
    let mail_id = |id| render_mail_id(registry, id);
    let event = match entry.record {
        TraceRecord::Sent(sent) => TraceEvent::Sent {
            mail_id: mail_id(sent.mail_id),
            root: mail_id(sent.root),
            parent_mail: sent.parent_mail.map(mail_id),
            sender: render_position(registry, sent.sender),
            recipient: render_position(registry, sent.recipient),
            kind: sent.kind,
            t_construct_start: sent.t_construct_start,
            t: sent.t,
        },
        TraceRecord::Received { mail_id: id, t, t_enqueue, enqueue_depth, thread_id } => {
            TraceEvent::Received { mail_id: mail_id(id), t, t_enqueue, enqueue_depth, thread_id }
        }
        TraceRecord::Finished { mail_id: id, t } => TraceEvent::Finished { mail_id: mail_id(id), t },
    };
    TraceRingEntry { sequence: entry.sequence, root: mail_id(entry.root), event }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mail::registry::noop_handler;
    use crate::testing::boot_authority;

    /// A registry holding one route under `name`, retired to `Dropped`.
    fn registry_with_retired(name: &str) -> (Registry, MailboxId) {
        let registry = Registry::new();
        let id =
            registry.try_register_inbox(&boot_authority(), name, noop_handler()).expect("register the fixture inbox");
        registry.drop_mailbox(&boot_authority(), id).expect("the live route retires");
        (registry, id)
    }

    fn path(text: &str) -> ErasedActorPath {
        ErasedActorPath::new(text).expect("fixture is an actor path")
    }

    /// A retired actor keeps its name (#6861), so its records must render
    /// its path. Catches a render that reads only `Live` routes, which
    /// would blank every retired actor out of a trace export.
    #[test]
    fn a_dropped_route_renders_its_path() {
        let (registry, id) = registry_with_retired("aether.test.retired");

        assert_eq!(render_position(&registry, id), Some(path("aether.test.retired")));
    }

    /// The roots a retired minter minted stay queryable. Catches a proof
    /// that reuses `live_route`, which refuses a `Dropped` route and so
    /// would make every root it minted unfilterable.
    #[test]
    fn a_retired_minters_root_proves_its_position() {
        let (registry, id) = registry_with_retired("aether.test.minter");

        let root = TraceMailId { sender: Some(path("aether.test.minter")), correlation_id: 9 };
        assert_eq!(prove_mail_id(&registry, &root), Some(MailId::new(id, 9)));
    }

    /// The chassis sentinel has no route record, yet every chassis root
    /// names it. Catches a render that asks only the route table, which
    /// would blank every injected root's sender.
    #[test]
    fn the_chassis_sentinel_renders_and_proves_as_aether_chassis() {
        let registry = Registry::new();
        let root = render_mail_id(&registry, MailId::new(MailboxId::CHASSIS_MAILBOX_ID, 4));

        assert_eq!(root.sender, Some(path("aether.chassis")));
        assert_eq!(prove_mail_id(&registry, &root), Some(MailId::new(MailboxId::CHASSIS_MAILBOX_ID, 4)));
    }

    /// A position no route record names renders `None`, never a guessed
    /// name, and a root naming no route is refused rather than matching
    /// nothing silently.
    #[test]
    fn an_unregistered_position_renders_none_and_its_root_is_refused() {
        let registry = Registry::new();
        assert_eq!(render_position(&registry, MailboxId(0x5151)), None);

        let request = TraceTail {
            max: 0,
            since: None,
            root: Some(TraceMailId { sender: Some(path("aether.test.never")), correlation_id: 1 }),
        };
        let refusal = TailQuery::prove(&request, &registry).err().expect("an unregistered root is refused");
        assert!(refusal.contains("aether.test.never"), "the refusal names the root: {refusal}");
    }
}
