//! Basic request/reply utility kind vocabulary.

/// Request addressed to a component that supports the ADR-0013
/// reply-to-sender smoke path. The component answers with `Pong`
/// carrying the same `seq`; the round trip proves that an operator
/// session → component → session reply works end-to-end.
#[repr(C)]
#[aether_data::kind(name = "aether.ping", pod, default, eq)]
pub struct Ping {
    pub seq: u32,
}

/// Reply-to-sender counterpart to `Ping`. The `seq` is the incoming
/// `Ping.seq` echoed back so the caller can match requests against
/// replies when multiple are in flight.
#[repr(C)]
#[aether_data::kind(name = "aether.pong", pod, default, eq)]
pub struct Pong {
    pub seq: u32,
}
