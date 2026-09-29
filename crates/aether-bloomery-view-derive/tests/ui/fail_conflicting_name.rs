use aether_bloomery_view::{View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.fail.name.event")]
struct Event;

#[derive(Default)]
struct Aggregate {
    cursor: ViewCursor,
}

#[view(cursor = cursor)]
impl View for Aggregate {
    #[fold]
    fn advance(&mut self, _event: Event) {}
}

fn main() {}
