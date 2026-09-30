use aether_bloomery_program::{View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.fail.impl.event")]
struct Event;

#[derive(Default)]
struct Generic<T> {
    cursor: ViewCursor,
    marker: core::marker::PhantomData<T>,
}

#[view(cursor = cursor)]
impl<T: 'static> View for Generic<T> {
    #[fold]
    fn event(&mut self, _event: Event) {}
}

#[derive(Default)]
struct Manual {
    cursor: ViewCursor,
}

#[view(cursor = cursor)]
impl View for Manual {
    type Error = core::convert::Infallible;

    #[fold]
    fn event(&mut self, _event: Event) {}
}

fn main() {}
