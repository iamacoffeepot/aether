//! Runtime-discovered `#[actor]` / `#[handler]` trybuild suite.
//!
//! Fixtures are every `tests/ui/*.rs` except `shard_support.rs` and harvest
//! siblings. Harvest files are the ones named as an `#[actor(…)]` argument by
//! another fixture — today `accepts_struct_hosted_actor.rs:31`,
//! `rejects_struct_ambiguous_runtime.rs:8`, `rejects_struct_no_handler.rs:6`,
//! `rejects_struct_no_namespace.rs:7`,
//! `rejects_actor_child_of_cardinality_native.rs:10`,
//! `rejects_actor_composable_native.rs:8`, and
//! `rejects_generic_native_lineage_struct.rs:6`. Those files are read off disk
//! by the struct-hosted harvest rather than compiled as cases; the `rt_` prefix
//! is the current spelling of that set, not the exclusion rule.
//!
//! A fixture is compile-fail if and only if `tests/ui/<name>.stderr` exists,
//! and a pass case otherwise. Compile-fail cases share one shard so trybuild
//! keeps the pass-free `cargo check --keep-going` path #5133 measured; pass
//! cases are bin-packed by file size across [`PASS_SHARD_COUNT`] shards.
//!
//! Each shard runs in a child of this test binary with `CARGO_TARGET_DIR` set
//! at spawn, so trybuild's project lock is per-shard and this process never
//! mutates its own environment.
//!
//! `.stderr` goldens are toolchain-sensitive — regenerate with
//! `TRYBUILD=overwrite cargo test -p aether-actor-derive --test ui`.

use std::collections::BTreeSet;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};

/// Pass cases are bin-packed across this many shards. Two is the split #5133
/// measured: any single pass shard stays under the ~40s ceiling that made one
/// combined pass batch miss the nextest timeout. The balance recomputes from
/// file size every run.
const PASS_SHARD_COUNT: usize = 2;

const SHARD_SUPPORT_STEM: &str = "shard_support";

const COMPILE_FAIL_SPEC: &str = "compile-fail";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Class {
    CompileFail,
    Pass,
}

struct Fixture {
    stem: String,
    path: PathBuf,
    class: Class,
    size: u64,
}

struct Shard {
    spec: String,
    class: Class,
    files: Vec<Fixture>,
}

#[test]
fn ui() {
    if let Some(spec) = shard_spec_from_args() {
        run_child(&spec);
        return;
    }
    run_parent();
}

fn run_parent() {
    let exe = env::current_exe().expect("resolve the current test executable");
    let children: Vec<(String, Child)> = partition(discover())
        .into_iter()
        .filter(|shard| !shard.files.is_empty())
        .map(|shard| {
            let child = spawn_shard(&exe, &shard.spec);
            (shard.spec, child)
        })
        .collect();

    let mut failures = String::new();
    for (spec, child) in children {
        let output = child.wait_with_output().unwrap_or_else(|e| panic!("wait for shard {spec}: {e}"));
        if !output.status.success() {
            failures.push_str(&relay_failure(&spec, &output));
        }
    }
    assert!(failures.is_empty(), "{failures}");
}

fn run_child(spec: &str) {
    let shard = partition(discover())
        .into_iter()
        .find(|shard| shard.spec == spec)
        .unwrap_or_else(|| panic!("unknown --shard spec {spec}"));

    let t = trybuild::TestCases::new();
    for file in &shard.files {
        match shard.class {
            Class::CompileFail => t.compile_fail(&file.path),
            Class::Pass => t.pass(&file.path),
        }
    }
}

fn spawn_shard(exe: &Path, spec: &str) -> Child {
    let target = shard_target(spec);
    fs::create_dir_all(&target).expect("create per-shard trybuild target");

    Command::new(exe)
        .arg("--exact")
        .arg("ui")
        .arg("--no-capture")
        .arg("--test-threads=1")
        .arg("--")
        .arg("--shard")
        .arg(spec)
        .env("CARGO_TARGET_DIR", &target)
        .current_dir(crate_root())
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("spawn shard {spec}: {e}"))
}

fn shard_target(spec: &str) -> PathBuf {
    Path::new(env!("CARGO_TARGET_TMPDIR")).join("trybuild-shards").join(spec)
}

fn shard_spec_from_args() -> Option<String> {
    let mut args = env::args();
    while let Some(arg) = args.next() {
        if arg == "--shard" {
            return args.next();
        }
        if let Some(spec) = arg.strip_prefix("--shard=") {
            return Some(spec.to_owned());
        }
    }
    None
}

fn relay_failure(spec: &str, output: &Output) -> String {
    let status = output.status;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    format!("\n--- shard {spec} failed (status {status}) ---\n{stdout}\n{stderr}\n")
}

