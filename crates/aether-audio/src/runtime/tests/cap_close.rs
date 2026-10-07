use super::*;
use aether_fs::{FsCapability, NamespaceRoots};
use aether_substrate::testing::{PumpedDriver, boot_bare_test_chassis, cleanup, fresh_substrate_and_rx, scratch_dir};
use aether_substrate::{BootError, Subname};

#[repr(C)]
#[aether_data::kind(name = "test.audio.probe.close", pod, default, eq)]
struct ProbeClose;

struct Probe;

#[aether_actor::actor(instanced, root, depends(AudioCapability))]
impl NativeActor for Probe {
    const NAMESPACE: &'static str = "test.audio.probe";
    type Config = ();

    fn init((): (), _ctx: &mut NativeInitCtx<'_>) -> Result<Self, BootError> {
        Ok(Self)
    }

    fn wire(_state: &mut Self, ctx: &mut NativeCtx<'_>) -> Result<(), BootError> {
        ctx.send::<AudioCapability>(&NoteOn { pitch: 60, velocity: 100, instrument_id: 0, pan: 0 });
        ctx.send::<AudioCapability>(&PlayTrack {
            namespace: "assets".to_owned(),
            path: "ghost.wav".to_owned(),
            gain: 1.0,
            looping: true,
            lane: None,
        });
        Ok(())
    }

    #[handler::tell]
    fn on_close(&mut self, ctx: &mut NativeCtx<'_>, _close: ProbeClose) {
        ctx.shutdown();
    }
}

#[test]
fn close_purges_sender_state_and_in_flight_load() {
    let (registry, mailer, _egress) = fresh_substrate_and_rx();
    let chassis = boot_bare_test_chassis(&registry, &mailer);
    let scratch = scratch_dir("audio", "cap-close");
    let roots =
        NamespaceRoots { save: scratch.join("save"), assets: scratch.join("assets"), config: scratch.join("config") };
    let _fs = chassis.boot_pumped_actor::<FsCapability>(roots, ()).expect("the fs cap boots");
    let mut driver: PumpedDriver<AudioCapability> =
        PumpedDriver::boot(chassis, AudioConfig { output: AudioOutput::Null, requested_sample_rate: None }, ());
    let probe = driver
        .chassis()
        .spawn_actor_for_test::<Probe>(Subname::Named("probe"), (), ())
        .finish()
        .expect("the probe spawns");
    driver.pump_until("the probe owns a monitor and a track load", |state| {
        let watched = !state.monitors.is_empty();
        let loading = !state.track_loads.is_empty();
        watched && loading
    });
    let root = driver.send_tracked(probe, &ProbeClose, None);
    driver.settle(&[root]);
    driver.chassis().await_closed(probe.erase());
    driver.pump_until("the departed probe is purged", |state| {
        let monitors_empty = state.monitors.is_empty();
        let loads_empty = state.track_loads.is_empty();
        monitors_empty && loads_empty
    });
    cleanup(&scratch);
}
