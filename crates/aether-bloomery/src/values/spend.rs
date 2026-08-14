//! The sealed spend ceiling (ADR-0192): what the fleet may spend, attested
//! like the rates it is graded at.
//!
//! A day's spend is one number across every vendor the sealed
//! [`PriceTable`](super::PriceTable) prices. Nothing in the pipeline used to
//! read that number. This value is the owner statement that does: two axes,
//! sealed bloom-wide through the ADR-0174 registry, compared at the seal door
//! against a window the host measured from the same priced column the ledger
//! already holds.
//!
//! An absent axis is uncapped, and an absent entry is uncapped on both — the
//! same posture [`PriceTable::default`](super::PriceTable::default) takes when
//! it prices nothing. A compiled-in dollar figure would be this repository
//! stating what the owner's fleet may spend.

use alloc::collections::BTreeMap;
use alloc::string::String;

use serde::{Deserialize, Serialize};

use crate::ids::BloomId;

/// The fleet's spend ceiling, sealed bloom-wide under
/// `aether.bloomery.spend_ceiling`.
///
/// Resolution is bloom-wide only, the departure ADR-0174 already took for
/// [`ApprovalPolicy`](super::ApprovalPolicy): a per-member entry would let one
/// member choose the ceiling that admits its own bloom. Micro-USD because a
/// float is not `Eq` and this value is sealed, the same reason
/// [`StudyCost::cost_micro_usd`](super::StudyCost::cost_micro_usd) is.
#[derive(aether_data::Kind, aether_data::Schema, Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
#[kind(name = "aether.bloomery.spend_ceiling")]
pub struct SpendCeiling {
    /// Cap on the whole window's summed spend. `None` is uncapped.
    pub window_micro_usd: Option<u64>,
    /// Cap on any one bloom's summed spend inside the window. `None` is
    /// uncapped.
    pub bloom_micro_usd: Option<u64>,
}

/// Why the seal door closed: which axis crossed, by how much, in which window.
///
/// The axis is the variant rather than a flag beside an optional bloom, so a
/// window crossing cannot be misread as a bloom crossing with a missing id.
/// The window label rides in the journaled value so a reader can tell which
/// day's ceiling closed the door without joining back to host configuration.
#[derive(aether_data::Schema, Clone, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum SpendQuiesce {
    /// The window's total is at or over [`SpendCeiling::window_micro_usd`].
    Window {
        /// The host-named window the measurement was taken in.
        window: String,
        /// The window's summed spend at the crossing.
        spent_micro_usd: u64,
        /// The sealed window ceiling that closed the door.
        ceiling_micro_usd: u64,
    },
    /// One bloom's total is at or over [`SpendCeiling::bloom_micro_usd`].
    Bloom {
        /// The host-named window the measurement was taken in.
        window: String,
        /// The first bloom in the window at or over the per-bloom ceiling.
        bloom: BloomId,
        /// That bloom's summed spend at the crossing.
        spent_micro_usd: u64,
        /// The sealed per-bloom ceiling that closed the door.
        ceiling_micro_usd: u64,
    },
}

/// The host-measured spend the reducer compares to a sealed [`SpendCeiling`].
///
/// Pure data: the reducer has no clock and no store, so the host names the
/// window and fills the totals. A caller with nothing to measure passes
/// [`Default`] — an empty window that never quiesces against a present
/// positive ceiling.
#[derive(Clone, PartialEq, Eq, Debug, Default, Serialize, Deserialize)]
pub struct SpendWindow {
    /// The host-named window this measurement covers — ADR-0186's day branch.
    pub label: String,
    /// Summed `cost_micro_usd` over every resolved study record in the window.
    pub total_micro_usd: u64,
    /// Per-bloom totals inside the window, keyed by bloom id so the first
    /// crossing is a stable scan rather than an insertion-order accident.
    pub per_bloom: BTreeMap<BloomId, u64>,
    /// Study-record evidence the resolver could not fold — missing bytes, or a
    /// record that does not grade its evidence's subject or name its own bloom.
    /// Counted apart from the total so an accounting gap is a number, not a
    /// suspicion.
    pub unaccounted_dispatches: u64,
    /// Resolved records whose priced column is zero — a model the sealed table
    /// priced at nothing, distinguishable from a cheap fleet whose totals are
    /// small but nonzero.
    pub unpriced_records: u64,
}

impl SpendCeiling {
    /// Compare this ceiling to a measured window. `None` is the door staying
    /// open: every present axis is under, or the axis is absent.
    ///
    /// The window axis is evaluated first. A fleet-wide crossing is the more
    /// complete statement, and naming one bloom beside it would send an
    /// operator to the wrong raise.
    #[must_use]
    pub fn quiesce(&self, spend: &SpendWindow) -> Option<SpendQuiesce> {
        if let Some(ceiling_micro_usd) = self.window_micro_usd
            && spend.total_micro_usd >= ceiling_micro_usd
        {
            return Some(SpendQuiesce::Window {
                window: spend.label.clone(),
                spent_micro_usd: spend.total_micro_usd,
                ceiling_micro_usd,
            });
        }
        if let Some(ceiling_micro_usd) = self.bloom_micro_usd
            && let Some((bloom, spent_micro_usd)) =
                spend.per_bloom.iter().find(|(_, spent)| **spent >= ceiling_micro_usd)
        {
            return Some(SpendQuiesce::Bloom {
                window: spend.label.clone(),
                bloom: *bloom,
                spent_micro_usd: *spent_micro_usd,
                ceiling_micro_usd,
            });
        }
        None
    }
}
