//! Session-reuse for the construct/verify/refine retry loop (#4902).
//!
//! The pool already owns eligibility (age, context cap, `head_hash`, lease).
//! This module is the missing consumer: it decides whether a lap should try
//! to resume, acquires from the pool, guards the slot-path cwd gotcha, and
//! stamps the decision next to the result record's actuals so the reuse rate
//! is auditable from the evidence the journal already keeps.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

use aether_bloomery::{Digest, Harness, PriceRates, PriceTable};

use crate::session::{
    AcquireMiss, AcquireOutcome, LeaseToken, LeasedSession, SessionBackend, SessionConfig, SessionKey, SessionManifest,
    SqliteSessionStore,
};

/// Seed prior: tokens of cold static-prefix re-derivation (Ŵ).
const COLD_PREFIX_TOKENS: u64 = 80_000;
/// Seed prior: tokens of one predicted repair turn (v̂).
const TURN_TOKENS: u64 = 4_500;
/// Seed prior: predicted repair turns for a named-site fix (n̂ midpoint).
const SEED_NAMED_SITE_TURNS: u64 = 15;
/// Seed prior: extra turns of going cold versus resume (Δn̂).
const SEED_EXTRA_COLD_TURNS: u64 = 5;

/// Which way a lap actually went.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReuseArm {
    /// The harness launched without a resume handle.
    Fresh,
    /// The harness was handed a pooled session id.
    Resumed,
}

impl ReuseArm {
    /// The evidence token the work order names.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Fresh => "fresh",
            Self::Resumed => "resumed",
        }
    }
}

/// Why a lap that could have resumed launched fresh instead.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissReason {
    /// No pooled session exists for the key.
    ColdKey,
    /// The pooled session is older than the prompt-cache TTL.
    Age,
    /// The pooled session's terminal context exceeds the cap.
    ContextCap,
    /// The static-prefix head moved since deposit.
    HeadHash,
    /// Grok's resume story is unprobed — a grok-keyed acquire always misses.
    Grok,
    /// The pooled session was deposited from a different lane slot.
    SlotMismatch,
}

impl MissReason {
    /// The evidence token the work order names.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ColdKey => "cold_key",
            Self::Age => "age",
            Self::ContextCap => "context_cap",
            Self::HeadHash => "head_hash",
            Self::Grok => "grok",
            Self::SlotMismatch => "slot_mismatch",
        }
    }
}

/// The acquire decision remembered on a run so `stream_evidence` can stamp it
/// beside the result record's actuals and deposit the session that came back.
#[derive(Debug, Clone)]
pub struct ReusePlan {
    /// The arm the inequality / seed chose before talking to the pool.
    pub predicted_arm: ReuseArm,
    /// Predicted repair turns (n̂) that went into the inequality.
    pub predicted_turns: u64,
    /// The arm the lap actually took.
    pub arm: ReuseArm,
    /// Why a predicted resume launched fresh, when it did.
    pub miss: Option<MissReason>,
    /// The Claude session id to thread as `--resume`, when resuming.
    pub resume: Option<String>,
    /// The lease to echo on deposit, when a resume leased a row.
    pub lease: Option<LeaseToken>,
    /// The pool key this lap is bound to.
    pub key: SessionKey,
    /// The static-prefix hash this lap acquired (and will deposit) under.
    pub head_hash: String,
    /// Canonical slot path this lap builds in — the cwd guard.
    pub slot_path: String,
}

/// The executor-side consumer of the session pool.
pub struct SessionReuse {
    pool: Mutex<Box<dyn SessionBackend>>,
    prices: PriceTable,
    now: Mutex<Option<u64>>,
    head_hash: Mutex<Option<String>>,
    state: Mutex<ReuseState>,
}

#[derive(Default)]
struct ReuseState {
    /// How many times this key has been deposited — the retry-index seed.
    deposits: HashMap<SessionKey, u32>,
    /// Terminal context T of the last deposit, used as the inequality's T.
    last_context: HashMap<SessionKey, u64>,
    /// Slot path the last deposit for this key came from.
    slot: HashMap<SessionKey, String>,
}

