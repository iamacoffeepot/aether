// Nested runtime stub for `accepts_struct_nested_runtime.rs` — read off disk
// by the struct-hosted `#[actor]` harvest through the module-path form
// (`nested::rt_nested`), never compiled as a fixture itself (its
// `Self::State` / `Ctx` refs never resolve, but the harvest only parses, it
// does not typecheck).
struct RuntimeState;
impl NativeActor for RuntimeState {
    const NAMESPACE: &'static str = "test.struct_nested_cap";

    #[handler::single]
    fn on_ping(state: &mut Self::State, ctx: &mut Ctx, mail: Ping) {}
}