fn discover() -> Vec<Fixture> {
    let ui_dir = fixture_dir();
    let stems = rs_stems_in(&ui_dir);
    let harvest = harvest_named_by_actor_args(&ui_dir, &stems);

    stems.into_iter().filter(|stem| !harvest.contains(stem)).map(|stem| fixture_in(&ui_dir, stem)).collect()
}

/// This crate's root as the running process sees it — the current directory
/// when it holds `tests/ui` (cargo and nextest run the test there), else the
/// compile-time path for a caller running from elsewhere. Runtime-first
/// because a lane-cached test binary can outlive the checkout it was
/// compiled in (dispatch-3177): its baked path then names a pruned tree.
fn crate_root() -> PathBuf {
    match env::current_dir() {
        Ok(dir) if dir.join("tests/ui").is_dir() => dir,
        _ => PathBuf::from(env!("CARGO_MANIFEST_DIR")),
    }
}

fn fixture_dir() -> PathBuf {
    crate_root().join("tests/ui")
}

fn rs_stems_in(ui_dir: &Path) -> BTreeSet<String> {
    let mut stems = BTreeSet::new();
    for entry in fs::read_dir(ui_dir).expect("read tests/ui") {
        let path = entry.expect("read tests/ui entry").path();
        if path.extension() != Some("rs".as_ref()) || !path.is_file() {
            continue;
        }
        let stem = path.file_stem().and_then(|s| s.to_str()).expect("utf-8 fixture name");
        if stem != SHARD_SUPPORT_STEM {
            stems.insert(stem.to_owned());
        }
    }
    stems
}

fn harvest_named_by_actor_args(ui_dir: &Path, stems: &BTreeSet<String>) -> BTreeSet<String> {
    let mut harvest = BTreeSet::new();
    for stem in stems {
        let src =
            fs::read_to_string(ui_dir.join(format!("{stem}.rs"))).unwrap_or_else(|e| panic!("read {stem}.rs: {e}"));
        for arg in actor_attribute_args(&src) {
            if stems.contains(&arg) {
                harvest.insert(arg);
            }
        }
    }
    harvest
}

fn fixture_in(ui_dir: &Path, stem: String) -> Fixture {
    let file = format!("{stem}.rs");
    let abs = ui_dir.join(&file);
    let size = fs::metadata(&abs).unwrap_or_else(|e| panic!("stat {file}: {e}")).len();
    let class = if abs.with_extension("stderr").is_file() {
        Class::CompileFail
    } else {
        Class::Pass
    };
    Fixture { stem, path: Path::new("tests/ui").join(file), class, size }
}

fn actor_attribute_args(src: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut rest = src;
    while let Some(idx) = rest.find("#[actor(") {
        rest = &rest[idx + "#[actor(".len()..];
        let Some((inner, tail)) = split_balanced_parens(rest) else {
            break;
        };
        args.extend(split_top_level_commas(inner).into_iter().map(|s| s.trim().to_owned()).filter(|s| !s.is_empty()));
        rest = tail;
    }
    args
}

fn split_balanced_parens(src: &str) -> Option<(&str, &str)> {
    let mut depth: usize = 1;
    let mut in_string = false;
    let mut chars = src.char_indices();
    while let Some((i, c)) = chars.next() {
        if in_string {
            if c == '\\' {
                chars.next();
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => {
                depth -= 1;
                if depth == 0 {
                    return Some((&src[..i], &src[i + 1..]));
                }
            }
            _ => {}
        }
    }
    None
}

fn split_top_level_commas(inner: &str) -> Vec<String> {
    let mut args = Vec::new();
    let mut start = 0;
    let mut depth: usize = 0;
    let mut in_string = false;
    let mut chars = inner.char_indices();
    while let Some((i, c)) = chars.next() {
        if in_string {
            if c == '\\' {
                chars.next();
            } else if c == '"' {
                in_string = false;
            }
            continue;
        }
        match c {
            '"' => in_string = true,
            '(' => depth += 1,
            ')' => depth = depth.saturating_sub(1),
            ',' if depth == 0 => {
                args.push(inner[start..i].to_owned());
                start = i + 1;
            }
            _ => {}
        }
    }
    args.push(inner[start..].to_owned());
    args
}

fn partition(fixtures: Vec<Fixture>) -> Vec<Shard> {
    let mut fail = Vec::new();
    let mut pass = Vec::new();
    for fixture in fixtures {
        match fixture.class {
            Class::CompileFail => fail.push(fixture),
            Class::Pass => pass.push(fixture),
        }
    }
    fail.sort_by(|a, b| a.stem.cmp(&b.stem));
    let mut shards = vec![Shard { spec: COMPILE_FAIL_SPEC.to_owned(), class: Class::CompileFail, files: fail }];
    for (i, mut files) in bin_pack(pass, PASS_SHARD_COUNT).into_iter().enumerate() {
        files.sort_by(|a, b| a.stem.cmp(&b.stem));
        shards.push(Shard { spec: format!("pass-{i}"), class: Class::Pass, files });
    }
    shards
}

fn bin_pack(mut files: Vec<Fixture>, n: usize) -> Vec<Vec<Fixture>> {
    files.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.stem.cmp(&b.stem)));
    let mut bins: Vec<(u64, Vec<Fixture>)> = (0..n).map(|_| (0, Vec::new())).collect();
    for file in files {
        let idx = bins
            .iter()
            .enumerate()
            .min_by_key(|(i, (weight, _))| (*weight, *i))
            .map(|(i, _)| i)
            .expect("PASS_SHARD_COUNT > 0");
        bins[idx].0 += file.size;
        bins[idx].1.push(file);
    }
    bins.into_iter().map(|(_, files)| files).collect()
}

