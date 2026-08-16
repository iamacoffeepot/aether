//! Edge-affinity slot choice (ADR-0196).
//!
//! When member B depends on member A, B's construct prefers the lane slot that
//! built A: that slot's target directory has already compiled the spliced base.
//! Preference, never a requirement — a busy or quarantined slot falls back to
//! the lowest free index the allocator already uses.
//!
//! The lookup key is the checkout hex B will build on. After A's construct
//! captures, that hex *is* A's candidate commit — the unique-maximum ancestor's
//! tree the splice path materializes as B's base. No graph walk lives here.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

/// Why a dispatch landed in the slot it did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotChoice {
    /// No predecessor slot to prefer — lowest free, as before the graph.
    Lowest,
    /// The predecessor's slot was free and this dispatch took it.
    Preferred,
    /// The predecessor's slot was held or quarantined; fell back to lowest free.
    Busy,
}

impl SlotChoice {
    /// The evidence token the work order names.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Lowest => "lowest",
            Self::Preferred => "preferred",
            Self::Busy => "busy",
        }
    }
}

/// The slot decision remembered on a run so evidence can stamp it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlotAffinity {
    /// The predecessor slot this dispatch wanted, when one was known.
    pub preferred: Option<usize>,
    /// The slot actually claimed.
    pub assigned: usize,
    /// Why [`assigned`](Self::assigned) is the slot it is.
    pub reason: SlotChoice,
}

impl SlotAffinity {
    /// A re-adopted run reclaims the slot it recorded; that is not a new choice.
    #[must_use]
    pub fn readopted(assigned: Option<usize>) -> Self {
        Self { preferred: None, assigned: assigned.unwrap_or(0), reason: SlotChoice::Lowest }
    }
}

/// Pick a slot index given a preferred predecessor and the indices that cannot
/// be handed out.
///
/// The search is still total: there is always a larger index that is neither
/// held nor quarantined. Preferred is consulted first and only first — a busy
/// preference never waits, it falls back.
#[must_use]
pub fn choose_slot(
    preferred: Option<usize>,
    held: &HashSet<usize>,
    quarantined: &HashSet<usize>,
) -> (usize, SlotChoice) {
    if let Some(preferred) = preferred
        && !held.contains(&preferred)
        && !quarantined.contains(&preferred)
    {
        return (preferred, SlotChoice::Preferred);
    }
    let slot = lowest_free(held, quarantined);
    let reason = if preferred.is_some() {
        SlotChoice::Busy
    } else {
        SlotChoice::Lowest
    };
    (slot, reason)
}

fn lowest_free(held: &HashSet<usize>, quarantined: &HashSet<usize>) -> usize {
    let mut slot = 0;
    while held.contains(&slot) || quarantined.contains(&slot) {
        slot += 1;
    }
    slot
}

/// Durable checkout-hex → builder-slot map, sitting beside the slot checkouts
/// so a coordinator restart still prefers the slot that compiled A.
pub struct BuilderSlots {
    path: PathBuf,
    slots: HashMap<String, usize>,
}

impl BuilderSlots {
    /// Load whatever the previous process left under `base_dir`, or start empty.
    #[must_use]
    pub fn load(base_dir: &Path) -> Self {
        let path = base_dir.join("edge-slots");
        let slots = fs::read_to_string(&path).ok().as_deref().map(parse_edge_slots).unwrap_or_default();
        Self { path, slots }
    }

    /// The slot that captured `checkout_hex`, when one has.
    #[must_use]
    pub fn preferred(&self, checkout_hex: &str) -> Option<usize> {
        self.slots.get(checkout_hex).copied()
    }

    /// Remember that `slot` produced the candidate at `checkout_hex`.
    pub fn record(&mut self, checkout_hex: String, slot: usize) {
        self.slots.insert(checkout_hex, slot);
        persist_edge_slots(&self.path, &self.slots);
    }
}

fn parse_edge_slots(text: &str) -> HashMap<String, usize> {
    let mut slots = HashMap::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let Some(hex) = parts.next() else {
            continue;
        };
        let Some(slot) = parts.next().and_then(|slot| slot.parse().ok()) else {
            continue;
        };
        if !hex.is_empty() {
            slots.insert(hex.to_owned(), slot);
        }
    }
    slots
}

