use super::{
    EngineId, Kind, MailEnvelope, MailboxAddress, MailboxId, ScopePathError, mailbox_id_from_path, validate_scope_path,
};

/// ADR-0098/0099 input hygiene: reject a `recipient_name` whose
/// `/`-rendered scope path exceeds the depth or byte caps before it
/// folds to a `MailboxId`. The MCP `send_mail` surface is the wire
/// boundary for user-controlled names, so the aggregate-key guard lands
/// here; [`mailbox_id_from_path`] stays infallible for static callers.
pub(super) fn validate_recipient_scope(recipient_name: &str) -> anyhow::Result<()> {
    let segments: Vec<&str> = recipient_name.split('/').collect();
    validate_scope_path(&segments).map_err(|e| match e {
        ScopePathError::TooDeep { limit } => {
            anyhow::anyhow!("recipient_name has more than {limit} scope segments")
        }
        ScopePathError::TooLong { limit } => {
            anyhow::anyhow!("recipient_name exceeds the {limit}-byte scope-path cap")
        }
    })
}

/// Resolve an operator-supplied recipient name into its wire mailbox id — the
/// runtime-name forwarding path the MCP front owns: names arrive as strings on
/// the tool call (`recipient_name`, a lineage address, a component path), so
/// there is no typed actor to resolve through. The one sanctioned
/// `mailbox_id_from_path` call site in this crate; every tool funnels here.
#[allow(clippy::disallowed_methods)] // the runtime-name wire-forwarding escape hatch — the tool surface receives names as strings
pub(super) fn recipient_mailbox(name: &str) -> MailboxId {
    mailbox_id_from_path(name)
}

/// Build a `MailEnvelope` addressed at a hub-local mailbox
/// (`engine = None`) carrying a typed kind.
pub(super) fn local_envelope<K: Kind>(mailbox: &str, kind: &K) -> MailEnvelope {
    MailEnvelope {
        to: MailboxAddress::local(recipient_mailbox(mailbox)),
        from: None,
        kind: K::ID,
        correlation_id: None,
        payload: kind.encode_into_bytes(),
    }
}

/// Build a `MailEnvelope` addressed at a mailbox on a specific
/// substrate (`engine = Some`) carrying a typed kind — the hub routes
/// it through to that engine's proxy.
pub(super) fn engine_envelope<K: Kind>(engine: EngineId, mailbox: &str, kind: &K) -> MailEnvelope {
    engine_envelope_by_id(engine, recipient_mailbox(mailbox), kind)
}

/// Like [`engine_envelope`] but addresses the recipient by
/// [`MailboxId`] directly. The trace-tree guided walk (ADR-0086 Phase
/// 3b) discovers recipients as ids embedded in `Sent` events, never as
/// names — a `MailboxId` is a one-way name hash, so there's no name to
/// reconstruct.
pub(super) fn engine_envelope_by_id<K: Kind>(engine: EngineId, mailbox: MailboxId, kind: &K) -> MailEnvelope {
    MailEnvelope {
        to: MailboxAddress { engine: Some(engine), mailbox },
        from: None,
        kind: K::ID,
        correlation_id: None,
        payload: kind.encode_into_bytes(),
    }
}