#[test]
fn the_discovered_set_is_the_three_hand_lists() {
    // Tripwire: the migration's whole risk is a discovery rule that quietly
    // enumerates a different set than the hand lists did; the pinned value is
    // computed from the directory, so it moves when a fixture is added — which
    // is the moment to update it deliberately, in one place, instead of
    // remembering a shard.
    let discovered: BTreeSet<String> = discover().into_iter().map(|f| f.stem).collect();
    let expected: BTreeSet<String> = HAND_LIST_FIXTURES.iter().copied().map(str::to_owned).collect();
    assert_eq!(discovered, expected);
}

#[test]
fn a_fixture_with_a_stderr_sibling_is_compile_fail() {
    let fixtures = discover();
    let class = |stem: &str| fixtures.iter().find(|f| f.stem == stem).map(|f| f.class);
    assert_eq!(class("rejects_missing_namespace_wasm"), Some(Class::CompileFail));
    assert_eq!(class("accepts_minimal_actor"), Some(Class::Pass));
    for stem in ["shard_support", "rt_ok", "rt_nohandler", "rt_nonamespace", "rt_ambiguous"] {
        assert_eq!(class(stem), None, "{stem} must not be compiled as a case");
    }
}

const HAND_LIST_FIXTURES: &[&str] = &[
    "accepts_actor_composable_wasm",
    "accepts_actor_lineage_wasm",
    "accepts_actor_runtime_feature",
    "accepts_actor_split_fallback",
    "accepts_actor_split_task_handler",
    "accepts_bare_type_address_of_embedded_peer",
    "accepts_cfg_gated_handler_native",
    "accepts_cfg_gated_handler_set_wasm",
    "accepts_cfg_gated_handler_wasm",
    "accepts_generic_local",
    "accepts_handler_set_wasm",
    "accepts_manual_handler_wasm",
    "accepts_minimal_actor",
    "accepts_multi_handler_native",
    "accepts_multi_handler_wasm",
    "accepts_state_actor",
    "accepts_struct_hosted_actor",
    "accepts_struct_nested_runtime",
    "rejects_accessor_without_state",
    "rejects_actor_child_of_cardinality_native",
    "rejects_actor_child_of_cardinality_wasm",
    "rejects_actor_composable_cardinality",
    "rejects_actor_composable_child_of",
    "rejects_actor_composable_native",
    "rejects_actor_root_wasm",
    "rejects_actor_unknown_arg",
    "rejects_bare_handler_native",
    "rejects_bare_handler_wasm",
    "rejects_bare_mail_variant_native",
    "rejects_duplicate_actor_lineage",
    "rejects_duplicate_handler_kind_native",
    "rejects_duplicate_handler_kind_wasm",
    "rejects_duplicate_native_init",
    "rejects_generic_native_lineage_impl",
    "rejects_generic_native_lineage_struct",
    "rejects_handler_set_duplicate_adoption",
    "rejects_handler_set_without_body",
    "rejects_malformed_actor_composable",
    "rejects_malformed_actor_lineage",
    "rejects_manual_marker_mismatch_wasm",
    "rejects_manual_task_handler_native",
    "rejects_missing_namespace_native",
    "rejects_missing_namespace_wasm",
    "rejects_missing_rehydrate",
    "rejects_multi_marker_mismatch_wasm",
    "rejects_multi_nonunit_return_native",
    "rejects_multi_task_handler_native",
    "rejects_nonself_handler_wasm",
    "rejects_slice_handler_wasm",
    "rejects_state_with_manual_hook",
    "rejects_stray_const_native",
    "rejects_stray_const_wasm",
    "rejects_struct_ambiguous_runtime",
    "rejects_struct_handler_set",
    "rejects_struct_missing_runtime",
    "rejects_struct_no_handler",
    "rejects_struct_no_namespace",
    "rejects_wasm_child_spawn_without_placement",
    "single_handler_cannot_reply",
];
