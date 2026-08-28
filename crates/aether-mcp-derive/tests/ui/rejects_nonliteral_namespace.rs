//! Every minted kind name is built from `NAMESPACE` at expansion time, so it
//! has to be a literal the macro can read rather than a constant it cannot.

use aether_mcp_derive as mcp;

struct Provider;
trait Actor {}

const ELSEWHERE: &str = "aether.test";

#[mcp::router]
impl Actor for Provider {
    const NAMESPACE: &'static str = ELSEWHERE;
}

fn main() {}
