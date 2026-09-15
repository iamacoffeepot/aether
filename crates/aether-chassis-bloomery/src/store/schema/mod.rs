//! Frozen per-version snapshots of the store's `SQLite` schema (issue 6023).
//!
//! On 2026-09-15 the coordinator failed to boot on the live journal with
//! `no such column: model_override`, because issue 5945's guarded `ALTER`
//! shipped without a version bump and every store already stamped 25 skipped
//! it. Nothing in `cargo test` failed when the schema moved without a bump,
//! so this module pins the schema: one checked-in DDL snapshot per schema
//! version beside this file (`v26.sql`, `v27.sql`, ...), each the normalized
//! `sqlite_master` rows of a fresh store, plus a `PINS` file listing every
//! snapshot's sha256.
//!
//! The discipline is three steps for every schema change, and the tests name
//! all three when one is missed: bump `SCHEMA_VERSION` in `runtime.rs`, add
//! the next snapshot with the regeneration test, and write the guarded
//! migration in `migrate_schema`. Snapshots are frozen once committed —
//! editing one in place trips the pin test — and a new column always goes
//! last in its `CREATE TABLE`, because `ALTER TABLE ... ADD COLUMN` appends:
//! anything else leaves a migrated store's DDL unequal to a fresh store's
//! and trips the migration test.
//!
//! Test-only: the only consumers are the snapshot tests, so the module is
//! gated on `cfg(test)` at its declaration in `store/mod.rs`.

use std::path::PathBuf;

use aether_bloomery::Digest;
use rusqlite::Connection;

#[cfg(test)]
mod tests;

/// The first line of every snapshot file, followed by the version number.
const HEADER_PREFIX: &str = "-- aether store schema snapshot v";

/// The directory holding the frozen snapshots, beside this module.
fn schema_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/store/schema")
}

/// The snapshot filename for a schema version (`v26.sql` for version 26).
fn snapshot_filename(version: i64) -> String {
    format!("v{version}.sql")
}

/// Collapse every whitespace run in `sql` to one space.
///
/// The `CREATE` statements the migrations execute are indented for readers;
/// `sqlite_master` keeps that layout, so two identical shapes written months
/// apart would compare unequal without this.
fn collapse_whitespace(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The normalized DDL of the store behind `conn`: one `type|name|sql` line
/// per `sqlite_master` row, whitespace-collapsed and sorted.
///
/// `sqlite_sequence` (the `AUTOINCREMENT` bookkeeping table) and the
/// `sqlite_autoindex_*` rows (which carry no `sql`) are not schema the
/// migrations own, so they are skipped. Everything else — tables, indexes,
/// triggers — is pinned.
fn normalized_ddl(conn: &Connection) -> rusqlite::Result<Vec<String>> {
    let mut statement = conn.prepare("SELECT type, name, sql FROM sqlite_master")?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?, row.get::<_, Option<String>>(2)?))
    })?;
    let mut ddl = Vec::new();
    for row in rows {
        let (kind, name, sql) = row?;
        let Some(sql) = sql else {
            continue;
        };
        if name == "sqlite_sequence" || name.starts_with("sqlite_autoindex_") {
            continue;
        }
        let collapsed = collapse_whitespace(&sql);
        ddl.push(format!("{kind}|{name}|{collapsed}"));
    }
    ddl.sort();
    Ok(ddl)
}

/// Render a snapshot file's bytes for `version` over `ddl`.
fn render_snapshot(version: i64, ddl: &[String]) -> String {
    let mut out = format!("{HEADER_PREFIX}{version}\n");
    for line in ddl {
        out.push_str(line);
        out.push('\n');
    }
    out
}

/// Parse a snapshot header line into its version.
fn parse_header_version(header: &str) -> Option<i64> {
    header.strip_prefix(HEADER_PREFIX)?.parse().ok()
}

/// Parse snapshot bytes into their version and DDL lines.
fn parse_snapshot(text: &str) -> Option<(i64, Vec<String>)> {
    let mut lines = text.lines();
    let version = lines.next().and_then(parse_header_version)?;
    let ddl = lines.filter(|line| !line.trim().is_empty()).map(str::to_owned).collect();
    Some((version, ddl))
}

/// The `CREATE` statement a snapshot line carries: the text after the second
/// `|` separator. `None` for a line that is not a snapshot row.
fn statement_sql(line: &str) -> Option<&str> {
    line.splitn(3, '|').nth(2)
}

/// sha256 over snapshot file bytes as lowercase hex, via the workspace's
/// content-addressing primitive. Hashing the exact file bytes (header
/// included) is what makes editing a frozen snapshot trip the pin.
fn snapshot_digest(bytes: &[u8]) -> String {
    Digest::of_wire_bytes(bytes).to_hex()
}
