//! The real tier's keep-up characterisation: the counters harvested from the
//! harness actors' plain fields at run end, bracketed by the paced drive
//! loop's elapsed-versus-budget timing.

use std::time::Duration;

use aether_data::Kind;
use serde::{Deserialize, Serialize};

use super::{CountQuery, CountReport, Drive};
use crate::SubstrateHarness;

/// The real tier's keep-up characterisation (iamacoffeepot/aether#1233): a
/// sustained-paced run answers "does it keep up at 60 Hz", not the per-hop
/// span tree (whose volume laps the trace ring at real-tier fan-out). The
/// counters are harvested from the harness actors' plain fields at run end;
/// the timings bracket the paced drive loop. `Some` only for [`Tier::Real`]
/// cells (the only paced tier); `None` otherwise.
///
/// [`Tier::Real`]: crate::perf::harness::Tier::Real
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct KeepUp {
    /// Total `Ping` mails dispatched across the topology (`Σ sent`) — the
    /// offered load.
    pub offered: u64,
    /// Total `Ping` mails handled across the topology (`Σ received`) — the
    /// work completed. Equals `offered` when the pool drained everything the
    /// pace offered (the drain-integrity check); a shortfall means mail was
    /// left in flight.
    pub completed: u64,
    /// Wall-clock nanoseconds the paced drive loop took.
    pub elapsed_nanos: u64,
    /// Wall-clock nanoseconds the loop *should* have taken at the pace
    /// (`frames / pace_hz`). `elapsed / expected > 1` means the run fell
    /// behind the 60 Hz budget — the keep-up signal.
    pub expected_nanos: u64,
}

/// Harvest the real tier's keep-up counters (iamacoffeepot/aether#1233).
/// Mails a [`CountQuery`] to every participating actor (by the same names the
/// trace harvest used), sums `offered = Σ sent` and `completed = Σ received`,
/// and brackets them with the paced elapsed-vs-expected timing. Returns `None`
/// (logged) if any actor's reply fails to arrive or decode, so a botched
/// harvest yields no keep-up cell rather than a wrong one — mirroring the
/// trace harvest's fail-closed posture.
pub(super) fn harvest_keepup(
    tb: &mut SubstrateHarness,
    names: &[String],
    topo_name: &str,
    drive: Drive,
    frames: u32,
    drive_elapsed: Duration,
) -> Option<KeepUp> {
    let mut offered = 0u64;
    let mut completed = 0u64;
    for name in names {
        let req = CountQuery::default().encode_into_bytes();
        let reply = match tb.send_bytes_and_await(name, CountQuery::ID, req) {
            Ok(reply) => reply,
            Err(e) => {
                tracing::warn!(target: "aether_perf", topo = %topo_name, %name, error = ?e, "count_query send failed");
                return None;
            }
        };
        let Some(report) = CountReport::decode_from_bytes(&reply) else {
            tracing::warn!(target: "aether_perf", topo = %topo_name, %name, "count_report decode failed");
            return None;
        };
        offered = offered.saturating_add(report.sent);
        completed = completed.saturating_add(report.received);
    }
    // Only a paced run has a budget to measure against; an unpaced cell never
    // reaches here (the real tier is always paced), so the `_ => 0` arm is a
    // belt-and-braces guard.
    let expected_nanos = match drive {
        Drive::Latency { pace_hz: Some(hz) } if hz > 0 => u64::from(frames).saturating_mul(1_000_000_000 / hz),
        _ => 0,
    };
    Some(KeepUp {
        offered,
        completed,
        elapsed_nanos: u64::try_from(drive_elapsed.as_nanos()).unwrap_or(u64::MAX),
        expected_nanos,
    })
}
