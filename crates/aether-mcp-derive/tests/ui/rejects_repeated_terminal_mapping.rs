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
struct Reply;

#[mcp::router]
impl Actor for Provider {
    const NAMESPACE: &'static str = "aether.test";

    #[mcp::tool(name = "deferring", description = "Defers once but is mapped twice.")]
    fn deferring(&mut self, _context: Context<'_, ()>, _input: Input) -> Outcome<Output> {
        todo!()
    }

    #[mcp::reply(Reply, tool = deferring)]
    fn map_once(&mut self, _ctx: &mut (), _reply: Reply) -> Result<Output, ToolError> {
        todo!()
    }

    #[mcp::reply(Reply, tool = deferring)]
    fn map_twice(&mut self, _ctx: &mut (), _reply: Reply) -> Result<Output, ToolError> {
        todo!()
    }
}

fn main() {}
