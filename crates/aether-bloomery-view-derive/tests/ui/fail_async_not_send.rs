use std::rc::Rc;

use aether_bloomery_view::{ArtifactResolver, ResolveError, View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.fail.async.send.event")]
struct Event;

#[derive(Default)]
struct Aggregate {
    cursor: ViewCursor,
}

#[view(cursor = cursor)]
impl View for Aggregate {
    #[fold]
    async fn event(&mut self, _event: Event, _artifacts: &mut ArtifactResolver) -> Result<(), ResolveError> {
        let local = Rc::new(1);
        std::future::ready(()).await;
        drop(local);
        Ok(())
    }
}

fn main() {}
