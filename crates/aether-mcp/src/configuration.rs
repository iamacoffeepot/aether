//! Resolved Model Context Protocol server configuration (ADR-0090,
//! ADR-0156). The `#[derive(Config)]` layer a chassis builds from argv and
//! env and hands to the server capability.
//!
//! The domain struct is always available so a marker-only build can name it;
//! the native derive rides `feature = "runtime"`, exactly as
//! `HttpServerConfig` does.

use std::collections::HashSet;
use std::fmt;

/// Default `authorization_token`: unset. An enabled server with an empty
/// token fails closed.
pub const DEFAULT_AUTHORIZATION_TOKEN: &str = "";
/// Default `maximum_in_flight_requests`: concurrent tool or resource
/// dispatches.
pub const DEFAULT_MAXIMUM_IN_FLIGHT_REQUESTS: usize = 128;
/// Default `requests_per_minute`: accepted messages per minute.
pub const DEFAULT_REQUESTS_PER_MINUTE: u32 = 600;
/// Default `request_burst`: token-bucket burst above the steady rate.
pub const DEFAULT_REQUEST_BURST: u32 = 64;
/// Default `tool_timeout_millis`: 600 s, below the hub's 610,000 ms HTTP
/// request deadline so the protocol layer expires a tool first.
pub const DEFAULT_TOOL_TIMEOUT_MILLIS: u64 = 600_000;
/// Default `reply_inline_maximum_bytes`: 16 `KiB`.
pub const DEFAULT_REPLY_INLINE_MAXIMUM_BYTES: usize = 16_384;
/// Default `response_inline_maximum_bytes`: 32 `KiB`.
pub const DEFAULT_RESPONSE_INLINE_MAXIMUM_BYTES: usize = 32_768;
/// Default `response_resource_maximum_bytes`: 1 `MiB` per stored response.
pub const DEFAULT_RESPONSE_RESOURCE_MAXIMUM_BYTES: usize = 1_048_576;
/// Default `response_resource_total_bytes`: 64 `MiB` across the store.
pub const DEFAULT_RESPONSE_RESOURCE_TOTAL_BYTES: usize = 67_108_864;
/// Default `response_resource_maximum_entries`: live stored responses.
pub const DEFAULT_RESPONSE_RESOURCE_MAXIMUM_ENTRIES: usize = 128;
/// Default `response_resource_lifetime_secs`: 600 s.
pub const DEFAULT_RESPONSE_RESOURCE_LIFETIME_SECS: u64 = 600;
/// Default `maximum_request_nesting_depth`: JSON nesting levels in a request.
pub const DEFAULT_MAXIMUM_REQUEST_NESTING_DEPTH: usize = 128;
/// Default `maximum_request_values`: JSON value nodes in a request.
pub const DEFAULT_MAXIMUM_REQUEST_VALUES: usize = 262_144;
/// Default `maximum_output_values`: decoded JSON value nodes in a provider
/// result.
pub const DEFAULT_MAXIMUM_OUTPUT_VALUES: usize = 262_144;
/// Default `provider_wire_maximum_bytes`: 1 `MiB` each way to a provider.
pub const DEFAULT_PROVIDER_WIRE_MAXIMUM_BYTES: usize = 1_048_576;
/// Default `maximum_http_response_bytes`: 2 `MiB` serialized response.
pub const DEFAULT_MAXIMUM_HTTP_RESPONSE_BYTES: usize = 2_097_152;
/// Default `maximum_registered_tools`: tool descriptors in the catalog.
pub const DEFAULT_MAXIMUM_REGISTERED_TOOLS: usize = 256;
/// Default `maximum_discoverable_resources`: listed resource descriptors.
pub const DEFAULT_MAXIMUM_DISCOVERABLE_RESOURCES: usize = 256;
/// Default `maximum_schema_bytes`: 256 `KiB` per serialized schema carrier.
pub const DEFAULT_MAXIMUM_SCHEMA_BYTES: usize = 262_144;

