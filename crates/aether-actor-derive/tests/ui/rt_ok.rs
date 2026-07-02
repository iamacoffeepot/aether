// Sibling runtime stub for `accepts_struct_hosted_actor.rs` — read off disk by
// the struct-hosted `#[actor]` harvest, never compiled as a fixture itself
// (its `Self::State` / `Ctx` refs never resolve, but the harvest only parses,
// it does not typecheck). It is an `impl NativeActor` (gap-1 trait filter) with
// a `const NAMESPACE` string literal and one `#[handler]`, so the harvest lifts
// the identity cleanly.
struct RuntimeState;
impl NativeActor for RuntimeState {
    const NAMESPACE: &'static str = "test.struct_hosted_cap";

    #[handler]
    fn on_ping(state: &mut Self::State, ctx: &mut Ctx, mail: Ping) {}
}
