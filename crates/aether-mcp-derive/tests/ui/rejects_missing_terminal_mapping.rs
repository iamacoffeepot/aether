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

    #[mcp::tool(name = "deferring", description = "Defers with nothing to answer it.")]
    fn deferring(&mut self, _context: Context<'_, ()>, _input: Input) -> Outcome<Output> {
        todo!()
    }
}

fn main() {}
