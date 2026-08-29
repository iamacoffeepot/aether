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

    #[mcp::tool(name = "same_name", description = "The first claim.")]
    fn first(&mut self, _context: Context<'_, ()>, _input: Input) -> Result<Output, ToolError> {
        todo!()
    }

    #[mcp::tool(name = "same_name", description = "The second claim.")]
    fn second(&mut self, _context: Context<'_, ()>, _input: Input) -> Result<Output, ToolError> {
        todo!()
    }
}

fn main() {}
