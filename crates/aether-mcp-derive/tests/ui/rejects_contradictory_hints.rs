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

    #[mcp::tool(name = "confused", description = "States two things at once.", read_only, destructive)]
    fn confused(&mut self, _context: Context<'_, ()>, _input: Input) -> Result<Output, ToolError> {
        todo!()
    }
}

fn main() {}
