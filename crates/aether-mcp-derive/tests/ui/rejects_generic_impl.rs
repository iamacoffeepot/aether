//! Each tool mints concrete sibling kinds named from the actor type, so a
//! generic impl has no single set of names to mint.

use aether_mcp_derive as mcp;

struct Provider<T>(T);
trait Actor {}

#[mcp::router]
impl<T> Actor for Provider<T> {
    const NAMESPACE: &'static str = "aether.test";
}

fn main() {}
