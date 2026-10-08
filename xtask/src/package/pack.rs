use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use aether_chassis::boot_manifest::ChassisSettings;
use aether_chassis::package::{NamedObject, NamespacePath, PackageEntry, PackageManifest, Sha256, encode_manifest};
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

/// Everything that becomes `pack/objects` and `pack/manifest`.
pub(super) struct PackContents<'a> {
    /// The boot components, in autoload order.
    pub(super) components: &'a [PackComponent],
    /// The named objects: each shipped path and the file whose bytes ship
    /// there.
    pub(super) named: &'a BTreeMap<NamespacePath, PathBuf>,
    pub(super) settings: ChassisSettings,
}

/// The workspace license files every depot carries, copied verbatim from the
/// repository root into the depot root beside the chassis binary.
const DEPOT_LICENSE_FILES: [&str; 2] = ["LICENSE-MIT", "LICENSE-APACHE"];

/// Write the depot tree at `out`: copy the chassis binary to
/// `<out>/<chassis_file>` (the host-platform filename, `.exe` on Windows),
/// copy the workspace license files from `workspace_root` via
/// [`copy_licenses`], copy `assets` (when given) into `pack/assets` via
/// [`copy_assets`], then write the `pack/` tree of content-addressed
/// objects (boot components and named objects alike) and `pack/manifest`
/// via [`write_pack`]. Regenerates `out` from
/// scratch so a stale prior run can't leave orphaned objects. Returns the
/// manifest it wrote.
pub(super) fn emit_depot(
    out: &Path,
    workspace_root: &Path,
    chassis_src: &Path,
    chassis_file: &str,
    contents: PackContents<'_>,
    assets: Option<&Path>,
) -> Result<PackageManifest> {
    if out.exists() {
        fs::remove_dir_all(out).with_context(|| format!("clear {}", out.display()))?;
    }
    fs::create_dir_all(out).with_context(|| format!("create {}", out.display()))?;
    copy_artifact(chassis_src, &out.join(chassis_file))?;
    copy_licenses(out, workspace_root)?;
    let manifest = write_pack(out, contents)?;
    if let Some(assets) = assets {
        copy_assets(out, assets)?;
    }
    Ok(manifest)
}

/// The depot's `pack/` subdirectory holding the manifest, the objects, and
/// the shipped asset tree.
const PACK_DIR: &str = "pack";
/// The content-addressed object directory within `pack/`.
const OBJECTS_DIR: &str = "objects";
/// The asset tree within `pack/` — the root the packaged chassis gives the
/// `assets` namespace (`aether_chassis::package::package_assets_root`).
const ASSETS_DIR: &str = "assets";

