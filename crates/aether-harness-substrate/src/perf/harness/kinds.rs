//! The sweep's own mail vocabulary: the payload the relay topology forwards,
//! and the out-of-band counter query the run-end keep-up harvest answers with.

/// Fire-and-forward payload the relay actors pass along. The `seq`
/// field is carried for legibility when eyeballing a trace; the relay
/// forwards the bytes verbatim, so the wire shape is irrelevant to the
/// measurement. The schema-hashed `ID` is what the relay matches and
/// the trace records.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "mlat.ping")]
pub struct Ping {
    pub seq: u32,
}

/// Run-end harvest query (iamacoffeepot/aether#1233): the harness mails one
/// of these to each participating actor after the drive loop to pull its
/// plain-field `Ping` counters out-of-band. The counters live in the actor's
/// own state (no shared atomics), so the only way to read them cross-thread is
/// a mail the actor answers — matching the existing `aether.trace.tail`
/// harvest flow. The body is meaningless (the kind id is the whole signal); a
/// single field keeps it a well-formed `Pod`.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "mlat.count_query")]
pub struct CountQuery {
    /// Unused; present only so the query carries a non-empty `Pod` body.
    pub nonce: u32,
}

/// The reply to a [`CountQuery`] (iamacoffeepot/aether#1233): one actor's
/// `Ping` throughput counters. The real tier's keep-up metric sums these
/// across the topology — `offered = Σ sent`, `completed = Σ received` — to
/// report completed-vs-offered without touching the (lapping) trace ring.
#[repr(C)]
#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    PartialEq,
    Eq,
    bytemuck::Pod,
    bytemuck::Zeroable,
    aether_data::Kind,
    aether_data::Schema,
)]
#[kind(name = "mlat.count_report")]
pub struct CountReport {
    /// `Ping` mails this actor dispatched downstream — the source's per-tick
    /// emissions, or a relay's per-inbound forwards.
    pub sent: u64,
    /// `Ping` mails this actor received and handled. Relays only; the source
    /// handles `Tick`, never `Ping`, so its `received` is always 0.
    pub received: u64,
}
