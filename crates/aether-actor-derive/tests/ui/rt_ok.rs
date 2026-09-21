// Sibling runtime stub for `accepts_struct_hosted_actor.rs` — read off disk by
// the struct-hosted `#[actor]` harvest, never compiled as a fixture itself
// (its `Self::State` / `Ctx` refs never resolve, but the harvest only parses,
// it does not typecheck). It is an `impl NativeActor` (gap-1 trait filter) with
// a `const NAMESPACE` string literal and reply-bearing handlers, so the harvest
// lifts both the identity and the reply markers cleanly.
struct RuntimeState;
impl NativeActor for RuntimeState {
    const NAMESPACE: &'static str = "test.struct_hosted_cap";

    #[handler::single]
    fn on_ping(state: &mut Self::State, ctx: &mut Ctx, mail: Ping) -> Pong {
        Pong { seq: mail.seq }
    }

    #[handler::multi]
    fn on_query(state: &mut Self::State, ctx: &mut Ctx<Erased, Multi<Row>>, query: Query) {}
}
