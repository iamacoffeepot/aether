use aether_bloomery_view::{View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.fail.attribute.event")]
struct Event;

#[derive(Default)]
struct Aggregate {
    cursor: ViewCursor,
}

#[view(cursor = cursor)]
impl View for Aggregate {
    #[fold(extra)]
    fn event(&mut self, _event: Event) {}
}

fn main() {}
