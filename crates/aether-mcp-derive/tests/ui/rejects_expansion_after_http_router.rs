//! `#[mcp::router]` must expand before `#[http::router]`, because composing a
//! tool branch onto an `#[http::reply]` mapper means consuming that marker.
//! The generated dispatchers the HTTP macro leaves behind are the signal that
//! it already ran, and this fixture stands in for that post-expansion shape.

use aether_mcp_derive as mcp;

struct Provider;
trait Actor {}

#[mcp::router]
impl Actor for Provider {
    const NAMESPACE: &'static str = "aether.test";

    fn __aether_route_probe_get(&mut self) {}
}

fn main() {}
