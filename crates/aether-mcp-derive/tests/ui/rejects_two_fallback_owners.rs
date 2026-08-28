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

    #[mcp::tool(name = "first_tool", description = "Deferred, mapped through the first owner.")]
    fn first_tool(&mut self, _context: Context<'_, ()>, _input: Input) -> Outcome<Output> {
        todo!()
    }

    #[mcp::tool(name = "second_tool", description = "Deferred, mapped through the second owner.")]
    fn second_tool(&mut self, _context: Context<'_, ()>, _input: Input) -> Outcome<Output> {
        todo!()
    }

    #[mcp::reply(Reply, tool = first_tool, map = map_first)]
    #[handler::manual]
    fn owner_one(&mut self, ctx: &mut (), reply: Reply) {
        todo!()
    }

    #[mcp::reply(Reply, tool = second_tool, map = map_second)]
    #[handler::manual]
    fn owner_two(&mut self, ctx: &mut (), reply: Reply) {
        todo!()
    }
}

fn main() {}
