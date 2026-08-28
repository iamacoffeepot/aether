//! Wire kinds owned by the Model Context Protocol server capability
//! (ADR-0121). The substrate core dispatches none of them, so they live here
//! rather than in `aether-kinds`. The module is always-on and wasm-safe — the
//! types name only `aether_data` and serde, so a provider that takes this
//! crate `default-features = false` to declare tools keeps compiling.
//!
//! The family mirrors `aether-http`'s route vocabulary, because a tool *is* a
//! route: [`RegisterToolSelf`] is the reflexive, host-stamped claim that
//! `RegisterRouteSelf` is for a route, with the same exclusive-versus-shared
//! semantics and the same "the cap resolves the registrant from the inbound
//! envelope's `Source`, so it cannot be forged" trust story.

use serde::{Deserialize, Serialize};
use std::fmt;

/// Actor namespace of the Model Context Protocol server capability.
///
/// Dot-separated ownership under the `aether.mcp` family; it introduces no
/// dash-sibling ambiguity. The registration and dispatch kinds below are
/// named from it.
pub const SERVER_NAMESPACE: &str = "aether.mcp.server";

/// The advertised tool safety hints (`readOnlyHint`, `destructiveHint`,
/// `idempotentHint`, `openWorldHint`).
///
/// All four are always emitted, so a client never has to guess a default.
/// The protocol defines them as advisory: authorization and domain validation
/// never consult them. Their defaults are the protocol's own —
/// `read_only: false`, `destructive: true`, `idempotent: false`,
/// `open_world: true` — the conservative reading of an undeclared tool.
// The four booleans are the protocol's own four annotation fields, named and
// numbered by the specification. Folding them into enums or a state machine
// would invent a vocabulary the wire does not have.
#[allow(clippy::struct_excessive_bools)]
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy, PartialEq, Eq)]
pub struct ToolAnnotations {
    /// The tool performs no state change.
    pub read_only: bool,
    /// The tool may perform a destructive update. Meaningful only when
    /// `read_only` is false; a read-only tool always reports `false`.
    pub destructive: bool,
    /// Repeating the call with the same arguments has no additional effect.
    pub idempotent: bool,
    /// The tool interacts with an open-ended external world.
    pub open_world: bool,
}

impl Default for ToolAnnotations {
    fn default() -> Self {
        Self { read_only: false, destructive: true, idempotent: false, open_world: true }
    }
}

/// `aether.mcp.server.register_tool_self` — claim a tool name for the
/// *sending* actor, with no explicit `mailbox` field.
///
/// The capability resolves the registrant from the inbound envelope's
/// host-stamped `Source` (ADR-0083), so the registrant cannot be forged and
/// the operation is gated to in-process actors by construction. Sent from
/// `wire`; the authoring macro appends one send per declared tool.
///
/// `shared` opts the registration into the name's member *set* exactly as
/// `RegisterRouteSelf` does for a route key: several actors may hold one
/// descriptor when every member opted in and all descriptor bytes and
/// metadata match, and dispatch is round-robin across live members. `false`
/// is the exclusive claim — a held name from a different mailbox is an `Err`.
///
/// The three schema carriers are `aether_data::wire::to_vec` of the
/// associated `Schema::SCHEMA`. They are bytes rather than fields because
/// this checkout has no `Schema` implementation for `SchemaType` itself, so a
/// registration kind cannot carry a `SchemaType` and still derive its own
/// schema. `aether_data::wire` is the existing serialization seam, not a
/// second schema vocabulary.
///
/// The capability recomputes `kind_id_from_parts(request_kind_name,
/// request_wrapper_schema)` and rejects a mismatch against `request_kind`, so
/// a descriptor cannot point at a kind it does not describe.
///
/// Reply: [`RegisterToolResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.server.register_tool_self")]
pub struct RegisterToolSelf {
    /// The public tool name, matching `^[a-z][a-z0-9_]{0,63}$`.
    pub name: String,
    /// Optional human-readable title.
    pub title: Option<String>,
    /// Tool description, 1 through 4,096 UTF-8 bytes.
    pub description: String,
    /// The four advertised safety hints.
    pub annotations: ToolAnnotations,
    /// Wire name of the hidden per-tool request kind the capability
    /// dispatches, `"{NAMESPACE}.tool.{name}"`.
    pub request_kind_name: String,
    /// Identifier of that hidden request kind, recomputed and checked by the
    /// capability against `request_kind_name` plus the request wrapper schema.
    pub request_kind: aether_data::KindId,
    /// Canonical bytes of the generated one-field `{ input }` request
    /// wrapper's `SchemaType`.
    #[serde(with = "aether_data::bytes")]
    pub request_wrapper_schema_bytes: Vec<u8>,
    /// Canonical bytes of the generated one-field `{ output }` value
    /// wrapper's `SchemaType`.
    #[serde(with = "aether_data::bytes")]
    pub output_wrapper_schema_bytes: Vec<u8>,
    /// Canonical bytes of the generated `{ inline, addressed }` boundary
    /// output `SchemaType` — the shape advertised as the tool's
    /// `outputSchema`.
    #[serde(with = "aether_data::bytes")]
    pub output_schema_bytes: Vec<u8>,
    /// Join the name's member set rather than claim it exclusively.
    pub shared: bool,
}

