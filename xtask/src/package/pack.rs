use std::fs;
use std::path::Path;

use aether_chassis::boot_manifest::ChassisSettings;
use aether_chassis::package::{PackageEntry, PackageManifest, Sha256, encode_manifest};
use anyhow::{Context, Result};
use sha2::{Digest, Sha256 as Sha256Hasher};

use crate::cargo::copy_artifact;

/// One component about to be written into a pack: its load labels plus the
/// wasm (and optional config) bytes that become content-addressed objects.
pub(super) struct PackComponent {
    pub(super) wasm: Vec<u8>,
    pub(super) config: Option<Vec<u8>>,
    pub(super) name: Option<String>,
    pub(super) export: Option<String>,
    pub(super) replicas: Option<u32>,
}

/// The workspace license files every depot carries, copied verbatim from the
/// repository root into the depot root beside the chassis binary.
const DEPOT_LICENSE_FILES: [&str; 2] = ["LICENSE-MIT", "LICENSE-APACHE"];

/// Write the depot tree at `out`: copy the chassis binary to
/// `<out>/<chassis_file>` (the host-platform filename, `.exe` on Windows),
/// copy the workspace license files from `workspace_root` via
/// [`copy_licenses`], then write the `pack/` tree of content-addressed
/// objects and `pack/manifest` via [`write_pack`]. Regenerates `out` from
/// scratch so a stale prior run can't leave orphaned objects. Returns the
/// manifest it wrote.
pub(super) fn emit_depot(
    out: &Path,
    workspace_root: &Path,
    chassis_src: &Path,
    chassis_file: &str,
    components: &[PackComponent],
    settings: ChassisSettings,
) -> Result<PackageManifest> {
    if out.exists() {
        fs::remove_dir_all(out).with_context(|| format!("clear {}", out.display()))?;
    }
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    copy_artifact(chassis_src, &out.join(chassis_file))?;
    copy_licenses(out, workspace_root)?;
    write_pack(out, components, settings)
}

/// Copy each of [`DEPOT_LICENSE_FILES`] from `workspace_root` into the depot
/// root. The shipped chassis binary statically links dependencies whose
/// licenses require the notice to travel with the distributed artifact, so a
/// depot without these files is not redistributable. Same copy mechanics as
/// the chassis binary — a missing source file fails the emit rather than
/// silently shipping an incomplete depot. They are attribution files sitting
/// beside the binary, not content-addressed payload, so they are deliberately
/// not `pack/objects` entries and the manifest (which lists components) does
/// not reference them.
fn copy_licenses(out: &Path, workspace_root: &Path) -> Result<()> {
    for file in DEPOT_LICENSE_FILES {
        copy_artifact(&workspace_root.join(file), &out.join(file))?;
    }
    Ok(())
}

/// Write the `pack/` tree under `<root>/pack`: hash each component's wasm (and
/// optional config) into `pack/objects/<sha256>` and write the
/// [`encode_manifest`] bytes to `pack/manifest`. The `pack/` subtree is
/// regenerated from scratch so a stale prior run can't leave orphaned objects.
/// Called by the depot [`emit_depot`], which also copies the chassis binary
/// alongside the `pack/` tree. Returns the manifest.
fn write_pack(root: &Path, components: &[PackComponent], settings: ChassisSettings) -> Result<PackageManifest> {
    let pack_dir = root.join("pack");
    if pack_dir.exists() {
        fs::remove_dir_all(&pack_dir).with_context(|| format!("clear {}", pack_dir.display()))?;
    }
    let objects_dir = pack_dir.join("objects");
    fs::create_dir_all(&objects_dir).with_context(|| format!("create {}", objects_dir.display()))?;

    let mut entries = Vec::with_capacity(components.len());
    for component in components {
        let object = write_object(&objects_dir, &component.wasm)?;
        let config = match &component.config {
            Some(bytes) => Some(write_object(&objects_dir, bytes)?),
            None => None,
        };
        entries.push(PackageEntry {
            object,
            config,
            name: component.name.clone(),
            export: component.export.clone(),
            replicas: component.replicas,
        });
    }

    let manifest = PackageManifest { settings, entries };
    let manifest_path = pack_dir.join("manifest");
    fs::write(&manifest_path, encode_manifest(&manifest))
        .with_context(|| format!("write {}", manifest_path.display()))?;
    Ok(manifest)
}

