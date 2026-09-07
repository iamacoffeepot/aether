//! Doctor report overlay is mail-owned: a real tick publishes through the API,
//! and a foreign RPC snapshot cannot overwrite it.

#![cfg(feature = "github")]

use aether_actor::Addressable;
use aether_chassis_bloomery::artifacts::ArtifactsConfig;
use aether_chassis_bloomery::bloomery::{
    Chassis, CheckResult, CoordinatorConfig, DoctorReactorCapability, DoctorReport, DoctorTick, GithubConnectionConfig,
    LatestDoctorReport, NotifyConfig,
};
use aether_chassis_bloomery::session::SessionConfig;
use aether_chassis_bloomery::signing::SigningConfig;
use aether_chassis_bloomery::store::StoreConfig;
use aether_chassis_bloomery::{BloomeryApiCapability, BloomeryChassis, BloomeryEnv};
use aether_harness_bloomery::{Wire, free_port};

const FORGED: &str = "forged_foreign_snapshot";

#[test]
fn a_foreign_rpc_snapshot_does_not_overwrite_the_doctors_last_report() {
    // The plausible bug: GET /view overlays whoever last mailed LatestDoctorReport,
    // including an RPC caller, so a forged dirty pass replaces the doctor's own.
    let fixture = tempfile::tempdir().expect("a fixture root binds");
    let root = fixture.path();
    let store_path = root.join("store.sqlite").to_str().expect("fixture paths are utf-8").to_owned();
    let artifacts_root = root.join("artifacts").to_str().expect("fixture paths are utf-8").to_owned();
    let rpc_port = free_port();
    let http_port = free_port();
    let env = BloomeryEnv {
        rpc_port,
        http_port,
        store: StoreConfig { path: store_path.clone() },
        artifacts: ArtifactsConfig { root: Some(artifacts_root.clone()) },
        github: GithubConnectionConfig::default(),
        notify: NotifyConfig::default(),
        coordinator: CoordinatorConfig {
            poll_interval_secs: 3600,
            store_path,
            artifacts_root: Some(artifacts_root),
            local_lane_enabled: false,
            local_worktree_base: root.join("worktrees").to_str().expect("fixture paths are utf-8").to_owned(),
            lane_target_base: root.join("targets").to_str().expect("fixture paths are utf-8").to_owned(),
            archive_base: root.join("archive").to_str().expect("fixture paths are utf-8").to_owned(),
            ..CoordinatorConfig::default()
        },
        session: SessionConfig::default(),
        signing: SigningConfig::default(),
    };
    let _chassis = BloomeryChassis::build(env).expect("the coordinator boots");
    let mut wire = Wire::connect(rpc_port, "doctor-report-mail");
    wire.set_http_port(http_port);
    wire.await_replayed();

    let doctor = <DoctorReactorCapability as Addressable>::resolve(0, ());
    wire.tick(doctor, &DoctorTick::default());
    let published = wire.doctor().expect("a completed doctor pass overlays GET /view");
    assert!(published.named(FORGED).is_none(), "the doctor's own pass does not carry the forged row: {published:?}");

    let forged = LatestDoctorReport::from(DoctorReport {
        checks: vec![CheckResult {
            name: FORGED.into(),
            statement: "a foreign sender must not become the overlay".into(),
            passed: false,
            divergences: vec!["rpc".into()],
        }],
    });
    let api = <BloomeryApiCapability as Addressable>::resolve(0, ());
    wire.tick(api, &forged);
    let after = wire.doctor().expect("the overlay remains after a foreign snapshot");
    assert_eq!(after, published, "a foreign RPC snapshot must not replace the doctor's last report");
}