impl SessionReuse {
    /// Build over an explicit pool and the bloom's sealed price table.
    #[must_use]
    pub fn new(pool: Box<dyn SessionBackend>, prices: PriceTable) -> Self {
        Self {
            pool: Mutex::new(pool),
            prices,
            now: Mutex::new(None),
            head_hash: Mutex::new(None),
            state: Mutex::new(ReuseState::default()),
        }
    }

    /// An in-memory pool with the capability's default eligibility knobs.
    ///
    /// Isolated (`:memory:`), so tests do not share a table with the mounted
    /// capability or with each other. Production opens through
    /// [`from_config`](Self::from_config).
    #[must_use]
    pub fn memory(prices: PriceTable) -> Self {
        let defaults = SessionConfig::default();
        let pool = SqliteSessionStore::open(
            ":memory:",
            defaults.cache_ttl_cutoff_mins.saturating_mul(60),
            defaults.lease_ttl_mins.saturating_mul(60),
            defaults.context_cap_tokens,
        )
        .expect("an in-memory session pool opens");
        Self::new(Box::new(pool), prices)
    }

    /// Open the same pool the mounted [`SessionPoolCapability`](crate::session::SessionPoolCapability)
    /// opened: the operator's `SessionConfig` path and eligibility knobs, not
    /// literals. The acquire inequality still needs the bloom's sealed rates,
    /// which ride each [`AcquireRequest`] rather than this constructor.
    pub fn from_config(session: &SessionConfig) -> rusqlite::Result<Self> {
        let pool = SqliteSessionStore::open(
            session.store_path(),
            session.cache_ttl_cutoff_mins.saturating_mul(60),
            session.lease_ttl_mins.saturating_mul(60),
            session.context_cap_tokens,
        )?;
        Ok(Self::new(Box::new(pool), PriceTable::default()))
    }

    /// Pin the clock the next acquire/deposit stamps — tests only.
    pub fn set_now(&self, now: u64) {
        *lock(&self.now) = Some(now);
    }

    /// Pin the static-prefix hash acquire/deposit use — tests only.
    pub fn set_head_hash(&self, hash: impl Into<String>) {
        *lock(&self.head_hash) = Some(hash.into());
    }

    /// Seed a deposited session so a later acquire can hit or miss it.
    pub fn seed(&self, key: &SessionKey, session_id: &str, manifest: &SessionManifest, slot_path: &str) {
        let _ = lock(&self.pool).release(key, None, session_id, manifest);
        let mut state = lock(&self.state);
        *state.deposits.entry(key.clone()).or_insert(0) += 1;
        state.last_context.insert(key.clone(), manifest.context_tokens);
        state.slot.insert(key.clone(), slot_path.to_owned());
    }

    /// Decide whether this lap resumes, acquire from the pool, and return the
    /// plan the spawn threads and the evidence stamps.
    #[must_use]
    pub fn acquire(&self, request: &AcquireRequest<'_>) -> ReusePlan {
        let key = SessionKey {
            model: request.model.to_owned(),
            effort: request.effort.to_owned(),
            task: request.task.to_owned(),
        };
        let prices = request.prices.unwrap_or(&self.prices);
        let head_hash = self.head_hash_for(request.worktree);
        let slot_path = canonical_slot(request.worktree);

        if request.harness == Some(Harness::Grok) {
            // Stated, not silent: Grok's resume story is unprobed and its free
            // cache writes shrink the win.
            return ReusePlan {
                predicted_arm: ReuseArm::Fresh,
                predicted_turns: SEED_NAMED_SITE_TURNS,
                arm: ReuseArm::Fresh,
                miss: Some(MissReason::Grok),
                resume: None,
                lease: None,
                key,
                head_hash,
                slot_path,
            };
        }

        let (deposits, last_context) = {
            let state = lock(&self.state);
            (state.deposits.get(&key).copied().unwrap_or(0), state.last_context.get(&key).copied())
        };
        let predicted_turns = SEED_NAMED_SITE_TURNS;
        let predicted_arm = Self::predicted_arm(prices, request.model, deposits, last_context, predicted_turns);

        if predicted_arm == ReuseArm::Fresh {
            return ReusePlan {
                predicted_arm,
                predicted_turns,
                arm: ReuseArm::Fresh,
                miss: None,
                resume: None,
                lease: None,
                key,
                head_hash,
                slot_path,
            };
        }

        let now = self.now();
        let outcome = {
            let mut pool = lock(&self.pool);
            pool.acquire_explained(&key, &head_hash, now).unwrap_or(AcquireOutcome::Missed(AcquireMiss::ColdKey))
        };

        match outcome {
            AcquireOutcome::Leased(leased) => {
                if let Some(miss) = self.slot_guard(&key, &slot_path, &leased) {
                    return ReusePlan {
                        predicted_arm,
                        predicted_turns,
                        arm: ReuseArm::Fresh,
                        miss: Some(miss),
                        resume: None,
                        lease: None,
                        key,
                        head_hash,
                        slot_path,
                    };
                }
                ReusePlan {
                    predicted_arm,
                    predicted_turns,
                    arm: ReuseArm::Resumed,
                    miss: None,
                    resume: Some(leased.session_bytes),
                    lease: Some(leased.lease),
                    key,
                    head_hash,
                    slot_path,
                }
            }
            AcquireOutcome::Missed(miss) => ReusePlan {
                predicted_arm,
                predicted_turns,
                arm: ReuseArm::Fresh,
                miss: Some(map_pool_miss(miss)),
                resume: None,
                lease: None,
                key,
                head_hash,
                slot_path,
            },
        }
    }

