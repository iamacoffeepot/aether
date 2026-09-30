use core::convert::Infallible;

use aether_bloomery_kinds::Ref;
use aether_bloomery_program::{At, Cited, CitedError, View, ViewCursor, view};

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

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.pass.cited_event")]
struct CitedEvent {
    cites: Ref<Event>,
}

#[derive(Default)]
struct Reads {
    cursor: ViewCursor,
    seen: u64,
}

#[view(cursor = cursor)]
impl View for Reads {
    #[fold]
    fn read(&mut self, event: CitedEvent, cited: &Cited) -> Result<(), CitedError> {
        self.seen += cited.get(event.cites)?.0;
        Ok(())
    }

    #[fold]
    fn count(&mut self, event: Event) {
        self.seen += event.0;
    }
}

#[derive(Default)]
struct Positions {
    cursor: ViewCursor,
    last: Option<At>,
}

#[view(cursor = cursor)]
impl View for Positions {
    #[fold]
    fn placed(&mut self, _event: Event, at: At) {
        self.last = Some(at);
    }

    #[fold]
    fn read(&mut self, event: CitedEvent, at: At, cited: &Cited) -> Result<(), CitedError> {
        cited.get(event.cites)?;
        self.last = Some(at);
        Ok(())
    }
}

fn main() {
    let aggregate = Aggregate::empty();
    let _ = aggregate.cursor();

    let mut reads = Reads::empty();
    let _ = reads.advance_cited(&[], &[]);

    let mut positions = Positions::empty();
    let _ = positions.advance(&[]);
    let _ = positions.last;
}
