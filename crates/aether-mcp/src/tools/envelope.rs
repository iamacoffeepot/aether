use super::{
    EngineId, Kind, MailEnvelope, MailboxAddress, MailboxId, ScopePathError, mailbox_id_from_name, validate_scope_path,
};

/// ADR-0098/0099 input hygiene: reject a `recipient_name` whose
/// `/`-rendered scope path exceeds the depth or byte caps before it reaches
/// the selected engine's registry.
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

/// Build a `MailEnvelope` addressed at a hub-local mailbox
/// (`engine = None`) carrying a typed kind. Callers pass only trusted
/// actor-owned singleton capability constants, never operator input.
pub(super) fn local_envelope<K: Kind>(mailbox: &str, kind: &K) -> MailEnvelope {
    MailEnvelope {
        #[allow(clippy::disallowed_methods)] // trusted actor-owned singleton capability constant
        to: MailboxAddress::local(mailbox_id_from_name(mailbox)),
        from: None,
        kind: K::ID,
        correlation_id: None,
        payload: kind.encode_into_bytes(),
    }
}

/// Build a `MailEnvelope` addressed at a mailbox on a specific
/// substrate (`engine = Some`) carrying a typed kind — the hub routes
/// it through to that engine's proxy. Callers pass only trusted actor-owned
/// singleton capability constants; operator addresses use
/// [`engine_envelope_by_id`] after engine resolution.
pub(super) fn engine_envelope<K: Kind>(engine: EngineId, mailbox: &str, kind: &K) -> MailEnvelope {
    #[allow(clippy::disallowed_methods)] // trusted actor-owned singleton capability constant
    engine_envelope_by_id(engine, mailbox_id_from_name(mailbox), kind)
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
