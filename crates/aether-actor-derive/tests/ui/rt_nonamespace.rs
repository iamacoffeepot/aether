// Sibling runtime stub for `rejects_struct_no_namespace.rs` — read off disk by
// the harvest, never compiled as a fixture. It is an `impl NativeActor` (so it
// passes the gap-1 trait filter) with a `#[handler]` but no `const NAMESPACE`,
// so the harvest errors on the missing name.
struct Whatever;
impl NativeActor for Whatever {
    #[handler]
    fn on_x(state: &mut Self::State, ctx: &mut Ctx, mail: Ping) {}
}