/// Hash `bytes` and write them to `<objects_dir>/<lowercase-hex>`, the
/// content-addressed object name the manifest references and the chassis
/// resolves against. Objects are immutable and content-keyed, so an
/// already-present object (a second component with identical bytes) is not
/// rewritten. Returns the [`Sha256`] identity.
fn write_object(objects_dir: &Path, bytes: &[u8]) -> Result<Sha256> {
    let mut hasher = Sha256Hasher::new();
    hasher.update(bytes);
    let object = Sha256(hasher.finalize().into());
    let path = objects_dir.join(object.to_hex());
    if !path.exists() {
        fs::write(&path, bytes).with_context(|| format!("write object {}", path.display()))?;
    }
    Ok(object)
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use aether_chassis::boot_manifest::ChassisSettings;
    use aether_chassis::package::{Sha256, decode_manifest};
    use sha2::{Digest, Sha256 as Sha256Hasher};

    use super::{DEPOT_LICENSE_FILES, PackComponent, emit_depot, write_pack};
    use crate::cargo::Profile;
    use crate::package::build::build_planned_components;
    use crate::package::plan::{PackageChassis, resolve_package_plan};

    /// Write a stand-in workspace root carrying both license files, so an emit
    /// under test reads the same sources `copy_licenses` reads from the real
    /// repository root. Returns nothing — callers pass `dir` as the
    /// `workspace_root` argument.
    fn write_license_root(dir: &Path) {
        use std::fs;

        fs::create_dir_all(dir).expect("create license root");
        for file in DEPOT_LICENSE_FILES {
            fs::write(dir.join(file), format!("{file} body")).expect("write license");
        }
    }

    #[test]
    fn emitted_depot_round_trips_through_decoder() {
        // Tripwire: the depot xtask writes must be readable by the chassis's
        // own `decode_manifest`, and every manifest reference must resolve
        // against `pack/objects` and re-hash to its filename. This catches
        // the emit bugs the target owns — a wrong object filename, a dropped
        // entry, a hash/bytes mismatch, or an encode that its own decoder
        // can't read — using the merged decoder as the oracle. It does not
        // re-test `encode_manifest`/`decode_manifest` symmetry (owned and
        // tested in aether-chassis); it tests that xtask's on-disk layout is
        // what that decoder consumes.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let out = env::temp_dir().join(format!("aether-xtask-package-{}-{seq}", process::id()));

        let chassis_src = env::temp_dir().join(format!("aether-xtask-chassis-{}-{seq}", process::id()));
        fs::write(&chassis_src, b"chassis-binary-bytes").expect("write fake chassis binary");

        let license_root = env::temp_dir().join(format!("aether-xtask-licenses-{}-{seq}", process::id()));
        write_license_root(&license_root);

        // Two distinct components plus a third sharing bytes with the first —
        // the shared-bytes case exercises content-address dedup (one object
        // file, two entries pointing at it).
        let named = |name: &str, wasm: Vec<u8>| PackComponent {
            wasm,
            config: None,
            name: Some(name.to_owned()),
            export: None,
            replicas: None,
        };
        let components = vec![
            named("alpha", vec![0x00, 0x61, 0x73, 0x6d, 1, 2, 3]),
            named("beta", vec![9, 9, 9, 9]),
            named("alpha_twin", vec![0x00, 0x61, 0x73, 0x6d, 1, 2, 3]),
        ];
        let manifest =
            emit_depot(&out, &license_root, &chassis_src, "aether-desktop", &components, ChassisSettings::default())
                .expect("emit");

        assert!(out.join("aether-desktop").exists(), "chassis binary copied into the depot root");

        let manifest_bytes = fs::read(out.join("pack").join("manifest")).expect("read pack/manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the emitted manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what emit_depot wrote");

        let objects_dir = out.join("pack").join("objects");
        for entry in &decoded.entries {
            let object_path = objects_dir.join(entry.object.to_hex());
            let disk = fs::read(&object_path).unwrap_or_else(|_| panic!("object {} exists", entry.object.to_hex()));
            let mut hasher = Sha256Hasher::new();
            hasher.update(&disk);
            let recomputed = Sha256(hasher.finalize().into());
            assert_eq!(recomputed, entry.object, "object file content hashes to its filename");
        }

        // The shared-bytes entries resolve to one object; the two distinct
        // components plus the shared object make two object files.
        let object_count = fs::read_dir(&objects_dir).expect("read objects dir").count();
        assert_eq!(object_count, 2, "content-address dedup writes one file per distinct payload");

        fs::remove_dir_all(&out).ok();
        fs::remove_file(&chassis_src).ok();
        fs::remove_dir_all(&license_root).ok();
    }

    #[test]
    fn write_pack_carries_config_object_settings_and_entry_order() {
        // The bundle path writes richer entries than `emit_depot` exercises —
        // a per-component config object plus the chassis settings (title /
        // window mode / tick rate) the standalone bins apply at boot. This
        // proves `write_pack` writes both the wasm and the config as distinct
        // content-addressed objects, threads the config hash onto the entry,
        // preserves entry order, and round-trips settings through the chassis's
        // own `decode_manifest` (the oracle). The bug it catches is a dropped
        // config object, a settings field lost on the way to the manifest, or a
        // reordered entry list.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let root = env::temp_dir().join(format!("aether-xtask-writepack-{}-{seq}", process::id()));

        let components = vec![
            PackComponent {
                wasm: vec![0x00, 0x61, 0x73, 0x6d, 1],
                config: Some(vec![7, 8, 9]),
                name: Some("first".to_owned()),
                export: None,
                replicas: Some(2),
            },
            PackComponent {
                wasm: vec![0xfe, 0xff],
                config: None,
                name: None,
                export: Some("alt".to_owned()),
                replicas: None,
            },
        ];
        let settings = ChassisSettings {
            title: Some("bundle".to_owned()),
            window_mode: Some("windowed:800x600".to_owned()),
            tick_hz: Some(30),
        };
        let manifest = write_pack(&root, &components, settings.clone()).expect("write pack");

        let manifest_bytes = fs::read(root.join("pack").join("manifest")).expect("read pack/manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the pack manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what write_pack wrote");
        assert_eq!(decoded.settings, settings, "chassis settings round-trip");
        assert_eq!(decoded.entries.len(), 2);
        assert!(decoded.entries[0].config.is_some(), "the first entry carries a config object");
        assert_eq!(decoded.entries[0].name.as_deref(), Some("first"));
        assert_eq!(decoded.entries[0].replicas, Some(2));
        assert_eq!(decoded.entries[1].config, None, "the config-less entry has no config hash");
        assert_eq!(decoded.entries[1].export.as_deref(), Some("alt"));

        // The first entry's wasm + config are two distinct objects; the second
        // entry's wasm is a third — three object files, none shared.
        let objects_dir = root.join("pack").join("objects");
        let object_count = fs::read_dir(&objects_dir).expect("read objects dir").count();
        assert_eq!(object_count, 3, "distinct wasm and config payloads each write one object");

        fs::remove_dir_all(&root).ok();
    }

    #[test]
    fn spec_driven_emit_carries_selected_entries_and_settings() {
        // A `--spec` product emit must carry each selected component's name /
        // export / config and the chassis settings all the way through the
        // chassis's own `decode_manifest`, resolve the spec's relative paths
        // against the spec file's directory, and ship the chosen (here
        // headless) chassis binary. Prebuilt-wasm entries keep the test off
        // cargo. The bugs it catches: a spec field dropped before the
        // manifest, a relative path anchored to the process cwd instead of the
        // spec dir (the prebuilt read would miss the file), the wrong chassis
        // bin shipped, or a config not written as its own object.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-xtask-spec-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create spec dir");

        fs::write(dir.join("alpha.wasm"), [0x00, 0x61, 0x73, 0x6d, 1]).expect("write alpha wasm");
        fs::write(dir.join("beta.wasm"), [0x00, 0x61, 0x73, 0x6d, 2]).expect("write beta wasm");
        fs::write(dir.join("alpha.cfg"), [7, 7, 7]).expect("write alpha config");

        // Component paths are relative — they must resolve against the spec
        // file's directory, not the process cwd.
        let spec = r#"{
            "chassis": "headless",
            "title": "loco-motion",
            "tick_hz": 30,
            "components": [
                { "wasm": "alpha.wasm", "config": "alpha.cfg", "name": "first", "export": "entry" },
                { "wasm": "beta.wasm" }
            ]
        }"#;
        let spec_path = dir.join("depot.json");
        fs::write(&spec_path, spec).expect("write spec");

        // `--chassis desktop` is the flag default; the spec's `headless` wins.
        let plan = resolve_package_plan(Some(&spec_path), PackageChassis::Desktop, &[], &[], None, None, None)
            .expect("resolve spec plan");
        assert_eq!(plan.chassis, PackageChassis::Headless, "spec chassis overrides the flag default");

        let (_, chassis_bin) = plan.chassis.substrate();
        assert_eq!(chassis_bin, "aether-headless", "headless selection ships the headless bin");

        let components = build_planned_components(&plan, Path::new("unused-for-prebuilt"), Profile::Release)
            .expect("read prebuilt components");

        let chassis_src = dir.join("fake-chassis");
        fs::write(&chassis_src, b"headless-binary-bytes").expect("write fake chassis");
        write_license_root(&dir);
        let out = dir.join("depot");
        let settings =
            ChassisSettings { title: plan.title.clone(), window_mode: plan.window_mode.clone(), tick_hz: plan.tick_hz };
        let manifest = emit_depot(&out, &dir, &chassis_src, chassis_bin, &components, settings).expect("emit depot");

        let manifest_bytes = fs::read(out.join("pack").join("manifest")).expect("read manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the emitted manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what emit_depot wrote");
        assert_eq!(decoded.settings.title.as_deref(), Some("loco-motion"), "spec title rides into the manifest");
        assert_eq!(decoded.settings.tick_hz, Some(30), "spec tick rate rides into the manifest");
        assert_eq!(decoded.entries.len(), 2);
        assert_eq!(decoded.entries[0].name.as_deref(), Some("first"));
        assert_eq!(decoded.entries[0].export.as_deref(), Some("entry"));
        assert!(decoded.entries[0].config.is_some(), "the first entry's config rode into the manifest");
        assert_eq!(decoded.entries[1].name, None, "the config-less entry carries no name");
        assert_eq!(decoded.entries[1].config, None, "the config-less entry has no config object");

        assert!(out.join("aether-headless").exists(), "the headless chassis bin is shipped into the depot");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn emitted_depot_ships_the_workspace_license_files() {
        // A depot is redistributed as-is and its chassis binary statically
        // links attribution-bearing dependencies, so both workspace license
        // files must land in the depot root with their bytes intact. The bug
        // it catches: an emit that ships the binary and the `pack/` tree but
        // drops the notices, leaving the depot non-redistributable — the emit
        // stays green because nothing else reads them.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-xtask-license-{}-{seq}", process::id()));
        fs::create_dir_all(&dir).expect("create test dir");
        write_license_root(&dir);

        let chassis_src = dir.join("fake-chassis");
        fs::write(&chassis_src, b"chassis-binary-bytes").expect("write fake chassis binary");

        let out = dir.join("depot");
        emit_depot(&out, &dir, &chassis_src, "aether-desktop", &[], ChassisSettings::default()).expect("emit depot");

        for file in DEPOT_LICENSE_FILES {
            let shipped = fs::read(out.join(file)).unwrap_or_else(|_| panic!("{file} shipped into the depot root"));
            let source = fs::read(dir.join(file)).expect("read the source license");
            assert_eq!(shipped, source, "{file} is copied verbatim");
        }

        fs::remove_dir_all(&dir).ok();
    }
}