    /// Deposit the session the attempt produced, so the next lap can judge it.
    pub fn deposit(&self, plan: &ReusePlan, session_id: &str, context_tokens: u64) {
        let now = self.now();
        let manifest = SessionManifest {
            parent_receipt: None,
            receipt: session_id.to_owned(),
            head_hash: plan.head_hash.clone(),
            context_tokens,
            workspace_tree_hash: String::new(),
            read_files: vec![plan.slot_path.clone()],
            deposited_at: now,
        };
        let _ = lock(&self.pool).release(&plan.key, plan.lease.as_ref(), session_id, &manifest);
        let mut state = lock(&self.state);
        *state.deposits.entry(plan.key.clone()).or_insert(0) += 1;
        state.last_context.insert(plan.key.clone(), context_tokens);
        state.slot.insert(plan.key.clone(), plan.slot_path.clone());
    }

    fn predicted_arm(
        prices: &PriceTable,
        model: &str,
        deposits: u32,
        last_context: Option<u64>,
        n_hat: u64,
    ) -> ReuseArm {
        // Retry-index seed: resume on lap 2, cold on lap 3+ — a session that
        // failed twice is carrying a wrong theory. Yields to the inequality
        // once a prior lap left a T and the sealed table prices this model.
        if deposits == 0 {
            return ReuseArm::Fresh;
        }
        if deposits >= 2 {
            return ReuseArm::Fresh;
        }
        let Some(t_tokens) = last_context else {
            return ReuseArm::Resumed;
        };
        let Some(row) = prices.row(model) else {
            return ReuseArm::Resumed;
        };
        match resume_is_cheaper(row, t_tokens, n_hat) {
            Some(false) => ReuseArm::Fresh,
            Some(true) | None => ReuseArm::Resumed,
        }
    }

    fn slot_guard(&self, key: &SessionKey, slot_path: &str, leased: &LeasedSession) -> Option<MissReason> {
        let (deposited, context) = {
            let state = lock(&self.state);
            (state.slot.get(key).cloned(), state.last_context.get(key).copied().unwrap_or(0))
        };
        let deposited = deposited?;
        if deposited == slot_path {
            return None;
        }
        // Put the session back — we leased it only to discover it belongs to
        // another slot, and holding the lease would wedge that slot's next lap.
        let now = self.now();
        let head_hash = lock(&self.head_hash).clone().unwrap_or_default();
        let manifest = SessionManifest {
            parent_receipt: Some(leased.parent_receipt.clone()),
            receipt: leased.parent_receipt.clone(),
            head_hash,
            context_tokens: context,
            workspace_tree_hash: String::new(),
            read_files: vec![deposited],
            deposited_at: now,
        };
        let _ = lock(&self.pool).release(key, Some(&leased.lease), &leased.session_bytes, &manifest);
        Some(MissReason::SlotMismatch)
    }

    fn head_hash_for(&self, worktree: &Path) -> String {
        lock(&self.head_hash).clone().unwrap_or_else(|| static_prefix_hash(worktree))
    }

