use core::convert::Infallible;

use aether_bloomery_view::{View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.pass.event")]
struct Event(u64);

#[derive(Default)]
struct Aggregate {
    cursor: ViewCursor,
    total: u64,
}

#[view(cursor = cursor)]
impl View for Aggregate {
    #[fold]
    fn infallible(&mut self, event: Event) {
        self.total += event.0;
    }

    #[fold]
    fn fallible(&mut self, _event: Event) -> Result<(), Infallible> {
        Ok(())
    }
}

fn main() {
    let aggregate = Aggregate::empty();
    let _ = aggregate.cursor();
}
