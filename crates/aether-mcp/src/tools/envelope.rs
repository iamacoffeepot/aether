use super::{ActorPath, EngineId, Kind, MailEnvelope, Recipient};

/// Build a `MailEnvelope` addressed at a hub-local actor
/// (`engine = None`) carrying a typed kind. Callers pass only the trusted
/// capability constants (`FLEET_CAP` and its siblings), never operator
/// input; the constant becomes the recipient's `ActorPath`.
pub(super) fn local_envelope<K: Kind>(recipient: &'static str, kind: &K) -> MailEnvelope {
    MailEnvelope { to: Recipient::local(capability_path(recipient)), kind: K::ID, payload: kind.encode_into_bytes() }
}

/// Build a `MailEnvelope` addressed at an actor on a specific
/// substrate (`engine = Some`) carrying a typed kind — the hub routes
/// it through to that engine's proxy. Callers pass only the trusted
/// capability constants; an operator address is resolved to its
/// canonical `ActorPath` first and sent with [`engine_envelope_to`].
pub(super) fn engine_envelope<K: Kind>(engine: EngineId, recipient: &'static str, kind: &K) -> MailEnvelope {
    engine_envelope_to(engine, capability_path(recipient), kind)
}

/// Like [`engine_envelope`] but addresses the recipient by an
/// [`ActorPath`] the caller already holds: the canonical path the
/// selected engine answered for an operator address, or for an id a
/// trace tree reported. The engine resolves the path on arrival.
pub(super) fn engine_envelope_to<K: Kind>(engine: EngineId, recipient: ActorPath, kind: &K) -> MailEnvelope {
    MailEnvelope {
        to: Recipient { engine: Some(engine), path: recipient },
        kind: K::ID,
        payload: kind.encode_into_bytes(),
    }
}

/// The `ActorPath` of a trusted capability constant. Every constant is a
/// bare namespace, so a refusal is a programming error in the constant.
fn capability_path(recipient: &'static str) -> ActorPath {
    ActorPath::new(recipient).unwrap_or_else(|error| panic!("capability constant {recipient:?} is a path: {error}"))
}