/// Init config for the Model Context Protocol server capability.
///
/// The listener is not ours: the endpoint is a `POST /mcp` route registered
/// with `HttpServerCapability`, so bind address, request body ceiling, and
/// connection limits are that capability's configuration. What lives here is
/// everything the protocol layer itself decides — who may call, how much it
/// will parse, how long a tool may run, and when a result becomes an address
/// instead of an inline value.
///
/// `#[derive(aether_substrate::Config)]` (ADR-0090) emits the env-shaped
/// `McpServerConfigurationLayer`, the clap-shaped
/// `McpServerConfigurationOverlay`, and the inherent `from_env` /
/// `from_argv_then_env` shims under `feature = "runtime"`; a marker-only
/// build carries only this domain struct.
///
/// `Debug` is hand-written so `authorization_token` renders redacted. The
/// checkout has no reusable secret-value type, and whether the token stays
/// mandatory at all is an open question for the owner; until that resolves,
/// the domain struct at least does not leak it into a log line or a
/// `--print-config` rendering that formats it.
#[derive(Clone)]
#[cfg_attr(feature = "runtime", derive(aether_substrate::Config))]
#[cfg_attr(feature = "runtime", config(env_prefix = "AETHER_MCP", cli_prefix = "mcp"))]
pub struct McpServerConfiguration {
    /// Enable the Model Context Protocol endpoint.
    ///
    /// Default `false` — like the HTTP server it rides on, the endpoint is
    /// opt-in, so an unconfigured chassis advertises no tools.
    #[cfg_attr(feature = "runtime", config(default = false))]
    pub enabled: bool,
    /// Static bearer token every request must present.
    ///
    /// A deployment access-control guard for a preconfigured client, not an
    /// OAuth flow, and never forwarded to an engine or a provider. An
    /// enabled server with an empty token fails closed and answers every
    /// request `401`; that is deliberate, so a misconfigured deployment is
    /// shut rather than open. A public multi-user deployment puts a
    /// conforming authorization server in front of the loopback endpoint.
    #[cfg_attr(feature = "runtime", config(default = ""))]
    pub authorization_token: String,
    /// Browser origins accepted on a request that carries `Origin`.
    ///
    /// An absent `Origin` is accepted — that is a native client, not a
    /// browser. A present value must match a member exactly; the default
    /// empty allowlist therefore rejects every present origin, which is the
    /// DNS-rebinding guard the transport asks for without conflating a
    /// browser origin with a native caller.
    ///
    /// A set rather than a list: membership is the only question asked of
    /// it, and it is the derive's one supported collection shape. Spelled as
    /// comma-separated values in the environment.
    #[cfg_attr(feature = "runtime", config(default = [], csv_set))]
    pub allowed_origins: HashSet<String>,
    /// Concurrent tool or resource dispatches admitted at once.
    ///
    /// Immediate lifecycle and list responses do not hold a permit; only
    /// asynchronous dispatch does.
    #[cfg_attr(feature = "runtime", config(default = 128))]
    pub maximum_in_flight_requests: usize,
    /// Steady-state accepted messages per minute, across all callers.
    #[cfg_attr(feature = "runtime", config(default = 600))]
    pub requests_per_minute: u32,
    /// Token-bucket burst allowed above `requests_per_minute`.
    #[cfg_attr(feature = "runtime", config(default = 64))]
    pub request_burst: u32,
    /// Deadline in milliseconds for one dispatched tool call.
    ///
    /// Must stay below the hosting `HttpServerConfig.request_timeout_millis`
    /// so the protocol layer, which can answer `isError: true` with a
    /// diagnosis, expires a slow tool before the HTTP layer expires the whole
    /// request with a bare `504`.
    #[cfg_attr(feature = "runtime", config(default = 600_000))]
    pub tool_timeout_millis: u64,
    /// Byte ceiling above which a decoded output `Bytes` leaf makes the
    /// whole output addressed.
    ///
    /// Retains the environment spelling `AETHER_MCP_REPLY_INLINE_MAX_BYTES`
    /// from the outgoing coordinator so an existing deployment's tuning keeps
    /// working; only the env key is inherited, the flag follows the derive's
    /// ordinary `--mcp-` naming.
    #[cfg_attr(feature = "runtime", config(env = "AETHER_MCP_REPLY_INLINE_MAX_BYTES", default = 16_384))]
    pub reply_inline_maximum_bytes: usize,
    /// Byte ceiling above which a serialized output becomes addressed
    /// regardless of its leaf types.
    ///
    /// Retains the environment spelling
    /// `AETHER_MCP_RESPONSE_INLINE_MAX_BYTES`, like the field above.
    #[cfg_attr(feature = "runtime", config(env = "AETHER_MCP_RESPONSE_INLINE_MAX_BYTES", default = 32_768))]
    pub response_inline_maximum_bytes: usize,
    /// Largest single response the ephemeral store will hold.
    #[cfg_attr(feature = "runtime", config(default = 1_048_576))]
    pub response_resource_maximum_bytes: usize,
    /// Total bytes the ephemeral response store will hold.
    ///
    /// The store does not evict an unexpired resource to make room; it
    /// rejects the new spill, so an address it already handed out stays
    /// valid for its advertised lifetime.
    #[cfg_attr(feature = "runtime", config(default = 67_108_864))]
    pub response_resource_total_bytes: usize,
    /// Live entries the ephemeral response store will hold.
    #[cfg_attr(feature = "runtime", config(default = 128))]
    pub response_resource_maximum_entries: usize,
    /// Seconds an ephemeral response address stays readable.
    #[cfg_attr(feature = "runtime", config(default = 600))]
    pub response_resource_lifetime_secs: u64,
    /// JSON nesting levels accepted in a request body.
    ///
    /// Crossing it is `-32600` with a null identifier: the parser does not
    /// trust an identifier recovered from a document it deliberately stopped
    /// constructing.
    #[cfg_attr(feature = "runtime", config(default = 128))]
    pub maximum_request_nesting_depth: usize,
    /// JSON value nodes accepted in a request body, within the HTTP body
    /// ceiling. Rejected before constructing the excess node.
    #[cfg_attr(feature = "runtime", config(default = 262_144))]
    pub maximum_request_values: usize,
    /// JSON value nodes accepted when decoding a provider's output bytes.
    ///
    /// Threaded into `decode_schema_strict` as its explicit ceiling, so a
    /// compact collection cannot inflate an input-proportional allowance.
    #[cfg_attr(feature = "runtime", config(default = 262_144))]
    pub maximum_output_values: usize,
    /// Byte ceiling on the encoded invocation sent to a provider and on the
    /// provider result accepted before decode.
    #[cfg_attr(feature = "runtime", config(default = 1_048_576))]
    pub provider_wire_maximum_bytes: usize,
    /// Byte ceiling on the serialized HTTP response, checked after final
    /// serialization.
    #[cfg_attr(feature = "runtime", config(default = 2_097_152))]
    pub maximum_http_response_bytes: usize,
    /// Tool descriptors the registry will admit.
    ///
    /// This ceiling is what makes an unpaginated `tools/list` and an
    /// immutable cached response deliberate rather than accidental.
    #[cfg_attr(feature = "runtime", config(default = 256))]
    pub maximum_registered_tools: usize,
    /// Discoverable resource descriptors the registry will admit.
    #[cfg_attr(feature = "runtime", config(default = 256))]
    pub maximum_discoverable_resources: usize,
    /// Bytes accepted in any one serialized schema carrier on a
    /// registration.
    #[cfg_attr(feature = "runtime", config(default = 262_144))]
    pub maximum_schema_bytes: usize,
}

