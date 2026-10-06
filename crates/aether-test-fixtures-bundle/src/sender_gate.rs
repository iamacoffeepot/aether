//! Sender-requirement fixtures (issue #7532, ADR-0231 §11): a guest whose
//! handlers state what their sender must handle, and a guest that covers it.
//!
//! `SenderGate` takes a [`SenderGateTake`] and a [`SenderGateDial`] only from a sender
//! that covers [`SenderGateGrantee`]. Each handler counts its run and mails the
//! sender it was handed a [`SenderGateGranted`] through that reference, so a
//! scenario reads from the counts whether a handler ran and from the
//! holder whether the reference reached the sender. A [`SenderGateQuery`] asks
//! nothing of its sender.
//!
//! `SenderGateHolder` declares the gate and covers `SenderGateGrantee`, so its plain
//! `ctx.send::<SenderGate>` of either kind builds. A [`SenderGateTrigger`] sends
//! both, and a [`SenderGateHolderQuery`] reads back what the gate answered.

use aether_actor::{ActorInitError, ProtocolRef, WasmActor, WasmCtx, WasmInitCtx, actor};
use aether_test_fixtures_kinds::{
    SenderGateDial, SenderGateDialed, SenderGateGranted, SenderGateGrantee, SenderGateHolderQuery,
    SenderGateHolderReport, SenderGateQuery, SenderGateReport, SenderGateTake, SenderGateTrigger,
};

#[derive(Default)]
pub struct SenderGate {
    takes: u32,
    dials: u32,
}

#[actor(root)]
impl WasmActor for SenderGate {
    const NAMESPACE: &'static str = "test.sender_gate.gate";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::default())
    }

    #[handler::tell]
    fn on_take(&mut self, ctx: &mut WasmCtx<'_>, take: SenderGateTake, sender: ProtocolRef<SenderGateGrantee>) {
        self.takes += 1;
        ctx.send_to(sender, &SenderGateGranted { tag: take.tag });
    }

    #[handler::request]
    fn on_dial(
        &mut self,
        ctx: &mut WasmCtx<'_>,
        dial: SenderGateDial,
        sender: ProtocolRef<SenderGateGrantee>,
    ) -> SenderGateDialed {
        self.dials += 1;
        ctx.send_to(sender, &SenderGateGranted { tag: dial.tag });

        SenderGateDialed::Ok { tag: dial.tag }
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: SenderGateQuery) -> SenderGateReport {
        SenderGateReport { takes: self.takes, dials: self.dials }
    }
}

#[derive(Default)]
pub struct SenderGateHolder {
    granted: Vec<u32>,
    dialed: Vec<SenderGateDialed>,
}

#[actor(root, depends(SenderGate))]
impl WasmActor for SenderGateHolder {
    const NAMESPACE: &'static str = "test.sender_gate.holder";

    fn init(_ctx: &mut WasmInitCtx<'_>) -> Result<Self, ActorInitError> {
        Ok(Self::default())
    }

    #[handler::tell]
    fn on_trigger(&mut self, ctx: &mut WasmCtx<'_>, trigger: SenderGateTrigger) {
        ctx.send::<SenderGate>(&SenderGateTake { tag: trigger.tag });
        ctx.send::<SenderGate>(&SenderGateDial { tag: trigger.tag });
    }

    #[handler::tell]
    fn on_granted(&mut self, _ctx: &mut WasmCtx<'_>, granted: SenderGateGranted) {
        self.granted.push(granted.tag);
    }

    #[handler::response]
    fn on_dialed(&mut self, _ctx: &mut WasmCtx<'_>, dialed: SenderGateDialed) {
        self.dialed.push(dialed);
    }

    #[handler::request]
    fn on_query(&mut self, _ctx: &mut WasmCtx<'_>, _query: SenderGateHolderQuery) -> SenderGateHolderReport {
        SenderGateHolderReport { granted: self.granted.clone(), dialed: self.dialed.clone() }
    }
}