/// Copy the tree at `src` verbatim into `<out>/pack/assets`, preserving
/// relative paths.
///
/// Verbatim and path-preserving because that is what an asset *is*: a
/// component reads one by mailing the path an author wrote, so a depot that
/// renamed, flattened, or content-addressed the tree would break every one of
/// those reads. This is the one part of `pack/` that is not hash-named.
///
/// Iterative rather than recursive: an asset tree's depth is the author's,
/// not something this command can bound (the workspace rule on recursion over
/// user-supplied data), so the walk carries its own directory stack.
fn copy_assets(out: &Path, src: &Path) -> Result<()> {
    let root = fs::canonicalize(src).with_context(|| format!("locate asset directory {}", src.display()))?;
    let dest_root = out.join(PACK_DIR).join(ASSETS_DIR);
    let mut pending = vec![root.clone()];

    while let Some(dir) = pending.pop() {
        let relative = dir.strip_prefix(&root).expect("every queued directory is under the asset root");
        let dest = dest_root.join(relative);
        fs::create_dir_all(&dest).with_context(|| format!("create {}", dest.display()))?;
        for entry in fs::read_dir(&dir).with_context(|| format!("read {}", dir.display()))? {
            let entry = entry.with_context(|| format!("read an entry of {}", dir.display()))?;
            let path = entry.path();
            // `file_type` rather than `metadata`, so a symlink is classified
            // as the link it is instead of silently following into a cycle or
            // out of the tree the author named.
            if entry.file_type().with_context(|| format!("stat {}", path.display()))?.is_dir() {
                pending.push(path);
            } else {
                copy_artifact(&path, &dest.join(entry.file_name()))?;
            }
        }
    }
    Ok(())
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
/// optional config) and each named object's file into `pack/objects/<sha256>`,
/// record each named object's sha256 and size under its path, and write the
/// [`encode_manifest`] bytes to `pack/manifest`. The `pack/` subtree is
/// regenerated from scratch so a stale prior run can't leave orphaned objects.
/// Called by the depot [`emit_depot`], which also copies the chassis binary
/// alongside the `pack/` tree. Returns the manifest.
fn write_pack(root: &Path, contents: PackContents<'_>) -> Result<PackageManifest> {
    let PackContents { components, named: named_files, settings } = contents;
    let pack_dir = root.join(PACK_DIR);
    if pack_dir.exists() {
        fs::remove_dir_all(&pack_dir).with_context(|| format!("clear {}", pack_dir.display()))?;
    }
    let objects_dir = pack_dir.join(OBJECTS_DIR);
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

    let mut named = BTreeMap::new();
    for (path, file) in named_files {
        let bytes = fs::read(file).with_context(|| format!("read named object {}", file.display()))?;
        let sha256 = write_object(&objects_dir, &bytes)?;
        named.insert(path.clone(), NamedObject { sha256, size: bytes.len() as u64 });
    }

    let manifest = PackageManifest { settings, entries, named };
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
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    use aether_chassis::boot_manifest::ChassisSettings;
    use aether_chassis::package::{NamespacePath, Sha256, decode_manifest, package_assets_root};
    use sha2::{Digest, Sha256 as Sha256Hasher};

    use super::{DEPOT_LICENSE_FILES, PackComponent, PackContents, emit_depot, write_pack};
    use crate::cargo::Profile;
    use crate::package::build::{build_planned_components, resolve_named};
    use crate::package::plan::{FlagSelection, PackageChassis, PackagePlan, resolve_package_plan};

    /// A package that names no objects.
    static NO_NAMED: BTreeMap<NamespacePath, PathBuf> = BTreeMap::new();

    /// The pack contents of boot `components` alone.
    fn boot_only(components: &[PackComponent], settings: ChassisSettings) -> PackContents<'_> {
        PackContents { components, named: &NO_NAMED, settings }
    }

    /// Resolve the `--spec` file at `spec_path` as the command does, under
    /// the desktop flag default.
    fn spec_plan(spec_path: &Path) -> anyhow::Result<PackagePlan> {
        let flags = FlagSelection {
            chassis: PackageChassis::Desktop,
            components: &[],
            configs: &[],
            named: &[],
            settings: ChassisSettings::default(),
        };
        resolve_package_plan(Some(spec_path), flags)
    }

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
        let contents = boot_only(&components, ChassisSettings::default());
        let manifest = emit_depot(&out, &license_root, &chassis_src, "aether-desktop", contents, None).expect("emit");

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
            clear_color: Some("f6f2e9".to_owned()),
        };
        let manifest = write_pack(&root, boot_only(&components, settings.clone())).expect("write pack");

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
            "clear_color": "f6f2e9",
            "components": [
                { "wasm": "alpha.wasm", "config": "alpha.cfg", "name": "first", "export": "entry" },
                { "wasm": "beta.wasm" }
            ]
        }"#;
        let spec_path = dir.join("depot.json");
        fs::write(&spec_path, spec).expect("write spec");

        // `--chassis desktop` is the flag default; the spec's `headless` wins.
        let plan = spec_plan(&spec_path).expect("resolve spec plan");
        assert_eq!(plan.chassis, PackageChassis::Headless, "spec chassis overrides the flag default");

        let (_, chassis_bin) = plan.chassis.substrate();
        assert_eq!(chassis_bin, "aether-headless", "headless selection ships the headless bin");

        let components = build_planned_components(&plan, Path::new("unused-for-prebuilt"), Profile::Release)
            .expect("read prebuilt components");

        let chassis_src = dir.join("fake-chassis");
        fs::write(&chassis_src, b"headless-binary-bytes").expect("write fake chassis");
        write_license_root(&dir);
        let out = dir.join("depot");
        let manifest = emit_depot(&out, &dir, &chassis_src, chassis_bin, boot_only(&components, plan.settings), None)
            .expect("emit depot");

        let manifest_bytes = fs::read(out.join("pack").join("manifest")).expect("read manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the emitted manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what emit_depot wrote");
        assert_eq!(decoded.settings.title.as_deref(), Some("loco-motion"), "spec title rides into the manifest");
        assert_eq!(decoded.settings.clear_color.as_deref(), Some("f6f2e9"), "spec clear_color rides into the manifest");
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

    /// A fresh scratch directory for one named-object test.
    fn named_scratch(tag: &str) -> PathBuf {
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-xtask-named-{tag}-{}-{seq}", process::id()));
        fs::create_dir_all(dir.join("squares").join("50")).expect("create the named directory");
        dir
    }

    #[test]
    fn spec_driven_emit_ships_named_objects_under_their_paths() {
        // A spec's `named` list ships objects boot does not load: every file
        // beneath a `dir`, at its relative path under `under`, and a single
        // `wasm` at its explicit path. Each is written under its sha256 and
        // recorded in the manifest with that hash and its size, which is what
        // the packaged chassis checks at boot and reads by path. The bugs
        // this catches: a directory file dropped or flattened, an object
        // recorded under the wrong hash or size, or a table the chassis's
        // own `decode_manifest` (the oracle) cannot read.
        use std::fs;

        let dir = named_scratch("emit");
        fs::write(dir.join("boot.wasm"), [0x00, 0x61, 0x73, 0x6d, 1]).expect("write boot wasm");
        fs::write(dir.join("late.wasm"), [0x00, 0x61, 0x73, 0x6d, 2, 2]).expect("write late wasm");
        fs::write(dir.join("squares").join("top.bin"), b"top").expect("write a directory file");
        fs::write(dir.join("squares").join("50").join("50.bin"), b"nested!").expect("write a nested file");
        let spec = r#"{
            "components": [ { "wasm": "boot.wasm" } ],
            "named": [
                { "dir": { "from": "squares", "under": "world/squares" } },
                { "wasm": { "file": "late.wasm", "path": "modules/late.wasm" } }
            ]
        }"#;
        let spec_path = dir.join("depot.json");
        fs::write(&spec_path, spec).expect("write spec");

        let plan = spec_plan(&spec_path).expect("resolve spec plan");
        let unused = Path::new("unused-for-prebuilt");
        let components = build_planned_components(&plan, unused, Profile::Release).expect("read prebuilt components");
        let named = resolve_named(&plan, unused, Profile::Release).expect("resolve named objects");
        let chassis_src = dir.join("fake-chassis");
        fs::write(&chassis_src, b"chassis-binary-bytes").expect("write fake chassis");
        write_license_root(&dir);
        let out = dir.join("depot");
        let contents = PackContents { components: &components, named: &named, settings: plan.settings };
        let manifest = emit_depot(&out, &dir, &chassis_src, "aether-desktop", contents, None).expect("emit depot");

        let manifest_bytes = fs::read(out.join("pack").join("manifest")).expect("read manifest");
        let decoded = decode_manifest(&manifest_bytes).expect("chassis decoder reads the emitted manifest");
        assert_eq!(decoded, manifest, "the decoded manifest equals what emit_depot wrote");
        let shipped: Vec<(&str, &[u8])> = vec![
            ("modules/late.wasm", &[0x00, 0x61, 0x73, 0x6d, 2, 2]),
            ("world/squares/50/50.bin", b"nested!"),
            ("world/squares/top.bin", b"top"),
        ];
        let paths: Vec<&str> = decoded.named.keys().map(NamespacePath::as_str).collect();
        assert_eq!(paths, shipped.iter().map(|(path, _)| *path).collect::<Vec<_>>());
        for ((path, bytes), object) in shipped.iter().zip(decoded.named.values()) {
            let mut hasher = Sha256Hasher::new();
            hasher.update(bytes);
            assert_eq!(object.sha256, Sha256(hasher.finalize().into()), "{path} is recorded under its own hash");
            assert_eq!(object.size, bytes.len() as u64, "{path} is recorded at its own size");
            let disk = fs::read(out.join("pack").join("objects").join(object.sha256.to_hex()));
            assert_eq!(disk.expect("the object's file exists under its sha256"), *bytes, "{path}");
        }

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn named_objects_refuse_a_taken_path_and_a_file_that_cannot_be_a_path() {
        // Two authored entries resolving to one path would ship one and drop
        // the other, and a file whose name cannot be a path would be
        // unreadable once shipped; both fail the run naming the files, and
        // nothing is skipped.
        use std::fs;

        let dir = named_scratch("refuse");
        fs::write(dir.join("boot.wasm"), [0x00, 0x61, 0x73, 0x6d, 1]).expect("write boot wasm");
        fs::write(dir.join("squares").join("top.bin"), b"top").expect("write a directory file");
        let unused = Path::new("unused-for-prebuilt");
        let resolve = |named: &str| {
            let spec_path = dir.join("depot.json");
            let spec = format!(r#"{{ "components": [ {{ "wasm": "boot.wasm" }} ], "named": [ {named} ] }}"#);
            fs::write(&spec_path, spec).expect("write spec");
            resolve_named(&spec_plan(&spec_path).expect("resolve spec plan"), unused, Profile::Release)
        };

        let taken = resolve(
            r#"{ "dir": { "from": "squares", "under": "world" } },
               { "wasm": { "file": "boot.wasm", "path": "world/top.bin" } }"#,
        )
        .expect_err("a path two entries resolve to is refused")
        .to_string();
        assert!(taken.contains("top.bin") && taken.contains("boot.wasm"), "both source files are named: {taken}");

        fs::write(dir.join("squares").join("50").join("Upper.bin"), b"x").expect("write an uppercase file");
        let unshippable = resolve(r#"{ "dir": { "from": "squares", "under": "world" } }"#)
            .expect_err("a file whose path is not a namespace path is refused")
            .to_string();
        assert!(unshippable.contains("Upper.bin"), "the file is named: {unshippable}");

        fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn emitted_depot_ships_the_asset_tree_where_the_chassis_boot_looks_for_it() {
        // A depot's assets are the one part of `pack/` that is not
        // content-addressed: a component reads one by mailing the path an
        // author wrote, so the tree has to arrive verbatim, at its original
        // relative paths, at the exact root the chassis gives the `assets`
        // namespace. The chassis's own `package_assets_root` is the oracle,
        // the same way `decode_manifest` is the oracle for the manifest.
        //
        // Two emit bugs this catches, both of which leave every other
        // assertion in this file green: copying the tree before `write_pack`
        // (which clears `pack/` wholesale, so the assets vanish), and
        // flattening a nested directory into the asset root.
        use std::env;
        use std::fs;
        use std::process;
        use std::sync::atomic::{AtomicU64, Ordering};

        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let dir = env::temp_dir().join(format!("aether-xtask-assets-{}-{seq}", process::id()));
        fs::create_dir_all(dir.join("assets").join("meshes")).expect("create asset tree");
        write_license_root(&dir);
        fs::write(dir.join("assets").join("teapot.dsl"), b"; teapot").expect("write a root asset");
        fs::write(dir.join("assets").join("meshes").join("box.dsl"), b"; box").expect("write a nested asset");

        let chassis_src = dir.join("fake-chassis");
        fs::write(&chassis_src, b"chassis-binary-bytes").expect("write fake chassis binary");

        let out = dir.join("depot");
        let components = vec![PackComponent {
            wasm: vec![0x00, 0x61, 0x73, 0x6d, 1],
            config: None,
            name: None,
            export: None,
            replicas: None,
        }];
        let contents = boot_only(&components, ChassisSettings::default());
        emit_depot(&out, &dir, &chassis_src, "aether-desktop", contents, Some(&dir.join("assets")))
            .expect("emit depot with assets");

        let root = package_assets_root(&out).expect("the chassis boot finds the asset root");
        assert_eq!(fs::read(root.join("teapot.dsl")).expect("the root asset shipped"), b"; teapot");
        assert_eq!(
            fs::read(root.join("meshes").join("box.dsl")).expect("the nested asset shipped"),
            b"; box",
            "a nested asset keeps its relative path",
        );
        assert!(out.join("pack").join("manifest").exists(), "the manifest survives the asset copy");

        // No `--assets` ships no asset root, so a depot that never named one
        // keeps the ordinary beside-the-binary default at boot.
        let bare = dir.join("bare-depot");
        let contents = boot_only(&components, ChassisSettings::default());
        emit_depot(&bare, &dir, &chassis_src, "aether-desktop", contents, None).expect("emit depot without assets");
        assert!(package_assets_root(&bare).is_none(), "no --assets means no shipped root");

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
        let contents = boot_only(&[], ChassisSettings::default());
        emit_depot(&out, &dir, &chassis_src, "aether-desktop", contents, None).expect("emit depot");

        for file in DEPOT_LICENSE_FILES {
            let shipped = fs::read(out.join(file)).unwrap_or_else(|_| panic!("{file} shipped into the depot root"));
            let source = fs::read(dir.join(file)).expect("read the source license");
            assert_eq!(shipped, source, "{file} is copied verbatim");
        }

        fs::remove_dir_all(&dir).ok();
    }
}