impl Default for McpServerConfiguration {
    fn default() -> Self {
        Self {
            enabled: false,
            authorization_token: DEFAULT_AUTHORIZATION_TOKEN.to_string(),
            allowed_origins: HashSet::new(),
            maximum_in_flight_requests: DEFAULT_MAXIMUM_IN_FLIGHT_REQUESTS,
            requests_per_minute: DEFAULT_REQUESTS_PER_MINUTE,
            request_burst: DEFAULT_REQUEST_BURST,
            tool_timeout_millis: DEFAULT_TOOL_TIMEOUT_MILLIS,
            reply_inline_maximum_bytes: DEFAULT_REPLY_INLINE_MAXIMUM_BYTES,
            response_inline_maximum_bytes: DEFAULT_RESPONSE_INLINE_MAXIMUM_BYTES,
            response_resource_maximum_bytes: DEFAULT_RESPONSE_RESOURCE_MAXIMUM_BYTES,
            response_resource_total_bytes: DEFAULT_RESPONSE_RESOURCE_TOTAL_BYTES,
            response_resource_maximum_entries: DEFAULT_RESPONSE_RESOURCE_MAXIMUM_ENTRIES,
            response_resource_lifetime_secs: DEFAULT_RESPONSE_RESOURCE_LIFETIME_SECS,
            maximum_request_nesting_depth: DEFAULT_MAXIMUM_REQUEST_NESTING_DEPTH,
            maximum_request_values: DEFAULT_MAXIMUM_REQUEST_VALUES,
            maximum_output_values: DEFAULT_MAXIMUM_OUTPUT_VALUES,
            provider_wire_maximum_bytes: DEFAULT_PROVIDER_WIRE_MAXIMUM_BYTES,
            maximum_http_response_bytes: DEFAULT_MAXIMUM_HTTP_RESPONSE_BYTES,
            maximum_registered_tools: DEFAULT_MAXIMUM_REGISTERED_TOOLS,
            maximum_discoverable_resources: DEFAULT_MAXIMUM_DISCOVERABLE_RESOURCES,
            maximum_schema_bytes: DEFAULT_MAXIMUM_SCHEMA_BYTES,
        }
    }
}