    fn now(&self) -> u64 {
        lock(&self.now).unwrap_or_else(|| SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs()))
    }
}

/// What one acquire needs from the dispatch.
pub struct AcquireRequest<'a> {
    /// The resolved harness, when the order named one.
    pub harness: Option<Harness>,
    /// The resolved model id.
    pub model: &'a str,
    /// The resolved effort tier.
    pub effort: &'a str,
    /// The pool task axis — command plus work-order description, so a critic
    /// cannot resume the constructor that shares its model, effort, and text.
    pub task: &'a str,
    /// The slot checkout this lap builds in.
    pub worktree: &'a Path,
    /// The bloom's sealed price table, when the host resolved one. `None`
    /// falls back to the table this [`SessionReuse`] was built with (tests).
    pub prices: Option<&'a PriceTable>,
}

/// The pool `task` axis: the lane command, then the work-order description.
///
/// Command first so `construct.implement` and `review.critic` never share a
/// session even when they carry the same description; the description keeps
/// cross-work-order reuse out. A description-less order keys on the command
/// alone.
#[must_use]
pub fn pool_task(command: &str, description: Option<&str>) -> String {
    description.filter(|task| !task.is_empty()).map_or_else(|| command.to_owned(), |task| format!("{command}\n{task}"))
}

/// Resume iff `n̂·T < (w/r)·Ŵ + 0.5·n̂·Ŵ + (o/r)·Δn̂·v̂`.
///
/// Evaluated in integers as `2r` times both sides so the rates stay in the
/// sealed columns and no price literal enters the predicate. `None` when the
/// row cannot evaluate (a zero cache-read rate would divide by zero).
pub fn resume_is_cheaper(row: &PriceRates, t_tokens: u64, n_hat: u64) -> Option<bool> {
    let read = row.cache_read;
    if read == 0 {
        return None;
    }
    let write = if row.cache_write_1h > 0 {
        row.cache_write_1h
    } else if row.cache_write > 0 {
        row.cache_write
    } else {
        row.cache_write_5m
    };
    // 2r · n̂ · T  <  2w · Ŵ  +  r · n̂ · Ŵ  +  2o · Δn̂ · v̂
    let left = read.saturating_mul(n_hat).saturating_mul(t_tokens).saturating_mul(2);
    let right = write
        .saturating_mul(COLD_PREFIX_TOKENS)
        .saturating_mul(2)
        .saturating_add(read.saturating_mul(n_hat).saturating_mul(COLD_PREFIX_TOKENS))
        .saturating_add(row.output.saturating_mul(SEED_EXTRA_COLD_TURNS).saturating_mul(TURN_TOKENS).saturating_mul(2));
    Some(left < right)
}

fn map_pool_miss(miss: AcquireMiss) -> MissReason {
    match miss {
        AcquireMiss::ColdKey | AcquireMiss::Leased => MissReason::ColdKey,
        AcquireMiss::Age => MissReason::Age,
        AcquireMiss::ContextCap => MissReason::ContextCap,
        AcquireMiss::HeadHash => MissReason::HeadHash,
    }
}

fn canonical_slot(worktree: &Path) -> String {
    worktree.canonicalize().unwrap_or_else(|_| worktree.to_path_buf()).to_string_lossy().into_owned()
}

/// sha256 of the static prefix a resume reuses (`CLAUDE.md` + the construct
/// instruction source). A checkout that has neither — the stub runner's
/// empty slot — uses a stable sentinel so a two-lap fixture still matches.
pub fn static_prefix_hash(worktree: &Path) -> String {
    let mut prefix = Vec::new();
    for rel in ["CLAUDE.md", "xtask/src/transform/construct_instructions.md"] {
        if let Ok(bytes) = fs::read(worktree.join(rel)) {
            prefix.extend_from_slice(&bytes);
        }
    }
    if prefix.is_empty() {
        return String::from("static-prefix");
    }
    hex_bytes(Digest::of_wire_bytes(&prefix).as_bytes())
}

fn hex_bytes(bytes: &[u8]) -> String {
    bytes.iter().fold(String::with_capacity(bytes.len() * 2), |mut out, byte| {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
        out
    })
}

