//! The guest's live held-reply tickets (ADR-0243 §6).
//!
//! [`HeldTickets`] lives in the per-component [`Registry`](super::Registry),
//! the one the ctx, the request-context table and the dehydrate ctx already
//! reach, so no ticket state sits in a global. It records every ticket this
//! instance holds and where the ticket is: live as a value in guest memory,
//! parked in a stored request context, or saved in the state a dehydrate
//! wrote. It also maps each request whose stored context parked a ticket to
//! that context's kind name, for the untaken-reply check.
//!
//! Three [`HeldLedger`] views grant the codec hooks, one per place a ticket
//! may be encoded or decoded:
//!
//! - [`ContextLedger`] parks tickets as a request context is stored;
//! - [`DehydrateLedger`] parks tickets as `on_dehydrate` saves state;
//! - [`ClaimLedger`] claims them back as a context take or a rehydrate decode
//!   rebuilds the `Held` values.
//!
//! The detached ticket, [`NO_REPLY_HANDLE`], answers nothing: every view
//! accepts it and records nothing.

use alloc::boxed::Box;
use alloc::collections::BTreeMap;

use aether_data::wire::{Error, HeldClaim, HeldLedger};
use aether_data::{KindId, RequestId};

use crate::mail::NO_REPLY_HANDLE;

/// Where one ticket's obligation currently sits.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Place {
    /// A `Held` value in guest memory owns it.
    Live,
    /// A stored request context's bytes own it.
    InContext,
    /// The state a dehydrate saved owns it. A running instance whose
    /// replace was refused or rolled back gets that state back through its
    /// `on_rehydrate`, which claims the ticket back to live as the `Held`
    /// decodes (issue 7125).
    Saved,
}

#[derive(Debug)]
struct Ticket {
    place: Place,
    reply: KindId,
}

/// Every held ticket this instance owns, plus the requests whose stored
/// context parked one.
#[derive(Debug, Default)]
pub struct HeldTickets {
    tickets: BTreeMap<u32, Ticket>,
    untaken: BTreeMap<RequestId, &'static str>,
}

impl HeldTickets {
    pub const fn new() -> Self {
        Self { tickets: BTreeMap::new(), untaken: BTreeMap::new() }
    }

    /// Record a freshly armed ticket as live.
    ///
    /// # Panics
    ///
    /// When the ticket is already held: the host never reuses a held reply
    /// handle, so a repeat means the two sides disagree (ADR-0063).
    pub fn arm(&mut self, ticket: u32, reply: KindId) {
        if ticket == NO_REPLY_HANDLE {
            return;
        }
        let previous = self.tickets.insert(ticket, Ticket { place: Place::Live, reply });
        assert!(previous.is_none(), "aether-actor: reply handle {ticket} armed while already held");
    }

    /// Forget an answered ticket.
    pub fn release(&mut self, ticket: u32) {
        self.tickets.remove(&ticket);
    }

    /// Whether a live ticket remains, one a dehydrate did not save.
    pub fn any_live(&self) -> bool {
        self.tickets.values().any(|ticket| ticket.place == Place::Live)
    }

    /// Return every saved ticket to live: the instance that saved it keeps
    /// running, and a ticket its restored state did not claim back must not
    /// pass a later dehydrate unsaved.
    pub fn revert_saved(&mut self) {
        for ticket in self.tickets.values_mut().filter(|ticket| ticket.place == Place::Saved) {
            ticket.place = Place::Live;
        }
    }

