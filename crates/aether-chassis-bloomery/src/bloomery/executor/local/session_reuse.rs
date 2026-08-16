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

use aether_bloomery::{Digest, PriceRates, PriceTable, REVIEW_CRITIC_COMMAND, StudyCost};

use crate::session::{
    AcquireMiss, AcquireOutcome, LeaseToken, LeasedSession, SessionBackend, SessionConfig, SessionKey, SessionManifest,
    SqliteSessionStore,
};

/// What a resumed dependent construct must be told: the tree is the splice, not
/// the files the predecessor session last edited on the bloom base.
pub const SPLICED_RESET_NOTE: &str = "The working tree was reset to the spliced dependency candidate; do not assume files from the predecessor session are still at the bloom base.";

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
    /// The harness session id to thread as `--resume`, when resuming.
    pub resume: Option<String>,
    /// The lease to echo on deposit, when a resume leased a row.
    pub lease: Option<LeaseToken>,
    /// The pool key this lap is bound to.
    pub key: SessionKey,
    /// The static-prefix hash this lap acquired (and will deposit) under.
    pub head_hash: String,
    /// Canonical slot path this lap builds in — the cwd guard.
    pub slot_path: String,
    /// Whether this lap resumed a predecessor's session along an edge rather
    /// than its own retry-lap key.
    pub edge: bool,
    /// Whether this lap is a builder seat (construct / refine / reconcile).
    /// Judge seats never update the slot's last-builder handle.
    pub is_builder: bool,
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
    /// Last builder key deposited in this slot path — the edge-affinity handle.
    builder_at_slot: HashMap<String, SessionKey>,
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
    ///
    /// Rebuilds the hot routing index from complete durable rows before the
    /// first acquire. A legacy or malformed row stays in the pool (eligibility
    /// and lease are unchanged) but is omitted here, so the next lap fails cold
    /// rather than resuming on invented deposit/slot/builder state.
    pub fn from_config(session: &SessionConfig) -> rusqlite::Result<Self> {
        let pool = SqliteSessionStore::open(
            session.store_path(),
            session.cache_ttl_cutoff_mins.saturating_mul(60),
            session.lease_ttl_mins.saturating_mul(60),
            session.context_cap_tokens,
        )?;
        let snapshot = pool.routing_snapshot()?;
        let reuse = Self::new(Box::new(pool), PriceTable::default());
        reuse.restore_snapshot(snapshot);
        Ok(reuse)
    }

    fn restore_snapshot(&self, rows: Vec<(SessionKey, u32, u64, String, bool)>) {
        let mut state = lock(&self.state);
        for (key, deposit_count, context_tokens, slot_path, is_builder) in rows {
            state.deposits.insert(key.clone(), deposit_count);
            state.last_context.insert(key.clone(), context_tokens);
            state.slot.insert(key.clone(), slot_path.clone());
            if is_builder {
                state.builder_at_slot.insert(slot_path, key);
            }
        }
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
        self.seed_builder(key, session_id, manifest, slot_path, true);
    }

    /// Seed like [`seed`](Self::seed), choosing whether the row is a builder
    /// session this slot's next construct may resume.
    pub fn seed_builder(
        &self,
        key: &SessionKey,
        session_id: &str,
        manifest: &SessionManifest,
        slot_path: &str,
        is_builder: bool,
    ) {
        self.persist_and_remember(key, None, session_id, manifest, slot_path, is_builder);
    }

    /// Decide whether this lap resumes, acquire from the pool, and return the
    /// plan the spawn threads and the evidence stamps.
    #[must_use]
    pub fn acquire(&self, request: &AcquireRequest<'_>) -> ReusePlan {
        let own = SessionKey {
            model: request.model.to_owned(),
            effort: request.effort.to_owned(),
            task: request.task.to_owned(),
        };
        let prices = request.prices.unwrap_or(&self.prices);
        let head_hash = self.head_hash_for(request.worktree);
        let slot_path = canonical_slot(request.worktree);
        let (key, edge) = self.acquire_key(request, &own, &slot_path);

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
                key: own,
                head_hash,
                slot_path,
                edge: false,
                is_builder: is_builder_command(request.command),
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
                        key: own,
                        head_hash,
                        slot_path,
                        edge: false,
                        is_builder: is_builder_command(request.command),
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
                    edge,
                    is_builder: is_builder_command(request.command),
                }
            }
            AcquireOutcome::Missed(miss) => ReusePlan {
                predicted_arm,
                predicted_turns,
                arm: ReuseArm::Fresh,
                miss: Some(map_pool_miss(miss)),
                resume: None,
                lease: None,
                key: own,
                head_hash,
                slot_path,
                edge: false,
                is_builder: is_builder_command(request.command),
            },
        }
    }

    /// Own key, or the slot's last builder when this is a cold construct on
    /// the same seat. A judge command never sees the builder handle.
    fn acquire_key(&self, request: &AcquireRequest<'_>, own: &SessionKey, slot_path: &str) -> (SessionKey, bool) {
        if is_judge_command(request.command) {
            return (own.clone(), false);
        }
        let state = lock(&self.state);
        if state.deposits.get(own).copied().unwrap_or(0) > 0 {
            return (own.clone(), false);
        }
        let Some(predecessor) = state.builder_at_slot.get(slot_path) else {
            return (own.clone(), false);
        };
        if predecessor.model != own.model || predecessor.effort != own.effort || predecessor == own {
            return (own.clone(), false);
        }
        let key = predecessor.clone();
        drop(state);
        (key, true)
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
        self.persist_and_remember(
            &plan.key,
            plan.lease.as_ref(),
            session_id,
            &manifest,
            &plan.slot_path,
            plan.is_builder,
        );
    }

    fn persist_and_remember(
        &self,
        key: &SessionKey,
        lease: Option<&LeaseToken>,
        session_id: &str,
        manifest: &SessionManifest,
        slot_path: &str,
        is_builder: bool,
    ) {
        let deposit_count = lock(&self.state).deposits.get(key).copied().unwrap_or(0).saturating_add(1);
        let _ =
            lock(&self.pool).release_routed(key, lease, session_id, manifest, (deposit_count, slot_path, is_builder));
        let mut state = lock(&self.state);
        *state.deposits.entry(key.clone()).or_insert(0) += 1;
        state.last_context.insert(key.clone(), manifest.context_tokens);
        state.slot.insert(key.clone(), slot_path.to_owned());
        if is_builder {
            state.builder_at_slot.insert(slot_path.to_owned(), key.clone());
        }
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
    /// The lane command. A judge command never resumes a builder session.
    pub command: &'a str,
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

/// Review and `AggregateReview` both dispatch `review.critic`. Independence
/// outranks any saving: a judge never resumes a builder session.
#[must_use]
pub fn is_judge_command(command: &str) -> bool {
    command == REVIEW_CRITIC_COMMAND
}

/// Every model lane that is not a judge is a builder — Construct, Refine, and
/// Reconcile all dispatch `construct.implement`.
#[must_use]
pub fn is_builder_command(command: &str) -> bool {
    !is_judge_command(command)
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

/// Stamp the acquire plan (and the result record's actuals) onto the
/// evidence envelope so the reuse rate is auditable from the journaled bytes.
///
/// Token columns and `priced_micro_usd` are the sealed-table figures, never
/// the harness's self-reported dollar amount — the same honesty rule the
/// study path already enforces.
#[must_use]
pub fn stamp_reuse(bytes: &[u8], plan: &ReusePlan, actuals: &ReuseActuals) -> Vec<u8> {
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
            "actual_turns": actuals.turns,
            "input_tokens": actuals.input_tokens,
            "cache_read_tokens": actuals.cache_read_tokens,
            "cache_write_tokens": actuals.cache_write_tokens,
            "output_tokens": actuals.output_tokens,
            "priced_micro_usd": actuals.priced_micro_usd,
            "edge": plan.edge,
        }),
    );
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| bytes.to_vec())
}

