// Sibling runtime stub for `rejects_struct_no_handler.rs` — read off disk by
// the struct-hosted `#[actor]` harvest, never compiled as a fixture itself. It
// has an impl but no `#[handler]`, so the harvest finds nothing to lift.
struct Whatever;
impl SomeTrait for Whatever {
    const NAMESPACE: &'static str = "x";
}
