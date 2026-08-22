//! What the fixtures command decides about on-disk bytes.

#![allow(clippy::unwrap_used)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::{env, process};

use super::{FIXTURES, FIXTURES_DIR, check_in, regen_in};

fn scratch(tag: &str) -> PathBuf {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let seq = SEQ.fetch_add(1, Ordering::Relaxed);
    let dir = env::temp_dir().join(format!("aether-xtask-fixtures-{tag}-{}-{seq}", process::id()));
    fs::create_dir_all(&dir).expect("create scratch dir");
    dir
}

fn checked_in_fixtures() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("..").join(FIXTURES_DIR)
}

fn copy_fixtures_into(root: &Path) -> PathBuf {
    let dest = root.join(FIXTURES_DIR);
    fs::create_dir_all(&dest).unwrap();
    for fixture in FIXTURES {
        fs::copy(checked_in_fixtures().join(fixture.file), dest.join(fixture.file)).unwrap();
    }
    dest
}

fn snapshot(dir: &Path) -> Vec<(String, Vec<u8>)> {
    let mut files: Vec<(String, Vec<u8>)> = fs::read_dir(dir)
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name().to_string_lossy().into_owned(), fs::read(entry.path()).unwrap())
        })
        .collect();
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}

#[test]
fn check_reports_a_corrupted_fixture_as_stale() {
    // A fixture that has drifted from its encoder used to be invisible short of
    // a full test build. `check` is the way to learn so without writing.
    let root = scratch("stale");
    let dest = copy_fixtures_into(&root);
    let target = dest.join("decisions.bin");
    let mut bytes = fs::read(&target).unwrap();
    bytes[0] ^= 0xff;
    fs::write(&target, &bytes).unwrap();
    let before = snapshot(&dest);

    let stale = check_in(&root).unwrap();

    assert_eq!(stale, ["decisions"]);
    assert_eq!(snapshot(&dest), before, "check writes nothing");
}

#[test]
fn regen_is_idempotent_on_a_clean_tree() {
    // Tripwire: the command must reproduce exactly the bytes the four
    // inline consts pinned, so the move from source to file re-pins nothing.
    let root = scratch("idempotent");
    copy_fixtures_into(&root);

    regen_in(&root, None).unwrap();

    let source = checked_in_fixtures();
    for fixture in FIXTURES {
        let written = fs::read(root.join(FIXTURES_DIR).join(fixture.file)).unwrap();
        let pinned = fs::read(source.join(fixture.file)).unwrap();
        let name = fixture.name;
        assert_eq!(written, pinned, "{name} drifted from the checked-in fixture");
    }
}
