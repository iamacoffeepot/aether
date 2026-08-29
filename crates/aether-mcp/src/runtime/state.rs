//! The capability's runtime state, and the pending table every deferred
//! operation lives in.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use aether_data::{KindId, MailboxId, SchemaType};
use aether_substrate::actor::monitor::MonitorHandle;
use aether_substrate::actor::native::NativeCtx;
use aether_substrate::chassis::inbox::InboundMail;
use aether_substrate::mail::mailer::Mailer;
use aether_substrate::mail::registry::MailboxEntry;

use crate::McpServerConfiguration;
use crate::protocol::MessageId;
use crate::protocol::json::ParseLimits;
use crate::schema::SchemaBudget;

use super::admission::{DeadlineTimer, InFlightPool, RateLimiter, elapsed_millis};
use super::registry::{RegistryLimits, ResourceRegistry, ToolRegistry};
use super::response_resources::{ResponseStore, ResponseStoreLimits};

/// What a pending entry is waiting for, and what its reply has to become.
pub enum PendingOperation {
    /// A `tools/call` awaiting `ToolInvocationResult`.
    Tool {
        name: String,
        /// The registered `{ output }` wrapper schema the provider's bytes are
        /// decoded against. Carried here rather than looked up again, so a
        /// registration that changed mid-flight cannot decode an in-flight
        /// result against a contract its caller never saw.
        output_wrapper_schema: SchemaType,
    },
    /// A `resources/read` awaiting `ReadResourceResult`.
    Resource { uri: String },
}

/// One deferred operation: the held HTTP obligation plus what it takes to
/// answer it.
///
/// The obligation is an [`InboundMail`] guard rather than a stored `Source`,
/// because holding the guard is what keeps the original HTTP request's causal
/// chain open across the round trip — the HTTP server would otherwise settle it
/// early and answer `502` while the provider was still working.
pub struct PendingCall {
    pub inbound: InboundMail,
    /// The client's identifier, copied back verbatim into the response.
    pub id: MessageId,
    pub operation: PendingOperation,
    /// Which occupant of this correlation the armed deadline belongs to.
    pub generation: u64,
}

/// The `aether.mcp.server` runtime state.
pub struct McpServerState {
    pub config: McpServerConfiguration,
    pub tools: ToolRegistry,
    pub resources: ResourceRegistry,
    pub responses: ResponseStore,
    pub rate: RateLimiter,
    pub in_flight: InFlightPool,
    /// Deferred operations, keyed by the dispatch's correlation id.
    ///
    /// Keyed by the *Aether* correlation, never by the client's JSON-RPC
    /// identifier: identifier uniqueness is a client responsibility and two
    /// independent concurrent POSTs may legally carry the same one, so a table
    /// keyed that way would cross their answers.
    pub pending: BTreeMap<u64, PendingCall>,
    /// Monotonic base for every deadline and lifetime this capability measures.
    pub epoch: Instant,
    /// `None` only for a disabled server, which arms nothing.
    pub timer: Option<DeadlineTimer>,
    pub mailer: Arc<Mailer>,
    pub self_mailbox: MailboxId,
    /// Live monitors on registration holders, so a departure purges its claims.
    pub monitors: HashMap<MailboxId, MonitorHandle>,
    /// Holders the substrate cannot monitor, remembered so one failure is
    /// reported once rather than on every claim they make.
    pub unmonitorable: HashSet<MailboxId>,
    next_generation: u64,
}

impl McpServerState {
    #[must_use]
    pub fn new(config: McpServerConfiguration, mailer: Arc<Mailer>, self_mailbox: MailboxId, epoch: Instant) -> Self {
        let limits = RegistryLimits {
            maximum_registered_tools: config.maximum_registered_tools,
            maximum_discoverable_resources: config.maximum_discoverable_resources,
            maximum_schema_bytes: config.maximum_schema_bytes,
            maximum_http_response_bytes: config.maximum_http_response_bytes,
            schema_budget: SchemaBudget::default(),
        };
        let store_limits = ResponseStoreLimits {
            maximum_bytes: config.response_resource_maximum_bytes,
            total_bytes: config.response_resource_total_bytes,
            maximum_entries: config.response_resource_maximum_entries,
            lifetime_secs: config.response_resource_lifetime_secs,
        };
        let rate = RateLimiter::new(config.requests_per_minute, config.request_burst, elapsed_millis(epoch));
        let in_flight = InFlightPool::new(config.maximum_in_flight_requests);

        Self {
            tools: ToolRegistry::new(limits),
            resources: ResourceRegistry::new(limits),
            responses: ResponseStore::new(store_limits),
            rate,
            in_flight,
            pending: BTreeMap::new(),
            epoch,
            timer: None,
            mailer,
            self_mailbox,
            monitors: HashMap::new(),
            unmonitorable: HashSet::new(),
            next_generation: 0,
            config,
        }
    }

    /// Milliseconds since this capability's monotonic epoch.
    #[must_use]
    pub fn now_millis(&self) -> u64 {
        elapsed_millis(self.epoch)
    }

    /// The next pending-slot generation.
    ///
    /// Monotonic across the process, so a correlation the substrate reuses
    /// cannot land on a deadline armed for its previous occupant.
    pub fn next_generation(&mut self) -> u64 {
        self.next_generation = self.next_generation.wrapping_add(1);
        self.next_generation
    }

    /// The request parser's bounds, resolved from configuration.
    #[must_use]
    pub fn parse_limits(&self) -> ParseLimits {
        ParseLimits {
            maximum_depth: self.config.maximum_request_nesting_depth,
            maximum_values: self.config.maximum_request_values,
        }
    }

    /// Whether a mailbox can still receive.
    ///
    /// Consulted at dispatch as well as at registration, because a monitor
    /// notice may not have been delivered yet when a call arrives; the two
    /// together are what stop a call landing on a departed holder.
    #[must_use]
    pub fn is_live(&self, mailbox: MailboxId) -> bool {
        matches!(self.mailer.registry().entry(mailbox), Some(MailboxEntry::Inbox { .. } | MailboxEntry::Inline(_)))
    }

    /// Whether a registrant's declared accept-set covers a kind.
    #[must_use]
    pub fn accepts(&self, mailbox: MailboxId, kind: KindId) -> bool {
        self.mailer.capability_registry().accepts(mailbox, kind)
    }

    /// Monitor a registration holder on its first claim, so its departure
    /// purges what it claimed.
    ///
    /// A monitor failure is logged rather than discarded: an unmonitored holder
    /// that vanishes leaves a descriptor whose calls fail forever with no notice
    /// to explain it, and that symptom is indistinguishable from a lost notice
    /// unless the failed attempt left a trace.
    pub fn watch<M: aether_actor::ReplyMode>(&mut self, ctx: &NativeCtx<'_, M>, mailbox: MailboxId) {
        if self.unmonitorable.contains(&mailbox) || self.monitors.contains_key(&mailbox) {
            return;
        }
        match ctx.monitor(mailbox) {
            Ok(handle) => {
                self.monitors.insert(mailbox, handle);
            }
            Err(error) => {
                self.unmonitorable.insert(mailbox);
                tracing::warn!(
                    target: "aether_mcp::server",
                    %mailbox,
                    ?error,
                    "registration holder is not monitorable; its claims cannot be purged when it departs",
                );
            }
        }
    }
}
