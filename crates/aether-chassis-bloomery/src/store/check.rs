//! Read-only store proof (#5500): decode every journal row through the
//! persisted registry, the way boot replay will, without mounting the chassis
//! or acting on the state.
//!
//! `bloomery --check-store` runs this so a candidate binary is proven against
//! a *copy* of the live journal before any restart of the real unit — the
//! decode refusal that would otherwise fatal-abort boot surfaces here as a
//! named report line and exit `1` instead. Opening the store runs pending
//! migrations, exactly as boot would, which is why the input must be a copy.
//! This is a decode proof over the boot-fatal surface, not a full boot: config
//! rows decode lazily at their point of use and are only counted here.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use aether_bloomery::encode_hex;
use aether_bloomery::persisted::{decode_recorded_decisions, decode_recorded_event};

use super::{SqliteStore, StoreBackend as _};

/// What `--check-store` found: per-stamp row tallies and every decode refusal.
#[derive(Debug, Default)]
pub struct StoreCheck {
    /// Journal rows examined.
    pub journal_rows: usize,
    /// Config rows present (counted, not decoded — they resolve lazily).
    pub config_rows: usize,
    /// Rows per recorded event stamp (`"absent"` for pre-column rows).
    pub event_stamps: BTreeMap<String, usize>,
    /// Rows per recorded decisions stamp (`"absent"` for pre-column rows).
    pub decisions_stamps: BTreeMap<String, usize>,
    /// One line per row that refused to decode, in journal order.
    pub refusals: Vec<String>,
}

impl StoreCheck {
    /// No row refused to decode.
    #[must_use]
    pub fn is_clean(&self) -> bool {
        self.refusals.is_empty()
    }

    /// The human report `--check-store` prints.
    #[must_use]
    pub fn render(&self) -> String {
        let mut out = String::new();
        let _ = writeln!(out, "journal rows: {}", self.journal_rows);
        let _ = writeln!(out, "config rows:  {}", self.config_rows);
        for (stamp, count) in &self.event_stamps {
            let _ = writeln!(out, "event     {stamp}  x{count}");
        }
        for (stamp, count) in &self.decisions_stamps {
            let _ = writeln!(out, "decisions {stamp}  x{count}");
        }
        for refusal in &self.refusals {
            let _ = writeln!(out, "REFUSED: {refusal}");
        }
        let _ = writeln!(
            out,
            "{}",
            if self.is_clean() {
                "store check: CLEAN"
            } else {
                "store check: REFUSED"
            }
        );
        out
    }
}

/// Open `path` (running pending migrations, as boot would) and decode every
/// journal row through the persisted registry.
///
/// # Errors
///
/// The database-level error when the store cannot be opened or read at all;
/// per-row decode refusals are report lines, not errors.
pub fn check_store(path: &str) -> rusqlite::Result<StoreCheck> {
    let mut store = SqliteStore::open(path)?;
    let mut check = StoreCheck { config_rows: store.load_configs()?.len(), ..StoreCheck::default() };
    for record in store.replay_journal()? {
        check.journal_rows += 1;
        let stamp = |digest: Option<&[u8]>| digest.map_or_else(|| String::from("absent"), encode_hex);
        *check.event_stamps.entry(stamp(record.event_schema.as_deref())).or_default() += 1;
        *check.decisions_stamps.entry(stamp(record.decisions_schema_digest.as_deref())).or_default() += 1;
        if let Err(error) = decode_recorded_event(&record.event, record.event_schema.as_deref()) {
            check.refusals.push(format!("record {} ({}): {error}", record.sequence, record.idempotency_key));
        }
        if let Err(error) = decode_recorded_decisions(&record.decisions, record.decisions_schema_digest.as_deref()) {
            check.refusals.push(format!("record {} ({}): {error}", record.sequence, record.idempotency_key));
        }
    }
    Ok(check)
}