/// Stamp the acquire plan (and the result record's actual turns) onto the
/// evidence envelope so the reuse rate is auditable from the journaled bytes.
#[must_use]
pub fn stamp_reuse(bytes: &[u8], plan: &ReusePlan, actual_turns: Option<u64>) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    let Some(object) = value.as_object_mut() else {
        return bytes.to_vec();
    };
    object.insert(
        "session_reuse".to_owned(),
        serde_json::json!({
            "arm": plan.arm.as_str(),
            "miss": plan.miss.map(MissReason::as_str),
            "predicted_arm": plan.predicted_arm.as_str(),
            "predicted_turns": plan.predicted_turns,
            "actual_turns": actual_turns,
        }),
    );
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| bytes.to_vec())
}

/// The Claude session id the result record carried, when it carried one.
#[must_use]
pub fn parse_session_id(bytes: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let record = value.get("result_record")?;
    record
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .or_else(|| {
            record.get("result").and_then(|result| result.get("session_id")).and_then(serde_json::Value::as_str)
        })
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Terminal context T: uncached input plus both cache classes, the prompt the
/// next resume would re-read.
#[must_use]
pub fn parse_context_tokens(bytes: &[u8]) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let record = value.get("result_record")?;
    let number = |key| record.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    let total = number("input").saturating_add(number("cache_read")).saturating_add(number("cache_write"));
    (total > 0).then_some(total)
}

/// `num_turns` from the nested result record, when present.
#[must_use]
pub fn parse_actual_turns(bytes: &[u8]) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    value.get("result_record")?.get("num_turns").and_then(serde_json::Value::as_u64)
}

#[cfg(test)]
mod tests {
    use super::{
        COLD_PREFIX_TOKENS, MissReason, ReuseArm, SEED_NAMED_SITE_TURNS, SessionReuse, resume_is_cheaper, stamp_reuse,
    };
    use crate::session::{SessionKey, SessionManifest};
    use aether_bloomery::{Harness, PriceRates, PriceTable};
    use std::path::Path;

    fn row(cache_read: u64, cache_write: u64, output: u64) -> PriceRates {
        PriceRates {
            input: 5_000_000,
            cache_read,
            cache_write_5m: cache_write,
            cache_write_1h: cache_write,
            cache_write,
            output,
            long_context: None,
        }
    }

    fn key() -> SessionKey {
        SessionKey { model: "claude-opus-5".to_owned(), effort: "high".to_owned(), task: "issue-4902".to_owned() }
    }

    fn manifest(head: &str, context: u64, at: u64) -> SessionManifest {
        SessionManifest {
            parent_receipt: None,
            receipt: "receipt".to_owned(),
            head_hash: head.to_owned(),
            context_tokens: context,
            workspace_tree_hash: String::new(),
            read_files: Vec::new(),
            deposited_at: at,
        }
    }

    #[test]
    fn the_inequality_reads_rates_from_the_row() {
        // Tripwire: resume vs cold is a function of the sealed columns. A
        // huge prior context T against cheap cache-reads still resumes; the
        // same T against a write that is not much dearer than a read goes
        // cold. No rate literals live in the predicate — flipping the row
        // is what flips the arm.
        let cheap_read = row(500_000, 10_000_000, 25_000_000);
        assert_eq!(resume_is_cheaper(&cheap_read, 8_000, SEED_NAMED_SITE_TURNS), Some(true));

        let even_rates = row(1_000_000, 1_000_000, 1_000_000);
        assert_eq!(resume_is_cheaper(&even_rates, COLD_PREFIX_TOKENS, SEED_NAMED_SITE_TURNS), Some(false));

        let unreadable = row(0, 10_000_000, 25_000_000);
        assert_eq!(resume_is_cheaper(&unreadable, 8_000, SEED_NAMED_SITE_TURNS), None);
    }

    #[test]
    fn grok_always_misses_by_name() {
        let reuse = SessionReuse::memory(PriceTable::default());
        reuse.set_head_hash("head-A");
        let plan = reuse.acquire(&super::AcquireRequest {
            harness: Some(Harness::Grok),
            model: "grok-4.6",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-0"),
            prices: None,
        });
        assert_eq!(plan.arm, ReuseArm::Fresh);
        assert_eq!(plan.miss, Some(MissReason::Grok));
        assert!(plan.resume.is_none());
    }