/// Reply to [`RegisterToolSelf`].
///
/// Failure modes: a name that is not the accepted grammar, a held name
/// claimed by a different mailbox, a shared join whose descriptor bytes or
/// metadata differ from the incumbent's, a request identifier that does not
/// recompute, a request kind the registrant's capability registry does not
/// accept, a schema carrier past the configured byte budget, a schema tree
/// past the translator's depth or node budget, an inadmissible schema shape,
/// a new name arriving after the catalog froze, or the descriptor count or
/// rendered listing crossing its ceiling.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.server.register_tool_result")]
pub enum RegisterToolResult {
    Ok,
    Err { error: String },
}

/// The common reply from every generated tool handler.
///
/// `Ok` carries `aether_data::wire::to_vec` bytes of the tool's hidden
/// `{ output }` value wrapper — not the boundary envelope, which the
/// capability builds after it decides inline versus addressed. `Err` is a
/// domain refusal: it becomes a successful JSON-RPC response carrying
/// `isError: true`, never a protocol error, because the invocation resolved
/// against a registered descriptor before it ran.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.server.tool_invocation_result")]
pub enum ToolInvocationResult {
    Ok {
        #[serde(with = "aether_data::bytes")]
        output_bytes: Vec<u8>,
    },
    Err {
        category: String,
        message: String,
    },
}

/// Hidden request context carried across a provider's deferred tool call.
///
/// A tool method that defers stores this under the downstream mail's
/// correlation; the generated reply handler probes for it *without*
/// consuming, so an HTTP `DeferredSource` on the same handler stays available
/// to `answer_deferred`. `tool_request_kind` is the discriminator that
/// selects one mapping when several tools share a reply kind — the reply kind
/// alone cannot.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.mcp.server.deferred_tool_source")]
pub struct DeferredToolSource {
    /// The original requester to answer once the downstream reply maps.
    pub source: aether_data::Source,
    /// Which tool's hidden request kind opened this deferral.
    pub tool_request_kind: aether_data::KindId,
}

/// A resource a provider advertises through `resources/list`.
///
/// Translation emits the protocol spellings — `mimeType` for `mime_type` and
/// `size` for `size_bytes`. Fields carry no meaning beyond the 2025-06-18
/// resource surface; nothing here is host-derived.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ResourceDescriptor {
    pub uri: String,
    pub name: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub mime_type: Option<String>,
    pub size_bytes: Option<u64>,
}

/// `aether.mcp.server.register_resource_provider_self` — claim a URI prefix
/// for the *sending* actor, resolved from the host-stamped `Source` like
/// [`RegisterToolSelf`].
///
/// Prefix claims are exclusive and longest-prefix matching wins, so two
/// providers may nest without ambiguity. `aether://mcp/response/` is reserved
/// to the capability's own ephemeral response store and cannot be claimed.
/// `descriptors` are the discoverable entries `resources/list` returns;
/// concrete unlisted addresses under the prefix stay dynamic, which is how
/// content hashes and response nonces work.
///
/// No authoring macro exists for resources in the first release: a provider
/// registers this from `wire` and handles [`ReadResource`] directly.
///
/// Reply: [`RegisterResourceProviderResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.server.register_resource_provider_self")]
pub struct RegisterResourceProviderSelf {
    /// Normalized absolute `aether://` prefix, ending in `/`.
    pub prefix: String,
    /// Discoverable descriptors under that prefix; may be empty.
    pub descriptors: Vec<ResourceDescriptor>,
}

/// Reply to [`RegisterResourceProviderSelf`].
///
/// Failure modes: a prefix that is not the accepted URI grammar, the reserved
/// response-store prefix, a prefix already held by a different mailbox, a new
/// discoverable prefix arriving after the discoverable catalog froze, or the
/// descriptor count or rendered listing crossing its ceiling.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.server.register_resource_provider_result")]
pub enum RegisterResourceProviderResult {
    Ok,
    Err { error: String },
}

