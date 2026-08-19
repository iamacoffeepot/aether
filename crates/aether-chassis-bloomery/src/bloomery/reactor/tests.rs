//! Typed `AdmitResult` handlers on the fire-and-forget admitting reactors.
//!
//! Control replies every admit, including a deterministic refuse. These reactors
//! used to drop that reply, so a bad payload re-drove forever behind one
//! boot-time dispatch-miss warn.

use std::fmt::{Debug, Write as _};
use std::sync::{Arc, Mutex};

use aether_actor::Manual;
use aether_bloomery::{AdmitResult, Event};
use aether_data::wire::from_bytes;
use aether_data::{Kind, MailId, MailboxId, Source};
use aether_substrate::actor::native::ctx::NativeCtx;
use aether_substrate::testing::test_mailer_and_rx;
use aether_substrate::{NativeActor, NativeBinding};
use tracing::field::{Field, Visit};
use tracing::span::{Attributes, Id, Record};
use tracing::subscriber::with_default;
use tracing::{Event as TracingEvent, Metadata, Subscriber};

use super::{
    ClaimReleaseReactorCapability, ClaimReleaseReactorState, ExecutorReactorCapability, ExecutorReactorState,
    IntegrateReactorCapability, IntegrateReactorState, LandReactorCapability, LandReactorState,
};
use crate::control::{ControlCore, ControlCoreState};

fn dispatch_result<A: NativeActor>(
    state: &mut A::State,
    binding: &Arc<NativeBinding>,
    result: &AdmitResult,
) -> Option<()> {
    let mut ctx = NativeCtx::<Manual, A>::new_for_actor(binding, Source::NONE, MailId::NONE, MailId::NONE);
    A::dispatch(state, &mut ctx, AdmitResult::ID, &result.encode_into_bytes())
}

/// The Err control replies when `Admit.event` does not decode as [`Event`] —
/// the deterministic refuse a fire-and-forget admitter used to swallow.
fn decode_failed_result() -> AdmitResult {
    match from_bytes::<Event>(&[0xff]) {
        Err(error) => AdmitResult::Err { error: format!("admit decode failed: {error}") },
        Ok(_) => panic!("garbage must not decode as Event"),
    }
}

#[derive(Default)]
struct RecordedEvents(Mutex<Vec<String>>);

impl RecordedEvents {
    fn rendered(&self) -> String {
        self.0.lock().unwrap().join("\n")
    }
}

struct EventRecorder(Arc<RecordedEvents>);

struct RenderedEvent(String);

impl Visit for RenderedEvent {
    fn record_debug(&mut self, field: &Field, value: &dyn Debug) {
        let _ = write!(self.0, " {}={value:?}", field.name());
    }
}

impl Subscriber for EventRecorder {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _attributes: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &TracingEvent<'_>) {
        let mut rendered = RenderedEvent(event.metadata().level().to_string());
        event.record(&mut rendered);
        self.0.0.lock().unwrap().push(rendered.0);
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

#[test]
fn a_refused_admission_is_a_reactor_error_event_not_silence() {
    // Tripwire: fire-and-forget Admit replies used to miss dispatch (one boot
    // warn, then silence). A deterministic refuse — the decode-failed Err
    // control replies for a bad payload — must surface as an error event on
    // every admitting reactor, not vanish.
    let (mailer, _rx) = test_mailer_and_rx();
    let mailbox = MailboxId(0);
    let binding = Arc::new(NativeBinding::new_for_test(Arc::clone(&mailer), mailbox));

    let mut executor = ExecutorReactorState::with_parts(None, None, Arc::clone(&mailer), mailbox);
    let mut integrate = IntegrateReactorState::with_parts(None, None, Arc::clone(&mailer), mailbox);
    let mut land = LandReactorState::with_parts(None, None, Arc::clone(&mailer), mailbox);
    let mut claim_release = ClaimReleaseReactorState::with_parts(None, None, Arc::clone(&mailer), mailbox);
    let mut control = ControlCoreState::inert(mailer);

    let refused = decode_failed_result();
    let accepted = AdmitResult::Ok { outcome: Vec::new() };
    let events = Arc::new(RecordedEvents::default());

    with_default(EventRecorder(Arc::clone(&events)), || {
        assert!(
            dispatch_result::<ExecutorReactorCapability>(&mut executor, &binding, &refused).is_some(),
            "executor must handle AdmitResult rather than miss dispatch",
        );
        assert!(
            dispatch_result::<IntegrateReactorCapability>(&mut integrate, &binding, &refused).is_some(),
            "integrate must handle AdmitResult rather than miss dispatch",
        );
        assert!(
            dispatch_result::<LandReactorCapability>(&mut land, &binding, &refused).is_some(),
            "land must handle AdmitResult rather than miss dispatch",
        );
        assert!(
            dispatch_result::<ClaimReleaseReactorCapability>(&mut claim_release, &binding, &refused).is_some(),
            "claim_release must handle AdmitResult rather than miss dispatch",
        );
        assert!(
            dispatch_result::<ControlCore>(&mut control, &binding, &refused).is_some(),
            "control must handle AdmitResult rather than miss dispatch",
        );

        assert!(dispatch_result::<ExecutorReactorCapability>(&mut executor, &binding, &accepted).is_some());
        assert!(dispatch_result::<IntegrateReactorCapability>(&mut integrate, &binding, &accepted).is_some());
        assert!(dispatch_result::<LandReactorCapability>(&mut land, &binding, &accepted).is_some());
        assert!(dispatch_result::<ClaimReleaseReactorCapability>(&mut claim_release, &binding, &accepted).is_some());
        assert!(dispatch_result::<ControlCore>(&mut control, &binding, &accepted).is_some());
    });

    let rendered = events.rendered();
    let errors = rendered.lines().filter(|line| line.contains("ERROR")).count();
    assert_eq!(errors, 5, "each refused admit is one error; Ok is silent: {rendered}");
    assert!(rendered.contains("admit decode failed"), "the event names the control-seam failure: {rendered}");
}