    /// The kind name of the context stored under `request`, when that
    /// context parked a ticket and has not been taken.
    pub fn untaken(&self, request: RequestId) -> Option<&'static str> {
        self.untaken.get(&request).copied()
    }

    /// Whether any stored context still holds a parked ticket.
    pub fn any_untaken(&self) -> bool {
        !self.untaken.is_empty()
    }

    /// Record that the context stored under `request` was taken.
    pub fn taken(&mut self, request: RequestId) {
        self.untaken.remove(&request);
    }

    /// Move `ticket` from one of `from` to `to`, when it answers `reply`.
    fn park(&mut self, ticket: u64, reply: KindId, from: &[Place], to: Place) -> Result<(), Error> {
        let unclaimed = Error::HeldUnclaimed { ticket, reply };
        let Ok(handle) = u32::try_from(ticket) else {
            return Err(unclaimed);
        };
        if handle == NO_REPLY_HANDLE {
            return Ok(());
        }

        match self.tickets.get_mut(&handle) {
            Some(held) if held.reply == reply && from.contains(&held.place) => {
                held.place = to;
                Ok(())
            }
            _ => Err(unclaimed),
        }
    }
}

/// Parks tickets as a request context is stored under `request`, and records
/// the context's kind name for the untaken-reply check.
pub struct ContextLedger<'a> {
    tickets: &'a mut HeldTickets,
    request: RequestId,
    context: &'static str,
}

impl<'a> ContextLedger<'a> {
    pub fn new(tickets: &'a mut HeldTickets, request: RequestId, context: &'static str) -> Self {
        Self { tickets, request, context }
    }
}

impl HeldLedger for ContextLedger<'_> {
    fn park(&mut self, ticket: u64, reply: KindId) -> Result<(), Error> {
        self.tickets.park(ticket, reply, &[Place::Live], Place::InContext)?;
        if ticket != u64::from(NO_REPLY_HANDLE) {
            self.tickets.untaken.insert(self.request, self.context);
        }
        Ok(())
    }

    fn claim(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error> {
        Err(Error::HeldUnclaimed { ticket, reply })
    }
}

/// Parks tickets as `on_dehydrate` saves state. A ticket an earlier
/// dehydrate saved, in a replace that was then rolled back, saves again.
pub struct DehydrateLedger<'a> {
    tickets: &'a mut HeldTickets,
}

impl<'a> DehydrateLedger<'a> {
    pub fn new(tickets: &'a mut HeldTickets) -> Self {
        Self { tickets }
    }
}

impl HeldLedger for DehydrateLedger<'_> {
    fn park(&mut self, ticket: u64, reply: KindId) -> Result<(), Error> {
        self.tickets.park(ticket, reply, &[Place::Live, Place::Saved], Place::Saved)
    }

    fn claim(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error> {
        Err(Error::HeldUnclaimed { ticket, reply })
    }
}

/// Claims tickets back into live `Held` values: from a stored context in
/// this instance, or from a context or saved state a predecessor wrote. The
/// host slot, carried across the replace, is what keeps a restored ticket
/// answerable; this view refuses only a ticket already live here.
pub struct ClaimLedger<'a> {
    tickets: &'a mut HeldTickets,
}

impl<'a> ClaimLedger<'a> {
    pub fn new(tickets: &'a mut HeldTickets) -> Self {
        Self { tickets }
    }
}

impl HeldLedger for ClaimLedger<'_> {
    fn park(&mut self, ticket: u64, reply: KindId) -> Result<(), Error> {
        Err(Error::HeldUnclaimed { ticket, reply })
    }

    fn claim(&mut self, ticket: u64, reply: KindId) -> Result<HeldClaim, Error> {
        let unclaimed = Error::HeldUnclaimed { ticket, reply };
        let Ok(handle) = u32::try_from(ticket) else {
            return Err(unclaimed);
        };
        if handle != NO_REPLY_HANDLE {
            match self.tickets.tickets.get_mut(&handle) {
                Some(held) if held.place == Place::Live || held.reply != reply => return Err(unclaimed),
                Some(held) => held.place = Place::Live,
                None => {
                    self.tickets.tickets.insert(handle, Ticket { place: Place::Live, reply });
                }
            }
        }
        Ok(HeldClaim(Box::new(())))
    }
}
