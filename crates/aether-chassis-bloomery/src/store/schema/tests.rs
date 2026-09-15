//! Snapshot tests pinning the store schema per version (issue 6023).
//!
//! Each failure names the three-step discipline a schema change needs: bump
//! `SCHEMA_VERSION`, add the next snapshot, write the guarded migration.

#![allow(clippy::unwrap_used)]

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;

use super::super::runtime::{SCHEMA_VERSION, SqliteStore};
use super::{
    normalized_ddl, parse_snapshot, render_snapshot, schema_dir, snapshot_digest, snapshot_filename, statement_sql,
};

/// What every failure message points at: the three steps a schema change
/// needs (bump, next snapshot, guarded migration).
const REMEDY: &str = "bump SCHEMA_VERSION (store/runtime.rs), add the next snapshot (src/store/schema/v*.sql) via the regeneration test, and write the guarded migration in migrate_schema";

/// The normalized DDL of a store opened at the current schema.
fn fresh_ddl() -> Vec<String> {
    let store = SqliteStore::open(":memory:").unwrap();
    normalized_ddl(&store.conn).unwrap()
}

/// Every checked-in snapshot file, sorted by filename.
fn snapshot_files() -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(schema_dir())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension().is_some_and(|extension| extension == "sql")
                && path.file_stem().is_some_and(|stem| stem.to_string_lossy().starts_with('v'))
        })
        .collect();
    files.sort();
    assert!(!files.is_empty(), "no schema snapshots beside the schema module: {REMEDY}");
    files
}

/// The version a snapshot filename claims (`v26.sql` claims 26).
fn filename_version(path: &Path) -> i64 {
    path.file_stem()
        .unwrap()
        .to_string_lossy()
        .strip_prefix('v')
        .unwrap()
        .parse()
        .unwrap_or_else(|_| panic!("snapshot {} is not named v<version>.sql", path.display()))
}

/// A snapshot file's parsed version and DDL, or a panic naming the file.
fn read_snapshot(path: &Path) -> (i64, Vec<String>) {
    let text = fs::read_to_string(path).unwrap();
    parse_snapshot(&text).unwrap_or_else(|| panic!("snapshot {} has no version header: {REMEDY}", path.display()))
}

/// Lines of `have` absent from `want`, for the drift report.
fn missing_lines<'a>(have: &'a [String], want: &[String]) -> Vec<&'a str> {
    have.iter().filter(|line| !want.contains(line)).map(String::as_str).collect()
}

/// A snapshot's `CREATE` statements in an order `SQLite` executes: tables
/// before the indexes and triggers that name them. The snapshot itself stays
/// sorted; only the rebuild reorders.
fn execution_order(ddl: &[String]) -> Vec<&str> {
    let mut tables = Vec::new();
    let mut rest = Vec::new();
    for line in ddl {
        let Some(sql) = statement_sql(line) else {
            continue;
        };
        if line.starts_with("table|") {
            tables.push(sql);
        } else {
            rest.push(sql);
        }
    }
    tables.append(&mut rest);
    tables
}

/// Rebuild a store from a frozen snapshot's DDL stamped at `version`, open
/// it under the current binary, and return its migrated DDL.
fn migrate_snapshot(path: &Path, version: i64, ddl: &[String]) -> Vec<String> {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("snapshot.db");
    {
        let conn = Connection::open(&db).unwrap();
        let mut batch = execution_order(ddl).join(";\n");
        batch.push_str(";\n");
        conn.execute_batch(&batch).unwrap();
        conn.pragma_update(None, "user_version", version).unwrap();
    }
    let store = SqliteStore::open(db.to_str().unwrap()).unwrap_or_else(|error| {
        panic!("a store built from {} (v{version}) refused to open under v{SCHEMA_VERSION}: {error}", path.display())
    });
    let stamped: i64 = store.conn.query_row("PRAGMA user_version", [], |row| row.get(0)).unwrap();
    assert_eq!(
        stamped,
        SCHEMA_VERSION,
        "migrating {} (v{version}) stamped {stamped}, not v{SCHEMA_VERSION}",
        path.display()
    );
    normalized_ddl(&store.conn).unwrap()
}

/// Record `filename`'s digest in `PINS`, keeping one line per snapshot
/// sorted by filename.
fn upsert_pin(filename: &str, digest: &str) {
    let path = schema_dir().join("PINS");
    let text = fs::read_to_string(&path).unwrap_or_default();
    let mut pins: Vec<(String, String)> = Vec::new();
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        if let (Some(old_digest), Some(old_name), None) = (parts.next(), parts.next(), parts.next())
            && old_name != filename
        {
            pins.push((old_name.to_owned(), old_digest.to_owned()));
        }
    }
    pins.push((filename.to_owned(), digest.to_owned()));
    pins.sort();
    let rendered = pins.iter().map(|(name, pin)| format!("{pin}  {name}")).collect::<Vec<_>>().join("\n") + "\n";
    fs::write(&path, rendered).unwrap();
}

