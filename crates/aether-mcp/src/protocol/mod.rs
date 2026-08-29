//! The Model Context Protocol message model for revision 2025-06-18.
//!
//! This module parses and renders; it never dispatches. The actor that serves
//! these shapes owns admission, registries, providers, and deadlines, and
//! reaches for the functions here at each edge of that work. Keeping the two
//! apart is what lets the whole message surface be exercised without booting
//! a chassis.
//!
//! The implemented profile is deliberately narrower than the revision: one
//! JSON response per request, an empty `202` for every notification, and
//! never an event stream. There is no standard-input transport, no session
//! identifier, no server-to-client request, and no server-originated
//! notification.

pub mod json;
pub mod lifecycle;
pub mod remote_procedure_call;
pub mod resources;
pub mod tools;

#[cfg(test)]
mod tests;

pub use remote_procedure_call::{
    Incoming, MessageId, Notification, PROTOCOL_REVISION, ProtocolError, Request, Response, parse_incoming,
};

/// A request method this server serves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RequestMethod {
    Initialize,
    Ping,
    ListTools,
    CallTool,
    ListResources,
    ReadResource,
}

impl RequestMethod {
    /// The wire name of this method.
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Initialize => "initialize",
            Self::Ping => "ping",
            Self::ListTools => "tools/list",
            Self::CallTool => "tools/call",
            Self::ListResources => "resources/list",
            Self::ReadResource => "resources/read",
        }
    }
}

/// What the server does with a method name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MethodSupport {
    /// Served.
    Served(RequestMethod),
    /// Defined by the revision, considered, and deliberately not served.
    ///
    /// Carrying the reason turns the exclusion into something a reviewer can
    /// check: the set is written out rather than falling out of a `_` arm, so
    /// adding a method to the served set is a visible edit and dropping one
    /// cannot happen silently.
    Excluded { reason: &'static str },
    /// Not a method this revision defines.
    Unknown,
}

impl MethodSupport {
    /// Whether a *request* naming this method must be answered
    /// `-32601 Method not found`.
    ///
    /// Both refusals answer the same way. The distinction exists for
    /// diagnostics and review, not for the client.
    #[must_use]
    pub fn is_method_not_found(self) -> bool {
        !matches!(self, Self::Served(_))
    }
}

/// Classify a method name.
///
/// A *notification* never consults this table for dispatch: the transport
/// requires an empty `202` for every syntactically valid notification, known
/// or not, and that includes a request-only method such as `tools/call`
/// arriving without an identifier. The table decides what a **request** may
/// name.
#[must_use]
pub fn classify(method: &str) -> MethodSupport {
    match method {
        "initialize" => MethodSupport::Served(RequestMethod::Initialize),
        "ping" => MethodSupport::Served(RequestMethod::Ping),
        "tools/list" => MethodSupport::Served(RequestMethod::ListTools),
        "tools/call" => MethodSupport::Served(RequestMethod::CallTool),
        "resources/list" => MethodSupport::Served(RequestMethod::ListResources),
        "resources/read" => MethodSupport::Served(RequestMethod::ReadResource),

        "resources/templates/list" => MethodSupport::Excluded {
            reason: "tools return concrete resource links, including chunk addresses, so no template discovery",
        },
        "resources/subscribe" | "resources/unsubscribe" => MethodSupport::Excluded {
            reason: "a subscription needs a server-to-client stream the stateless profile cannot open",
        },
        "prompts/list" | "prompts/get" => MethodSupport::Excluded {
            reason: "there is no prompt catalog at this boundary; instructions and tool descriptions cover it",
        },
        "completion/complete" => MethodSupport::Excluded {
            reason: "completion depends on prompt or resource-template arguments this surface does not expose",
        },
        "roots/list" | "sampling/createMessage" | "elicitation/create" => MethodSupport::Excluded {
            reason: "a client capability, reached by a server-to-client request the stateless profile never makes",
        },
        "logging/setLevel" => MethodSupport::Excluded {
            reason: "no logging capability is advertised and no protocol log notification is emitted",
        },

        _ => MethodSupport::Unknown,
    }
}