    #[test]
    fn a_second_lap_acquires_the_deposited_session() {
        let reuse = SessionReuse::memory(PriceTable::default());
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let plan = reuse.acquire(&super::AcquireRequest {
            harness: Some(Harness::Claude),
            model: "claude-opus-5",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-0"),
            prices: None,
        });
        assert_eq!(plan.arm, ReuseArm::Resumed, "lap 2 resumes the deposited session");
        assert_eq!(plan.resume.as_deref(), Some("sess-1"));
        assert_eq!(plan.predicted_arm, ReuseArm::Resumed);
        assert_eq!(plan.predicted_turns, SEED_NAMED_SITE_TURNS);
    }

    #[test]
    fn each_named_miss_falls_back_to_fresh() {
        let reuse = SessionReuse::memory(PriceTable::default());
        reuse.set_now(1_000);

        reuse.set_head_hash("head-A");
        let cold = reuse.acquire(&super::AcquireRequest {
            harness: Some(Harness::Claude),
            model: "claude-opus-5",
            effort: "high",
            task: "missing",
            worktree: Path::new("/slot-0"),
            prices: None,
        });
        assert_eq!(cold.miss, None, "lap 1 is a seed-cold, not a pool miss");
        assert_eq!(cold.arm, ReuseArm::Fresh);

        reuse.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");

        reuse.set_head_hash("head-MOVED");
        let head = reuse.acquire(&super::AcquireRequest {
            harness: Some(Harness::Claude),
            model: "claude-opus-5",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-0"),
            prices: None,
        });
        assert_eq!(head.miss, Some(MissReason::HeadHash));
        assert!(head.resume.is_none());

        reuse.set_head_hash("head-A");
        reuse.set_now(1_000 + 55 * 60);
        let aged = reuse.acquire(&super::AcquireRequest {
            harness: Some(Harness::Claude),
            model: "claude-opus-5",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-0"),
            prices: None,
        });
        assert_eq!(aged.miss, Some(MissReason::Age));

        let capped = SessionReuse::memory(PriceTable::default());
        capped.set_head_hash("head-A");
        capped.set_now(1_000);
        capped.seed(&key(), "sess-1", &manifest("head-A", 150_001, 1_000), "/slot-0");
        let over = capped.acquire(&super::AcquireRequest {
            harness: Some(Harness::Claude),
            model: "claude-opus-5",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-0"),
            prices: None,
        });
        assert_eq!(over.miss, Some(MissReason::ContextCap));

        let slotted = SessionReuse::memory(PriceTable::default());
        slotted.set_head_hash("head-A");
        slotted.set_now(1_000);
        slotted.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");
        let mismatch = slotted.acquire(&super::AcquireRequest {
            harness: Some(Harness::Claude),
            model: "claude-opus-5",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-1"),
            prices: None,
        });
        assert_eq!(mismatch.miss, Some(MissReason::SlotMismatch));
        assert!(mismatch.resume.is_none());
    }

    #[test]
    fn stamp_reuse_sits_beside_the_result_record() {
        let plan = reuse_plan_for_stamp();
        let stamped =
            stamp_reuse(br#"{"command":"construct.implement","result_record":{"num_turns":12}}"#, &plan, Some(12));
        let value: serde_json::Value =
            serde_json::from_slice(&stamped).expect("stamp_reuse emits JSON beside the result record");
        assert_eq!(value["session_reuse"]["arm"], "resumed");
        assert_eq!(value["session_reuse"]["predicted_turns"], 15);
        assert_eq!(value["session_reuse"]["actual_turns"], 12);
        assert_eq!(value["result_record"]["num_turns"], 12, "the actuals stay on the record");
    }

    fn reuse_plan_for_stamp() -> super::ReusePlan {
        super::ReusePlan {
            predicted_arm: ReuseArm::Resumed,
            predicted_turns: 15,
            arm: ReuseArm::Resumed,
            miss: None,
            resume: Some("sess-1".to_owned()),
            lease: None,
            key: key(),
            head_hash: "head-A".to_owned(),
            slot_path: "/slot-0".to_owned(),
        }
    }
}