#[test]
fn fresh_store_schema_matches_the_snapshot_for_schema_version() {
    // Tripwire for the 2026-09-15 boot failure (issue 6023): a column added
    // to a `CREATE TABLE` without a new snapshot fails here, before deploy.
    let ddl = fresh_ddl();
    let path = schema_dir().join(snapshot_filename(SCHEMA_VERSION));
    let text = fs::read_to_string(&path)
        .unwrap_or_else(|_| panic!("no snapshot for schema version {SCHEMA_VERSION} at {}: {REMEDY}", path.display()));
    let (version, pinned) =
        parse_snapshot(&text).unwrap_or_else(|| panic!("snapshot {} has no version header: {REMEDY}", path.display()));
    assert_eq!(
        version,
        SCHEMA_VERSION,
        "snapshot {} pins version {version}, not the current {SCHEMA_VERSION}: {REMEDY}",
        path.display()
    );
    let only_fresh = missing_lines(&ddl, &pinned);
    let only_pinned = missing_lines(&pinned, &ddl);
    assert!(
        only_fresh.is_empty() && only_pinned.is_empty(),
        "the fresh store's schema drifted from {} without a versioned snapshot: {REMEDY}\nonly in the fresh store:\n{}\nonly in the snapshot:\n{}",
        path.display(),
        only_fresh.join("\n"),
        only_pinned.join("\n")
    );
}

#[test]
fn every_frozen_snapshot_migrates_to_the_fresh_schema() {
    // A snapshot added without its guarded migration fails here: the store
    // rebuilt from the previous snapshot never gains the new shape, so its
    // DDL differs from a fresh store's.
    let fresh = fresh_ddl();
    for path in snapshot_files() {
        let (version, ddl) = read_snapshot(&path);
        assert_eq!(
            filename_version(&path),
            version,
            "snapshot {} names a different version than its v<version>.sql filename",
            path.display()
        );
        assert!(
            version <= SCHEMA_VERSION,
            "snapshot {} pins v{version}, past the current v{SCHEMA_VERSION}: {REMEDY}",
            path.display()
        );
        if version == SCHEMA_VERSION {
            continue;
        }
        let migrated = migrate_snapshot(&path, version, &ddl);
        let missing = missing_lines(&fresh, &migrated);
        let extra = missing_lines(&migrated, &fresh);
        assert!(
            missing.is_empty() && extra.is_empty(),
            "a store built from {} (v{version}) and opened under v{SCHEMA_VERSION} never reached the fresh schema: {REMEDY}\nmissing after migrate:\n{}\nunexpected after migrate:\n{}",
            path.display(),
            missing.join("\n"),
            extra.join("\n")
        );
    }
}

#[test]
fn frozen_snapshots_carry_their_pinned_digest() {
    // Editing a frozen snapshot in place trips this pin; so does a snapshot
    // with no pin and a pin with no snapshot.
    let dir = schema_dir();
    let pins = fs::read_to_string(dir.join("PINS"))
        .unwrap_or_else(|_| panic!("no PINS beside the schema snapshots at {}: {REMEDY}", dir.display()));
    let mut pinned: Vec<(String, String)> = Vec::new();
    for line in pins.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let mut parts = line.split_whitespace();
        let (Some(digest), Some(name), None) = (parts.next(), parts.next(), parts.next()) else {
            panic!("malformed PINS line {line:?}: each line is `<sha256>  <filename>`");
        };
        pinned.push((digest.to_owned(), name.to_owned()));
    }
    for (digest, name) in &pinned {
        let bytes = fs::read(dir.join(name)).unwrap_or_else(|_| {
            panic!("PINS names {name}, which is not beside the schema module: remove the pin or restore the snapshot")
        });
        assert_eq!(
            &snapshot_digest(&bytes),
            digest,
            "snapshot {name} no longer matches its pin: snapshots are frozen once committed — {REMEDY}"
        );
    }
    for path in snapshot_files() {
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(
            pinned.iter().any(|(_, pinned_name)| pinned_name == &name),
            "snapshot {name} has no pin in PINS: snapshots are frozen once committed — {REMEDY}"
        );
    }
}

/// Write the snapshot for the current schema version from a fresh store.
///
/// Run as `AETHER_WRITE_SCHEMA_SNAPSHOT=1 cargo test -p
/// aether-chassis-bloomery --lib schema -- --ignored`, after bumping
/// `SCHEMA_VERSION` and writing its guarded migration. Refuses to overwrite:
/// snapshots are frozen once committed.
#[test]
#[ignore = "writes src/store/schema/v<version>.sql; run with AETHER_WRITE_SCHEMA_SNAPSHOT=1"]
fn regenerate_the_schema_snapshot_for_the_current_version() {
    let write = env::vars_os()
        .find_map(|(key, value)| (key == "AETHER_WRITE_SCHEMA_SNAPSHOT").then_some(value))
        .and_then(|value| value.into_string().ok());
    assert_eq!(write.as_deref(), Some("1"), "the regeneration path only runs with AETHER_WRITE_SCHEMA_SNAPSHOT=1");
    let path = schema_dir().join(snapshot_filename(SCHEMA_VERSION));
    assert!(
        !path.exists(),
        "refusing to overwrite {}: snapshots are frozen once committed — bump SCHEMA_VERSION first",
        path.display()
    );
    let rendered = render_snapshot(SCHEMA_VERSION, &fresh_ddl());
    fs::write(&path, &rendered).unwrap();
    upsert_pin(&path.file_name().unwrap().to_string_lossy(), &snapshot_digest(rendered.as_bytes()));
}
