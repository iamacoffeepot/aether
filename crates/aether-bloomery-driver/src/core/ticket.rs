//! Typed tickets: one newtype per command kind, minted only by the core.
//!
//! Each ticket is a private-field `u64` and its own mail kind under
//! `aether.bloomery.driver.ticket.*`, so the shell can store it as the
//! request context and take it back from the reply. Because the ticket is
//! typed per command, a reply cannot be routed to the wrong continuation:
//! each reply method takes only its own ticket type, and a ticket the core
//! is not waiting on yields no commands.

macro_rules! tickets {
    ($( $name:ident => $kind:literal, )*) => {
        $(
            #[aether_data::kind(name = $kind, copy, eq, no_serde)]
            pub struct $name(u64);

            impl $name {
                pub(crate) fn mint(id: u64) -> Self {
                    Self(id)
                }
            }

            impl PartialOrd for $name {
                fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
                    Some(self.cmp(other))
                }
            }

            impl Ord for $name {
                fn cmp(&self, other: &Self) -> std::cmp::Ordering {
                    self.0.cmp(&other.0)
                }
            }
        )*
    };
}

tickets! {
    EventsTicket => "aether.bloomery.driver.ticket.events",
    ArtifactTicket => "aether.bloomery.driver.ticket.artifact",
    ClosureTicket => "aether.bloomery.driver.ticket.closure",
    AppendTicket => "aether.bloomery.driver.ticket.append",
    LoadTicket => "aether.bloomery.driver.ticket.load",
    InvokeTicket => "aether.bloomery.driver.ticket.invoke",
}

/// Caller handle minted by [`ProgramCore::call`](crate::ProgramCore::call).
///
/// The shell stores its deferred reply under this id before it performs the
/// returned commands. It is not a mail kind: it never crosses the shell boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct CallerId(u64);

impl CallerId {
    pub(crate) fn mint(id: u64) -> Self {
        Self(id)
    }
}