fn persist_edge_slots(path: &Path, slots: &HashMap<String, usize>) {
    let mut text = String::new();
    let mut entries: Vec<_> = slots.iter().collect();
    entries.sort_by(|a, b| a.0.cmp(b.0));
    for (hex, slot) in entries {
        text.push_str(hex);
        text.push(' ');
        text.push_str(&slot.to_string());
        text.push('\n');
    }
    if let Err(error) = fs::write(path, text) {
        tracing::warn!(
            path = %path.display(),
            %error,
            "local executor backend: could not persist edge-affinity slot map",
        );
    }
}

/// Stamp the slot choice onto the evidence envelope so affinity is auditable
/// from the same bytes the journal already keeps.
#[must_use]
pub fn stamp_slot_affinity(bytes: &[u8], affinity: &SlotAffinity) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    let Some(object) = value.as_object_mut() else {
        return bytes.to_vec();
    };
    object.insert(
        "slot_affinity".to_owned(),
        serde_json::json!({
            "preferred": affinity.preferred,
            "assigned": affinity.assigned,
            "reason": affinity.reason.as_str(),
        }),
    );
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| bytes.to_vec())
}

#[cfg(test)]
mod tests {
    use super::{SlotChoice, choose_slot, parse_edge_slots, stamp_slot_affinity};
    use std::collections::HashSet;

    fn held(slots: &[usize]) -> HashSet<usize> {
        slots.iter().copied().collect()
    }

    #[test]
    fn a_free_preferred_slot_wins_over_the_lowest_free() {
        // Tripwire: without affinity the allocator always hands out the lowest
        // free index, so B would miss the warm target dir in A's slot whenever
        // a lower index happened to be idle. The plausible bug is treating
        // preference as a tie-break instead of the first choice.
        let (slot, reason) = choose_slot(Some(1), &held(&[0]), &HashSet::new());
        assert_eq!((slot, reason), (1, SlotChoice::Preferred));

        let (slot, reason) = choose_slot(Some(2), &HashSet::new(), &HashSet::new());
        assert_eq!((slot, reason), (2, SlotChoice::Preferred), "a free preferred slot is taken even when 0 is free");
    }

    #[test]
    fn a_busy_preferred_slot_falls_back_to_lowest_free() {
        // Preference is never a wait. A busy (or quarantined) predecessor slot
        // must not park B behind A-the-next-occupant; it takes any free slot.
        let (slot, reason) = choose_slot(Some(0), &held(&[0]), &HashSet::new());
        assert_eq!((slot, reason), (1, SlotChoice::Busy));

        let (slot, reason) = choose_slot(Some(1), &held(&[0]), &held(&[1]));
        assert_eq!((slot, reason), (2, SlotChoice::Busy), "quarantine is as blocking as a live hold");
    }

    #[test]
    fn no_predecessor_still_takes_the_lowest_free() {
        let (slot, reason) = choose_slot(None, &held(&[0, 2]), &HashSet::new());
        assert_eq!((slot, reason), (1, SlotChoice::Lowest));
    }

    #[test]
    fn the_edge_slot_file_is_last_write_wins_per_checkout() {
        // A refine of A produces a new capture; B must prefer the slot that
        // built the capture it will actually check out, not the first one A
        // ever ran in.
        let slots = parse_edge_slots("deadbeef 0\ndeadbeef 1\ncafe 2\n");
        assert_eq!(slots.get("deadbeef").copied(), Some(1));
        assert_eq!(slots.get("cafe").copied(), Some(2));
    }

    #[test]
    fn stamp_slot_affinity_sits_beside_the_result_record() {
        let stamped = stamp_slot_affinity(
            br#"{"command":"construct.implement","result_record":{"num_turns":3}}"#,
            &super::SlotAffinity { preferred: Some(1), assigned: 1, reason: SlotChoice::Preferred },
        );
        let value: serde_json::Value = serde_json::from_slice(&stamped).expect("stamp emits JSON");
        assert_eq!(value["slot_affinity"]["preferred"], 1);
        assert_eq!(value["slot_affinity"]["assigned"], 1);
        assert_eq!(value["slot_affinity"]["reason"], "preferred");
        assert_eq!(value["result_record"]["num_turns"], 3);
    }
}
