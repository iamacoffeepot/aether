use std::convert::Infallible;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;

use aether_bloomery_kinds::{Entry, Ref, Seq};
use aether_bloomery_view::{ArtifactResolver, ResolveError, View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.pass.async.value")]
struct Value(u64);

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.pass.async.event")]
struct Event(Ref<Value>);

#[derive(Default)]
struct Aggregate {
    cursor: ViewCursor,
    total: u64,
}

#[view(cursor = cursor)]
impl View for Aggregate {
    #[fold]
    async fn resolve(&mut self, event: Event, artifacts: &mut ArtifactResolver) -> Result<(), ResolveError> {
        self.total += artifacts.read(event.0).await?.0;
        Ok(())
    }

    #[fold]
    fn observe(&mut self, _event: Event) {}
}

#[derive(Default)]
struct SynchronousLocal {
    cursor: ViewCursor,
    value: Rc<u64>,
}

#[view(cursor = cursor)]
impl View for SynchronousLocal {
    #[fold]
    fn observe(&mut self, _event: Event) {
        self.value = Rc::new(*self.value + 1);
    }
}

struct ManualLocal {
    cursor: Seq,
    value: Rc<u64>,
}

impl View for ManualLocal {
    type Error = Infallible;
    type Advance<'a> = Pin<Box<dyn Future<Output = Result<(), Self::Error>> + 'a>>;

    fn empty() -> Self {
        Self { cursor: Seq(0), value: Rc::new(0) }
    }

    fn cursor(&self) -> Seq {
        self.cursor
    }

    fn advance<'a>(&'a mut self, entries: &'a [Entry], _artifacts: &'a mut ArtifactResolver) -> Self::Advance<'a> {
        Box::pin(async move {
            std::future::ready(()).await;
            self.value = Rc::new(*self.value + entries.len() as u64);
            if let Some(last) = entries.last() {
                self.cursor = last.seq;
            }
            Ok(())
        })
    }
}

fn requires_send<T: Send>(_: T) {}

fn main() {
    let (mut artifacts, _) = ArtifactResolver::operation();
    let mut aggregate = Aggregate::empty();
    requires_send(aggregate.advance(&[], &mut artifacts));
    SynchronousLocal::empty().advance_ready(&[]).unwrap();
    ManualLocal::empty().advance_ready(&[]).unwrap();
}
