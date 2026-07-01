// Sibling runtime stub for `rejects_struct_ambiguous_runtime.rs` — read off disk
// by the harvest, never compiled as a fixture. It has two `#[handler]`-bearing
// `impl NativeActor` blocks (as a platform-cfg'd split cap would), so the
// cfg-blind harvest cannot choose between them and errors on the ambiguity.
struct RuntimeA;
impl NativeActor for RuntimeA {
    const NAMESPACE: &'static str = "test.ambiguous_cap";

    #[handler]
    fn on_x(state: &mut Self::State, ctx: &mut Ctx, mail: Ping) {}
}

struct RuntimeB;
impl NativeActor for RuntimeB {
    const NAMESPACE: &'static str = "test.ambiguous_cap";

    #[handler]
    fn on_y(state: &mut Self::State, ctx: &mut Ctx, mail: Pong) {}
}