/// `aether.mcp.server.read_resource` — dispatch of one `resources/read` to
/// the provider holding the longest matching prefix.
///
/// `uri` is already parsed and normalized by the capability; a provider never
/// prefix-matches an unchecked raw string. Reply: [`ReadResourceResult`].
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.server.read_resource")]
pub struct ReadResource {
    pub uri: String,
}

/// Reply to [`ReadResource`].
///
/// A successful arm must echo the requested normalized `uri`; the capability
/// treats a mismatch as an internal error rather than trusting the provider's
/// spelling. `Blob` carries raw bytes — a provider never supplies
/// pre-encoded base64, and never a host path. Unlike a tool call, a
/// provider-backed read stays a protocol request: `Err` with category
/// `not_found` becomes `-32002`, other categories `-32603`.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone)]
#[kind(name = "aether.mcp.server.read_resource_result")]
pub enum ReadResourceResult {
    Text {
        uri: String,
        mime_type: String,
        text: String,
    },
    Blob {
        uri: String,
        mime_type: String,
        #[serde(with = "aether_data::bytes")]
        bytes: Vec<u8>,
    },
    Err {
        category: String,
        message: String,
    },
}

/// Hidden request context carried across a resource provider's own
/// downstream read.
///
/// A provider that answers [`ReadResource`] by mailing a storage capability
/// stores this under the downstream mail's correlation and recovers it on the
/// reply to answer the original requester.
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.mcp.server.deferred_resource_source")]
pub struct DeferredResourceSource {
    pub source: aether_data::Source,
}

/// The addressed half of a tool's boundary output envelope.
///
/// When a decoded output byte leaf or the serialized output as a whole
/// crosses its inline ceiling, the capability stores the exact UTF-8 JSON
/// serialization of the raw output and returns this in place of the inline
/// value. Addressing the *complete* output rather than substituting the
/// oversized leaf is what keeps the advertised `outputSchema` satisfied: a
/// `Bytes` property is declared as an array of integers, and replacing that
/// leaf with an object would change its type.
#[derive(aether_data::Schema, Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct AddressedOutput {
    /// `aether://mcp/response/<nonce>` — an unpredictable 128-bit nonce.
    pub uri: String,
    /// Byte count of the stored raw output.
    pub bytes: u64,
    /// Bounded shape summary; it describes structure and a sample, never
    /// expanded nested content.
    pub summary: String,
}

/// A tool's refusal to execute or to produce a valid declared output.
///
/// This is the Rust-side error a `#[mcp::tool]` method and a reply mapper
/// return. It lowers to [`ToolInvocationResult::Err`] on the wire and to a
/// *successful* JSON-RPC response carrying `isError: true` at the boundary.
///
/// Expected domain outcomes that callers branch on belong in the declared
/// `Output` instead — an idempotent request reporting `AlreadyApplied` is an
/// output variant, not a `ToolError`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolError {
    /// Short machine-readable category, e.g. `"invalid_state"`.
    pub category: String,
    /// Human-readable message. The boundary caps the rendered content block
    /// at 2,048 bytes; longer detail belongs in an addressed diagnostic
    /// resource when it is safe to expose.
    pub message: String,
}

impl ToolError {
    /// Build a tool error from a category and message.
    #[must_use]
    pub fn new(category: impl Into<String>, message: impl Into<String>) -> Self {
        Self { category: category.into(), message: message.into() }
    }
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.category, self.message)
    }
}

impl From<ToolError> for ToolInvocationResult {
    fn from(error: ToolError) -> Self {
        Self::Err { category: error.category, message: error.message }
    }
}

/// The capability's own deadline wake — not part of the public vocabulary.
///
/// One runtime-owned timer thread keeps a deadline heap for every deferred
/// operation and posts this back to the capability's mailbox when the earliest
/// one comes due; it never touches actor state itself. `generation` is what
/// stops a reused pending-table slot from accepting a stale timer event: a
/// correlation that completed and whose slot was refilled carries a newer
/// generation, so the old wake finds a mismatch and does nothing.
///
/// It is a `Kind` because the timer reaches the actor the only way anything
/// reaches an actor — by mail. It is `#[doc(hidden)]` because nothing outside
/// this capability may send it: an external sender could otherwise expire
/// another caller's in-flight tool call.
#[doc(hidden)]
#[derive(aether_data::Kind, aether_data::Schema, Serialize, Deserialize, Debug, Clone, Copy)]
#[kind(name = "aether.mcp.server.request_deadline_elapsed")]
pub struct RequestDeadlineElapsed {
    /// Correlation of the pending operation whose deadline came due.
    pub correlation_id: u64,
    /// Which occupant of that pending slot the deadline was armed for.
    pub generation: u64,
}
