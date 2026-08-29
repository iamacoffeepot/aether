//! The marker's own expansion is reached only when the enclosing impl has no
//! `#[mcp::router]` to consume it first.

use aether_mcp_derive as mcp;

struct Provider;

impl Provider {
    #[mcp::tool(name = "unrouted", description = "No router consumed this marker.")]
    fn unrouted(&mut self) {}
}

fn main() {}
