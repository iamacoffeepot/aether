use aether_bloomery_view::{ArtifactResolver, ResolveError, View, ViewCursor, view};

#[derive(Clone, aether_data::Storage)]
#[kind(name = "test.view.fail.async.event")]
struct Event;

#[derive(Default)]
struct SharedResolver {
    cursor: ViewCursor,
}

#[view(cursor = cursor)]
impl View for SharedResolver {
    #[fold]
    async fn event(&mut self, _event: Event, _artifacts: &ArtifactResolver) -> Result<(), ResolveError> {
        Ok(())
    }
}

#[derive(Default)]
struct MissingResult {
    cursor: ViewCursor,
}

#[view(cursor = cursor)]
impl View for MissingResult {
    #[fold]
    async fn event(&mut self, _event: Event, _artifacts: &mut ArtifactResolver) {}
}

#[derive(Default)]
struct WrongResolver {
    cursor: ViewCursor,
}

#[view(cursor = cursor)]
impl View for WrongResolver {
    #[fold]
    async fn event(&mut self, _event: Event, _artifacts: &mut u64) -> Result<(), ResolveError> {
        Ok(())
    }
}

fn main() {}
