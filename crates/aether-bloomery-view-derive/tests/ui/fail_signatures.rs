use aether_bloomery_view::{View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.fail.event")]
struct Event;

macro_rules! aggregate {
    ($name:ident) => {
        #[derive(Default)]
        struct $name {
            cursor: ViewCursor,
        }
    };
}

aggregate!(Async);
#[view(cursor = cursor)]
impl View for Async {
    #[fold]
    async fn event(&mut self, _event: Event) {}
}

aggregate!(Const);
#[view(cursor = cursor)]
impl View for Const {
    #[fold]
    const fn event(&mut self, _event: Event) {}
}

aggregate!(Unsafe);
#[view(cursor = cursor)]
impl View for Unsafe {
    #[fold]
    unsafe fn event(&mut self, _event: Event) {}
}

aggregate!(Generic);
#[view(cursor = cursor)]
impl View for Generic {
    #[fold]
    fn event<T>(&mut self, _event: Event) {}
}

aggregate!(Receiver);
#[view(cursor = cursor)]
impl View for Receiver {
    #[fold]
    fn event(&self, _event: Event) {}
}

aggregate!(Borrowed);
#[view(cursor = cursor)]
impl View for Borrowed {
    #[fold]
    fn event(&mut self, _event: &Event) {}
}

aggregate!(Extra);
#[view(cursor = cursor)]
impl View for Extra {
    #[fold]
    fn event(&mut self, _event: Event, _other: Event) {}
}

aggregate!(Output);
#[view(cursor = cursor)]
impl View for Output {
    #[fold]
    fn event(&mut self, _event: Event) -> bool {
        true
    }
}

aggregate!(Abi);
#[view(cursor = cursor)]
impl View for Abi {
    #[fold]
    extern "C" fn event(&mut self, _event: Event) {}
}

fn main() {}