/// Per-call token evidence priced from the sealed table, stamped beside the arm.
#[derive(Debug, Clone, Default)]
pub struct ReuseActuals {
    /// `num_turns` from the result record, when present.
    pub turns: Option<u64>,
    /// Uncached input tokens.
    pub input_tokens: u64,
    /// Cache-read tokens.
    pub cache_read_tokens: u64,
    /// Cache-write tokens.
    pub cache_write_tokens: u64,
    /// Output tokens.
    pub output_tokens: u64,
    /// What those columns are worth under the sealed [`PriceTable`], or `None`
    /// when the table prices no such model.
    pub priced_micro_usd: Option<u64>,
}

impl ReuseActuals {
    /// The study-cost shape the sealed table prices.
    #[must_use]
    pub fn as_cost(&self) -> StudyCost {
        StudyCost {
            turns: self.turns.unwrap_or(0),
            input_tokens: self.input_tokens,
            cache_read_tokens: self.cache_read_tokens,
            cache_write_tokens: self.cache_write_tokens,
            output_tokens: self.output_tokens,
            ..StudyCost::default()
        }
    }
}

/// The harness session id the result record carried, when it carried one.
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

/// Token columns off the result record (nested or top-level), unpriced.
///
/// The envelope the local lane writes nests the record; a bare harness result
/// record is the object itself. Either way these are the columns the sealed
/// table prices — never the harness's self-reported dollar figure.
#[must_use]
pub fn parse_token_actuals(bytes: &[u8]) -> ReuseActuals {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return ReuseActuals::default();
    };
    let record = value.get("result_record").unwrap_or(&value);
    let number = |key| record.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
    ReuseActuals {
        turns: record.get("num_turns").and_then(serde_json::Value::as_u64),
        input_tokens: number("input"),
        cache_read_tokens: number("cache_read"),
        cache_write_tokens: number("cache_write"),
        output_tokens: number("output"),
        priced_micro_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{
        COLD_PREFIX_TOKENS, MissReason, ReuseArm, SEED_NAMED_SITE_TURNS, SessionReuse, resume_is_cheaper, stamp_reuse,
    };
    use crate::session::{SessionBackend, SessionConfig, SessionKey, SessionManifest, SqliteSessionStore};
    use aether_bloomery::{PriceRates, PriceTable};
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

    fn table(model: &str, rates: PriceRates) -> PriceTable {
        let mut table = PriceTable::default();
        table.rows.insert(model.to_owned(), rates);
        table
    }

    fn key() -> SessionKey {
        SessionKey { model: "claude-opus-5".to_owned(), effort: "high".to_owned(), task: "issue-4902".to_owned() }
    }

    fn grok_key() -> SessionKey {
        SessionKey { model: "grok-4.6".to_owned(), effort: "high".to_owned(), task: "issue-4902".to_owned() }
    }

    fn grok_request() -> super::AcquireRequest<'static> {
        super::AcquireRequest {
            model: "grok-4.6",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-0"),
            prices: None,
            command: "construct.implement",
        }
    }

    fn construct_request(
        model: &'static str,
        task: &'static str,
        worktree: &'static str,
    ) -> super::AcquireRequest<'static> {
        super::AcquireRequest {
            model,
            effort: "high",
            task,
            worktree: Path::new(worktree),
            prices: None,
            command: "construct.implement",
        }
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
    fn a_grok_key_is_decided_by_its_row_not_by_its_name() {
        // Tripwire: grok used to short-circuit to a named miss before the pool
        // was ever consulted, so the construct/refine volume seats relaunched
        // cold on every lap. Grok keys in like any other harness now — and the
        // arm still comes from the sealed row, so lifting the gate must not
        // have replaced it with an unconditional resume.
        let cheap_read = table("grok-4.6", row(500_000, 10_000_000, 25_000_000));
        let resuming = SessionReuse::memory(cheap_read);
        resuming.set_head_hash("head-A");
        resuming.set_now(1_000);
        resuming.seed(&grok_key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let plan = resuming.acquire(&grok_request());
        assert_eq!(plan.arm, ReuseArm::Resumed, "a warm grok session under a cheap-read row resumes");
        assert_eq!(plan.resume.as_deref(), Some("sess-1"));
        assert_eq!(plan.miss, None);

        let even_rates = table("grok-4.6", row(1_000_000, 1_000_000, 1_000_000));
        let going_cold = SessionReuse::memory(even_rates);
        going_cold.set_head_hash("head-A");
        going_cold.set_now(1_000);
        going_cold.seed(&grok_key(), "sess-1", &manifest("head-A", COLD_PREFIX_TOKENS, 1_000), "/slot-0");

        let plan = going_cold.acquire(&grok_request());
        assert_eq!(plan.arm, ReuseArm::Fresh, "the same warm session under a row that prices cold cheaper goes fresh");
        assert_eq!(plan.miss, None, "a prediction, not a pool miss");
    }

    #[test]
    fn a_second_lap_acquires_the_deposited_session() {
        let reuse = SessionReuse::memory(PriceTable::default());
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let plan = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
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
        let cold = reuse.acquire(&construct_request("claude-opus-5", "missing", "/slot-0"));
        assert_eq!(cold.miss, None, "lap 1 is a seed-cold, not a pool miss");
        assert_eq!(cold.arm, ReuseArm::Fresh);

        reuse.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");

        reuse.set_head_hash("head-MOVED");
        let head = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(head.miss, Some(MissReason::HeadHash));
        assert!(head.resume.is_none());

        reuse.set_head_hash("head-A");
        reuse.set_now(1_000 + 55 * 60);
        let aged = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(aged.miss, Some(MissReason::Age));

        let capped = SessionReuse::memory(PriceTable::default());
        capped.set_head_hash("head-A");
        capped.set_now(1_000);
        capped.seed(&key(), "sess-1", &manifest("head-A", 150_001, 1_000), "/slot-0");
        let over = capped.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(over.miss, Some(MissReason::ContextCap));

        let slotted = SessionReuse::memory(PriceTable::default());
        slotted.set_head_hash("head-A");
        slotted.set_now(1_000);
        slotted.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");
        let mismatch = slotted.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-1"));
        assert_eq!(mismatch.miss, Some(MissReason::SlotMismatch));
        assert!(mismatch.resume.is_none());
    }

    #[test]
    fn stamp_reuse_sits_beside_the_result_record() {
        let plan = reuse_plan_for_stamp();
        let actuals = super::ReuseActuals {
            turns: Some(12),
            input_tokens: 1_000,
            cache_read_tokens: 8_000,
            cache_write_tokens: 4_000,
            output_tokens: 200,
            priced_micro_usd: Some(42),
        };
        let stamped =
            stamp_reuse(br#"{"command":"construct.implement","result_record":{"num_turns":12}}"#, &plan, &actuals);
        let value: serde_json::Value =
            serde_json::from_slice(&stamped).expect("stamp_reuse emits JSON beside the result record");
        assert_eq!(value["session_reuse"]["arm"], "resumed");
        assert_eq!(value["session_reuse"]["predicted_turns"], 15);
        assert_eq!(value["session_reuse"]["actual_turns"], 12);
        assert_eq!(value["session_reuse"]["priced_micro_usd"], 42);
        assert_eq!(value["session_reuse"]["input_tokens"], 1_000);
        assert_eq!(value["result_record"]["num_turns"], 12, "the actuals stay on the record");
    }

    #[test]
    fn a_dependent_construct_resumes_the_predecessors_session_in_the_same_slot() {
        // Tripwire: B's own key is cold (different description), so without
        // edge lookup the first lap of a dependent always launched fresh and
        // paid the forensics tax ADR-0196 exists to avoid.
        let reuse = SessionReuse::memory(PriceTable::default());
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&key(), "sess-A", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let plan = reuse.acquire(&construct_request("claude-opus-5", "issue-B", "/slot-0"));
        assert_eq!(plan.arm, ReuseArm::Resumed, "B resumes A's construct session in A's slot");
        assert_eq!(plan.resume.as_deref(), Some("sess-A"));
        assert!(plan.edge, "the acquire must name the edge so evidence can justify it");
    }

    #[test]
    fn a_cross_seat_edge_never_resumes() {
        // Acceptance: grok built A; a different seat on B must not resume, even
        // in A's slot. The plausible bug is matching on slot path alone and
        // dropping the model/effort seat check.
        let reuse = SessionReuse::memory(PriceTable::default());
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&grok_key(), "sess-grok", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let plan = reuse.acquire(&construct_request("claude-opus-5", "issue-B", "/slot-0"));
        assert_eq!(plan.arm, ReuseArm::Fresh, "a claude B does not resume a grok A");
        assert!(plan.resume.is_none());
        assert!(!plan.edge);
    }

    #[test]
    fn a_judge_never_acquires_a_builder_session() {
        // Acceptance / ADR-0196: Review and AggregateReview seats always start
        // fresh. The plausible bug is the new slot-path builder handle leaking
        // into review.critic because the judge shares the predecessor's model,
        // effort, and slot.
        let reuse = SessionReuse::memory(PriceTable::default());
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&key(), "sess-A", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let judge = reuse.acquire(&super::AcquireRequest {
            model: "claude-opus-5",
            effort: "high",
            task: "review.critic\nissue-4902",
            worktree: Path::new("/slot-0"),
            prices: None,
            command: "review.critic",
        });
        assert_eq!(judge.arm, ReuseArm::Fresh, "a judge must not resume the builder");
        assert!(judge.resume.is_none());
        assert!(!judge.edge);

        let still = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(still.resume.as_deref(), Some("sess-A"), "the judge acquire must not have leased the builder row");
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
            edge: false,
            is_builder: true,
        }
    }

    fn file_reuse(path: &str) -> SessionReuse {
        let reuse = SessionReuse::from_config(&SessionConfig { db_path: path.to_owned(), ..SessionConfig::default() })
            .expect("a file-backed session pool opens");
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse
    }

    fn temp_pool_path(dir: &tempfile::TempDir) -> String {
        dir.path().join("sessions.db").to_str().expect("utf-8 temp path").to_owned()
    }

    fn judge_key() -> SessionKey {
        SessionKey { task: "review.critic\nissue-4902".to_owned(), ..key() }
    }

    #[test]
    fn reopening_the_pool_restores_builder_routing() {
        // Tripwire: ReuseState used to start empty after a coordinator restart
        // even though the SQLite pool still held the session. A same-slot
        // dependent then paid the cold forensics tax ADR-0196 exists to avoid;
        // a judge seat and a mismatched slot must still stay cold, and a
        // twice-deposited key must keep the retry-index rule (cold on lap 3).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_pool_path(&dir);

        let reuse = file_reuse(&path);
        reuse.seed(&key(), "sess-A", &manifest("head-A", 8_000, 1_000), "/slot-0");
        reuse.seed_builder(&judge_key(), "sess-judge", &manifest("head-A", 4_000, 1_001), "/slot-0", false);
        drop(reuse);

        let reuse = file_reuse(&path);
        let mismatch = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-1"));
        assert_eq!(mismatch.miss, Some(MissReason::SlotMismatch), "the restored slot must still guard cwd");
        assert!(mismatch.resume.is_none());

        let edge = reuse.acquire(&construct_request("claude-opus-5", "issue-B", "/slot-0"));
        assert_eq!(edge.arm, ReuseArm::Resumed, "a same-slot dependent resumes the restored builder");
        assert_eq!(edge.resume.as_deref(), Some("sess-A"), "the judge deposit must not become the slot handle");
        assert!(edge.edge);

        let judge = reuse.acquire(&super::AcquireRequest {
            model: "claude-opus-5",
            effort: "high",
            task: "review.critic\nissue-other",
            worktree: Path::new("/slot-0"),
            prices: None,
            command: "review.critic",
        });
        assert_eq!(judge.arm, ReuseArm::Fresh, "a judge command never sees the restored builder handle");
        assert!(judge.resume.is_none());
        assert!(!judge.edge);
        drop(reuse);

        let reuse = file_reuse(&path);
        // The edge acquire above left the row leased at t=1000 (15-min TTL).
        // A restart that still sees that lease must not look like a lost count.
        reuse.set_now(2_000);
        let own = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(own.arm, ReuseArm::Resumed, "deposit count 1 must survive so lap 2 still resumes");
        assert_eq!(own.resume.as_deref(), Some("sess-A"));
    }

    #[test]
    fn a_twice_deposited_key_stays_cold_after_reopen() {
        // Tripwire: inventing deposit_count=1 for every reopened row would
        // resume a session that already failed twice. The real count must
        // survive, not a "one builder deposit" default.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_pool_path(&dir);

        let reuse = file_reuse(&path);
        reuse.seed(&key(), "sess-A", &manifest("head-A", 8_000, 1_000), "/slot-0");
        reuse.seed(&key(), "sess-A2", &manifest("head-A", 8_000, 1_002), "/slot-0");
        drop(reuse);

        let reuse = file_reuse(&path);
        let third = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(third.arm, ReuseArm::Fresh, "a twice-deposited key must stay cold after restart");
        assert_eq!(third.miss, None, "retry-index seed, not a pool miss");
    }

    #[test]
    fn reopening_restores_terminal_context_into_the_inequality() {
        // Tripwire: last_context is what turns a cheap-read row into a resume
        // and an even-rate row with a large T into a prediction-cold. If
        // restart restored the count but invented or dropped T, the even-rate
        // arm would flip to Resumed (None T skips the inequality).
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_pool_path(&dir);

        let reuse = file_reuse(&path);
        reuse.seed(&key(), "sess-A", &manifest("head-A", COLD_PREFIX_TOKENS, 1_000), "/slot-0");
        drop(reuse);

        let reuse = file_reuse(&path);
        let even_rates = table("claude-opus-5", row(1_000_000, 1_000_000, 1_000_000));
        let plan = reuse.acquire(&super::AcquireRequest {
            model: "claude-opus-5",
            effort: "high",
            task: "issue-4902",
            worktree: Path::new("/slot-0"),
            prices: Some(&even_rates),
            command: "construct.implement",
        });
        assert_eq!(plan.arm, ReuseArm::Fresh, "restored T must feed the inequality");
        assert_eq!(plan.miss, None, "a prediction, not a pool miss");
        assert_eq!(plan.predicted_arm, ReuseArm::Fresh);
    }

    #[test]
    fn a_legacy_row_fails_cold_after_reopen() {
        // Tripwire: treating every reopened row as one builder deposit would
        // resume a judge (or an incomplete pre-routing row) on a dependency
        // edge. The pool row stays eligible; the hot index must not see it.
        let dir = tempfile::tempdir().expect("tempdir");
        let path = temp_pool_path(&dir);

        let mut store = SqliteSessionStore::open(
            &path,
            SessionConfig::default().cache_ttl_cutoff_mins.saturating_mul(60),
            SessionConfig::default().lease_ttl_mins.saturating_mul(60),
            SessionConfig::default().context_cap_tokens,
        )
        .expect("legacy pool opens");
        store.release(&key(), None, "sess-legacy", &manifest("head-A", 8_000, 1_000)).expect("unrouted deposit");
        drop(store);

        let reuse = file_reuse(&path);
        let edge = reuse.acquire(&construct_request("claude-opus-5", "issue-B", "/slot-0"));
        assert_eq!(edge.arm, ReuseArm::Fresh, "an unrouted legacy row must not become a builder handle");
        assert!(edge.resume.is_none());
        assert!(!edge.edge);

        let own = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(own.arm, ReuseArm::Fresh, "an unrouted legacy row must not seed a deposit count");
        assert!(own.resume.is_none());
    }
}
