//! Host-only mail for retaining one prepared component in its native slot.
//! The adapter maps an attempt to journal facts; these kinds contain no
//! Bloomery types and expose no guest lifecycle hook.

use aether_data::{KindId, MailboxId};
use aether_kinds::{ComponentCapabilities, ReplaceComponent};
use serde::{Deserialize, Serialize};

/// Unique within an adapter incarnation. A new incarnation uses a greater
/// epoch after reconciling the durable journal.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize, aether_data::Schema)]
pub struct SlotAttempt {
    pub epoch: u64,
    pub serial: u64,
}

#[aether_data::kind(name = "aether.component.slot.prepare")]
pub struct PrepareSlot {
    pub attempt: SlotAttempt,
    pub replacement: ReplaceComponent,
    pub warmup_kind: KindId,
    #[serde(with = "aether_data::bytes")]
    pub warmup_bytes: Vec<u8>,
    pub ack_recipient: MailboxId,
    pub ack_kind: KindId,
}

#[aether_data::kind(name = "aether.component.slot.prepared")]
pub enum SlotPrepared {
    Ok {
        attempt: SlotAttempt,
        #[serde(with = "aether_data::bytes")]
        ack_bytes: Vec<u8>,
    },
    Err {
        attempt: SlotAttempt,
        error: String,
    },
}

#[aether_data::kind(name = "aether.component.slot.evaluate_resident")]
pub struct EvaluateResident {
    pub attempt: SlotAttempt,
    pub event_kind: KindId,
    #[serde(with = "aether_data::bytes")]
    pub event_bytes: Vec<u8>,
}

/// Only confirms direct dispatch. The adapter must separately await the
/// guest's prepared and evaluated result mails.
#[aether_data::kind(name = "aether.component.slot.resident_delivered")]
pub enum ResidentDelivered {
    Ok { attempt: SlotAttempt },
    Err { attempt: SlotAttempt, error: String },
}

#[aether_data::kind(name = "aether.component.slot.commit")]
pub struct CommitSlot {
    pub attempt: SlotAttempt,
}

#[aether_data::kind(name = "aether.component.slot.committed")]
pub enum SlotCommitted {
    Ok { attempt: SlotAttempt, capabilities: ComponentCapabilities },
    Err { attempt: SlotAttempt, error: String },
}

#[aether_data::kind(name = "aether.component.slot.cancel")]
pub struct CancelSlot {
    pub attempt: SlotAttempt,
}

#[aether_data::kind(name = "aether.component.slot.cancelled")]
pub enum SlotCancelled {
    Ok { attempt: SlotAttempt },
    Err { attempt: SlotAttempt, error: String },
}
