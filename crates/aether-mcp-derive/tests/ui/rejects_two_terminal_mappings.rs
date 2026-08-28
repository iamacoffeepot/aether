//! A deferred tool answers exactly once. Two mappings on *different* reply
//! kinds pass the same-kind duplicate check and are caught by the count.

use aether_mcp_derive as mcp;

struct Provider;
trait Actor {}

struct Context<'a, C>(&'a mut C);
struct ToolError;
struct Input;
struct Output;
enum Outcome<T> {
    Deferred,
    Reply(T),
}
struct FirstReply;
struct SecondReply;

#[mcp::router]
impl Actor for Provider {
    const NAMESPACE: &'static str = "aether.test";

    #[mcp::tool(name = "deferring", description = "Defers once but is answered from two kinds.")]
    fn deferring(&mut self, _context: Context<'_, ()>, _input: Input) -> Outcome<Output> {
        todo!()
    }

    #[mcp::reply(FirstReply, tool = deferring)]
    fn map_first(&mut self, _ctx: &mut (), _reply: FirstReply) -> Result<Output, ToolError> {
        todo!()
    }

    #[mcp::reply(SecondReply, tool = deferring)]
    fn map_second(&mut self, _ctx: &mut (), _reply: SecondReply) -> Result<Output, ToolError> {
        todo!()
    }
}

fn main() {}