/// What `Debug` prints in place of a configured token.
const REDACTED: &str = "<redacted>";

impl fmt::Debug for McpServerConfiguration {
    /// Hand-written so the bearer token never reaches a log line or a
    /// configuration dump. Every other field is rendered normally; the token
    /// renders as its presence, not its value.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("McpServerConfiguration")
            .field("enabled", &self.enabled)
            .field(
                "authorization_token",
                &if self.authorization_token.is_empty() {
                    ""
                } else {
                    REDACTED
                },
            )
            .field("allowed_origins", &self.allowed_origins)
            .field("maximum_in_flight_requests", &self.maximum_in_flight_requests)
            .field("requests_per_minute", &self.requests_per_minute)
            .field("request_burst", &self.request_burst)
            .field("tool_timeout_millis", &self.tool_timeout_millis)
            .field("reply_inline_maximum_bytes", &self.reply_inline_maximum_bytes)
            .field("response_inline_maximum_bytes", &self.response_inline_maximum_bytes)
            .field("response_resource_maximum_bytes", &self.response_resource_maximum_bytes)
            .field("response_resource_total_bytes", &self.response_resource_total_bytes)
            .field("response_resource_maximum_entries", &self.response_resource_maximum_entries)
            .field("response_resource_lifetime_secs", &self.response_resource_lifetime_secs)
            .field("maximum_request_nesting_depth", &self.maximum_request_nesting_depth)
            .field("maximum_request_values", &self.maximum_request_values)
            .field("maximum_output_values", &self.maximum_output_values)
            .field("provider_wire_maximum_bytes", &self.provider_wire_maximum_bytes)
            .field("maximum_http_response_bytes", &self.maximum_http_response_bytes)
            .field("maximum_registered_tools", &self.maximum_registered_tools)
            .field("maximum_discoverable_resources", &self.maximum_discoverable_resources)
            .field("maximum_schema_bytes", &self.maximum_schema_bytes)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::{McpServerConfiguration, REDACTED};

    /// The token is the one field whose value must never reach a log line or
    /// a `--print-config` rendering, and the only thing standing between it
    /// and both is this hand-written `Debug`. This fails if someone replaces
    /// it with `#[derive(Debug)]` — an edit that looks like tidying and
    /// silently starts printing the credential.
    #[test]
    fn debug_redacts_a_configured_token() {
        let rendered =
            format!("{:?}", McpServerConfiguration { authorization_token: "s3cret".into(), ..Default::default() });

        assert!(!rendered.contains("s3cret"), "token leaked into Debug output: {rendered}");
        assert!(rendered.contains(REDACTED), "token presence not reported: {rendered}");
    }

    /// An unset token must be distinguishable from a set one in a dump, so an
    /// operator diagnosing a `401` can see which of the two states they are
    /// in. This fails if the redaction collapses both to the same rendering.
    #[test]
    fn debug_distinguishes_an_unset_token() {
        let rendered = format!("{:?}", McpServerConfiguration::default());

        assert!(!rendered.contains(REDACTED), "an unset token rendered as if it were set: {rendered}");
    }
}
