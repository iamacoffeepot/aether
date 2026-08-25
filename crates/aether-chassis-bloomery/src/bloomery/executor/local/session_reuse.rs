//! Thread-bound session reuse for the construct/verify/refine retry loop (#4986).
//!
//! A session belongs to one thread — one (workpiece × role). A lap continues
//! that thread's session when the pool's eligibility gates pass (age, context
//! cap, `head_hash`, same slot path). Two static rules are the whole predicate:
//!
//! - After two consecutive failed laps on one thread, go fresh.
//! - A judging lap never resumes a building thread (`pool_task` puts the lane
//!   command ahead of the description; the commands are distinct constants).
//!
//! A first lap of a thread is fresh, with one exception, and only one: a
//! dependent construct offered the journaled session of a predecessor it
//! declares an edge to (#5178), which forks that conversation so the dependent
//! records an id of its own.
//!
//! There used to be a second route, and it was not an edge at all (#5427). The
//! pool kept the last builder session deposited *at each slot path* and handed
//! it to any cold construct that landed in that slot with a matching model and
//! effort — no workpiece check, no declared dependency. Two unrelated members
//! dispatched into one slot six minutes apart therefore shared a conversation:
//! the second opened carrying the first's whole history (188k of context
//! against 79k) and then deposited the first's session id as its own. The slot
//! path was never a member identity, and with the checkout keyed to the
//! workpiece (#5425) it is not even a stable directory for one.
//!
//! Completed actuals still
//! fold into a durable (model, effort, family) calibration cell so a later
//! change can read it; the acquire path no longer consults it. Each lap's
//! evidence stamps the sealed-table price of the observed calls beside the
//! replayed other-arm counterfactual.
//!
//! Same-member Refine always resumes the construct session journaled on the
//! dispatch record, whatever context that session carries. A refine lap is the
//! same author fixing findings against the tree it just built, so relaunching
//! it cold re-reads the whole member from scratch — strictly more expensive
//! than any long-context band. That path is keyed by (bloom, workpiece), not
//! the pool task text, so a findings overlay cannot hide the handle.
//!
//! A dependent Construct at unblock offers a predecessor's journaled session
//! the same way (#5178): projection adds a per-link increment, warmth reuses
//! the pool's cache-TTL cutoff, and a join picks the largest stored context.
//! Missing, stale, over-cliff, or refused handles launch fresh.

use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

use aether_bloomery::{Digest, PriceTable, REVIEW_CRITIC_COMMAND, StudyCall, StudyCost};

use crate::session::{
    AcquireMiss, AcquireOutcome, LeaseToken, LeasedSession, SessionBackend, SessionConfig, SessionKey, SessionManifest,
    SqliteSessionStore,
};

/// What a resumed dependent construct must be told: the tree is the splice, not
/// the files the predecessor session last edited on the bloom base.
pub const SPLICED_RESET_NOTE: &str = "The working tree was reset to the spliced dependency candidate; do not assume files from the predecessor session are still at the bloom base.";

/// Per-sample turn cap: a missing/zero count is ignored, anything above this
/// is recorded as this so one runaway cannot dominate the cell.
const TURN_OBSERVATION_CAP: u64 = 32;
/// Per-sample terminal-context cap, matching the pool's default ceiling.
const CONTEXT_OBSERVATION_CAP: u64 = 150_000;
/// Default prompt-token threshold a resumed *dependent construct* must project
/// under. Matches grok-4.6's measured long-context pricing cliff. Same-member
/// refine does not consult it.
pub(super) const DEFAULT_PRICING_CLIFF_TOKENS: u64 = 200_000;
/// Tokens one dependency link is projected to add on top of a predecessor's
/// stored context. Measured successor increment (#5178).
pub(super) const DEFAULT_DEPENDENCY_INCREMENT_TOKENS: u64 = 56_000;
/// Default provider-cache warmth, matching [`SessionConfig::cache_ttl_cutoff_mins`].
pub(super) const DEFAULT_CACHE_TTL_SECS: u64 = 55 * 60;

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
    /// The stored predecessor context plus the dependency increment projects
    /// at or over the harness pricing-cliff threshold.
    PricingCliff,
    /// The harness refused the resume handle before a billed turn.
    ResumeRefused,
    /// A sibling dependent already continues this predecessor's session in the
    /// tree that session is bound to, so this member opened its own.
    SessionTaken,
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
            Self::PricingCliff => "pricing_cliff",
            Self::ResumeRefused => "resume_refused",
            Self::SessionTaken => "session_taken",
        }
    }
}

/// Whether a journaled construct session should be resumed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefineResume {
    /// Resume this harness session id.
    Resumed(String),
    /// Launch fresh. `miss` names why when a journaled handle was considered.
    Fresh {
        /// Why a journaled handle was not resumed, when one existed.
        miss: Option<MissReason>,
    },
}

/// One predecessor's journaled construct session, as the dependent-construct
/// resume gate judges it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PredecessorCandidate {
    /// The harness session id construct journaled.
    pub session_id: String,
    /// Terminal context stored with the handle.
    pub context_tokens: u64,
    /// Deposit time in unix seconds, when the row recorded one.
    pub deposited_unix: Option<u64>,
    /// Whether this dispatch stands in the tree that session is bound to —
    /// whether the dependent inherited *this* predecessor's slug (#5425).
    ///
    /// A harness binds a conversation to the directory it was born in, and one
    /// predecessor's tree is inherited by one dependent: a sibling that opened
    /// its own session cannot resume a handle whose edits would land in someone
    /// else's live checkout, so its candidate is not eligible however warm and
    /// cheap the stored context is.
    pub continues_tree: bool,
}

/// Decide whether a journaled construct session is worth resuming.
///
/// A usable handle always resumes: the refine lap is the same author fixing
/// findings on the tree that session just built, so the alternative is not a
/// cheaper prompt but a cold re-read of the whole member. Only an empty or
/// whitespace-only id — an unparseable handle — launches fresh. The pricing
/// cliff gates dependent-construct chains only ([`decide_predecessor_resume`]),
/// where the resumed context belongs to a *different* member.
#[must_use]
pub fn decide_refine_resume(session_id: &str) -> RefineResume {
    if usable_session_id(session_id) {
        RefineResume::Resumed(session_id.to_owned())
    } else {
        RefineResume::Fresh { miss: None }
    }
}

/// Decide whether a dependent construct should resume a predecessor session.
///
/// Empty or unusable ids, a session a sibling already continues in the tree it
/// is bound to, a missing deposit time, a deposit older than `warmth_secs`, and
/// a projection of `context + increment` at or over the cliff all launch
/// fresh. Among handles that pass, the largest stored
/// context wins — the session that already carries the most shared prefix.
#[must_use]
pub fn decide_predecessor_resume(
    candidates: &[PredecessorCandidate],
    now_unix: u64,
    warmth_secs: u64,
    increment_tokens: u64,
    pricing_cliff_tokens: u64,
) -> RefineResume {
    let mut eligible: Vec<&PredecessorCandidate> = Vec::new();
    let mut saw_stale = false;
    let mut saw_cliff = false;
    let mut saw_taken = false;
    for candidate in candidates {
        if !usable_session_id(&candidate.session_id) {
            continue;
        }
        if !candidate.continues_tree {
            saw_taken = true;
            continue;
        }
        let Some(deposited) = candidate.deposited_unix else {
            saw_stale = true;
            continue;
        };
        if now_unix.saturating_sub(deposited) > warmth_secs {
            saw_stale = true;
            continue;
        }
        if candidate.context_tokens.saturating_add(increment_tokens) >= pricing_cliff_tokens {
            saw_cliff = true;
            continue;
        }
        eligible.push(candidate);
    }
    if let Some(best) = eligible.iter().max_by_key(|candidate| candidate.context_tokens) {
        return RefineResume::Resumed(best.session_id.clone());
    }
    if saw_cliff {
        RefineResume::Fresh { miss: Some(MissReason::PricingCliff) }
    } else if saw_stale {
        RefineResume::Fresh { miss: Some(MissReason::Age) }
    } else if saw_taken {
        RefineResume::Fresh { miss: Some(MissReason::SessionTaken) }
    } else {
        RefineResume::Fresh { miss: None }
    }
}

/// A session id the harness can be asked to resume — non-empty after trim.
#[must_use]
pub fn usable_session_id(session_id: &str) -> bool {
    !session_id.trim().is_empty()
}

/// The acquire decision remembered on a run so `stream_evidence` can stamp it
/// beside the result record's actuals and deposit the session that came back.
#[derive(Debug, Clone)]
pub struct ReusePlan {
    /// The arm the lap actually took.
    pub arm: ReuseArm,
    /// Why a lap that could have resumed launched fresh, when it did.
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
    now: Mutex<Option<u64>>,
    head_hash: Mutex<Option<String>>,
    state: Mutex<ReuseState>,
    pricing_cliff_tokens: Mutex<u64>,
    dependency_increment_tokens: Mutex<u64>,
    cache_ttl_secs: Mutex<u64>,
}

#[derive(Default)]
struct ReuseState {
    /// Consecutive non-concluding laps on this key. Reset on a conclusion;
    /// two in a row send the next acquire fresh.
    failures: HashMap<SessionKey, u32>,
    /// Terminal context of the last deposit, replayed if the slot guard has
    /// to put a mismatched lease back.
    last_context: HashMap<SessionKey, u64>,
    /// Slot path the last deposit for this key came from.
    slot: HashMap<SessionKey, String>,
}

impl SessionReuse {
    /// Build over an explicit pool.
    #[must_use]
    pub fn new(pool: Box<dyn SessionBackend>) -> Self {
        Self {
            pool: Mutex::new(pool),
            now: Mutex::new(None),
            head_hash: Mutex::new(None),
            state: Mutex::new(ReuseState::default()),
            pricing_cliff_tokens: Mutex::new(DEFAULT_PRICING_CLIFF_TOKENS),
            dependency_increment_tokens: Mutex::new(DEFAULT_DEPENDENCY_INCREMENT_TOKENS),
            cache_ttl_secs: Mutex::new(DEFAULT_CACHE_TTL_SECS),
        }
    }

    /// An in-memory pool with the capability's default eligibility knobs.
    ///
    /// Isolated (`:memory:`), so tests do not share a table with the mounted
    /// capability or with each other. Production opens through
    /// [`from_config`](Self::from_config).
    #[must_use]
    pub fn memory() -> Self {
        let defaults = SessionConfig::default();
        let pool = SqliteSessionStore::open(
            ":memory:",
            defaults.cache_ttl_cutoff_mins.saturating_mul(60),
            defaults.lease_ttl_mins.saturating_mul(60),
            defaults.context_cap_tokens,
        )
        .expect("an in-memory session pool opens");
        Self::new(Box::new(pool))
    }

    /// Open the same pool the mounted [`SessionPoolCapability`](crate::session::SessionPoolCapability)
    /// opened: the operator's `SessionConfig` path and eligibility knobs, not
    /// literals. The sealed rates ride the stamp, not this constructor.
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
        let reuse = Self::new(Box::new(pool));
        *lock(&reuse.pricing_cliff_tokens) = session.pricing_cliff_tokens;
        *lock(&reuse.dependency_increment_tokens) = session.dependency_increment_tokens;
        *lock(&reuse.cache_ttl_secs) = session.cache_ttl_cutoff_mins.saturating_mul(60);
        reuse.restore_snapshot(snapshot);
        Ok(reuse)
    }

    fn restore_snapshot(&self, rows: Vec<(SessionKey, u32, u64, String, bool)>) {
        let mut state = lock(&self.state);
        for (key, deposit_count, context_tokens, slot_path, _is_builder) in rows {
            state.failures.insert(key.clone(), deposit_count);
            state.last_context.insert(key.clone(), context_tokens);
            state.slot.insert(key, slot_path);
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

    /// Pin the pricing-cliff threshold — tests only.
    pub fn set_pricing_cliff_tokens(&self, tokens: u64) {
        *lock(&self.pricing_cliff_tokens) = tokens;
    }

    /// The prompt-token cliff a dependent-construct resume must project under.
    #[must_use]
    pub fn pricing_cliff_tokens(&self) -> u64 {
        *lock(&self.pricing_cliff_tokens)
    }

    /// Tokens one dependency link is projected to add on a predecessor resume.
    #[must_use]
    pub fn dependency_increment_tokens(&self) -> u64 {
        *lock(&self.dependency_increment_tokens)
    }

    /// Provider-cache warmth, in seconds, a predecessor resume must sit inside.
    #[must_use]
    pub fn cache_ttl_secs(&self) -> u64 {
        *lock(&self.cache_ttl_secs)
    }

    /// The clock acquire/deposit and journaled resume gates share — tests pin it.
    #[must_use]
    pub fn unix_now(&self) -> u64 {
        self.now()
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
        self.persist_and_remember(key, None, session_id, manifest, (slot_path, is_builder), None);
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
        let head_hash = self.head_hash_for(request.worktree);
        let slot_path = canonical_slot(request.worktree);
        // The key is the lap's own and nothing else (#5427). A thread is
        // (workpiece × role), so the only session this lap may continue is the
        // one its own thread deposited; a declared-edge inheritance is decided
        // upstream against the member graph, not guessed at from a directory.
        let key = own.clone();

        let (failures, deposited) = {
            let state = lock(&self.state);
            (state.failures.get(&key).copied().unwrap_or(0), state.slot.contains_key(&key))
        };
        let is_builder = is_builder_command(request.command);

        // First lap of a thread is fresh by definition; two consecutive
        // failures go fresh without asking the pool.
        if !deposited || failures >= 2 {
            return ReusePlan {
                arm: ReuseArm::Fresh,
                miss: None,
                resume: None,
                lease: None,
                key: own,
                head_hash,
                slot_path,
                edge: false,
                is_builder,
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
                        arm: ReuseArm::Fresh,
                        miss: Some(miss),
                        resume: None,
                        lease: None,
                        key: own,
                        head_hash,
                        slot_path,
                        edge: false,
                        is_builder,
                    };
                }
                ReusePlan {
                    arm: ReuseArm::Resumed,
                    miss: None,
                    resume: Some(leased.session_bytes),
                    lease: Some(leased.lease),
                    key,
                    head_hash,
                    slot_path,
                    edge: false,
                    is_builder,
                }
            }
            AcquireOutcome::Missed(miss) => ReusePlan {
                arm: ReuseArm::Fresh,
                miss: Some(map_pool_miss(miss)),
                resume: None,
                lease: None,
                key: own,
                head_hash,
                slot_path,
                edge: false,
                is_builder,
            },
        }
    }

    /// Fold a completed attempt's token actuals into the durable cell for
    /// this plan's (model, effort, family). Missing or zero turns are ignored;
    /// a turn or context above the per-sample cap is recorded at the cap.
    pub fn observe(&self, plan: &ReusePlan, actuals: &ReuseActuals) {
        let Some(turns) = usable_turns(actuals.turns) else {
            return;
        };
        let family = lane_family(&plan.key.task);
        if family.is_empty() {
            return;
        }
        let context = actuals
            .input_tokens
            .saturating_add(actuals.cache_read_tokens)
            .saturating_add(actuals.cache_write_tokens)
            .min(CONTEXT_OBSERVATION_CAP);
        let _ = lock(&self.pool).update_calibration(&plan.key.model, &plan.key.effort, family, turns, context);
    }

    /// Deposit the session the attempt produced, so the next lap can judge it.
    ///
    /// `concluded` is whether the lap produced a passing verdict. A conclusion
    /// resets the consecutive-failure counter; anything else increments it.
    pub fn deposit(&self, plan: &ReusePlan, session_id: &str, context_tokens: u64, concluded: bool) {
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
            (&plan.slot_path, plan.is_builder),
            Some(concluded),
        );
    }

    fn persist_and_remember(
        &self,
        key: &SessionKey,
        lease: Option<&LeaseToken>,
        session_id: &str,
        manifest: &SessionManifest,
        seat: (&str, bool),
        concluded: Option<bool>,
    ) {
        let (slot_path, is_builder) = seat;
        let mut state = lock(&self.state);
        match concluded {
            Some(true) => {
                state.failures.insert(key.clone(), 0);
            }
            Some(false) => {
                *state.failures.entry(key.clone()).or_insert(0) += 1;
            }
            None => {
                state.failures.entry(key.clone()).or_insert(0);
            }
        }
        // The pool snapshot drops a zero count, so a concluded (or seeded)
        // row still persists as 1 — enough to restore slot and builder state.
        let routing_count = state.failures.get(key).copied().unwrap_or(0).max(1);
        state.last_context.insert(key.clone(), manifest.context_tokens);
        state.slot.insert(key.clone(), slot_path.to_owned());
        drop(state);
        let _ =
            lock(&self.pool).release_routed(key, lease, session_id, manifest, (routing_count, slot_path, is_builder));
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

/// A reuse plan that does not lease from the pool — same-member refine resume
/// from a journaled construct handle, or the fresh fallback that handle produces.
#[must_use]
pub fn plan_for(
    request: &AcquireRequest<'_>,
    arm: ReuseArm,
    miss: Option<MissReason>,
    resume: Option<String>,
) -> ReusePlan {
    ReusePlan {
        arm,
        miss,
        resume,
        lease: None,
        key: SessionKey {
            model: request.model.to_owned(),
            effort: request.effort.to_owned(),
            task: request.task.to_owned(),
        },
        head_hash: static_prefix_hash(request.worktree),
        slot_path: canonical_slot(request.worktree),
        edge: false,
        is_builder: is_builder_command(request.command),
    }
}

/// The calibration key's family axis: the command's leading segment, so
/// `construct.implement` and a construct retry share a cell while `review`
/// stays isolated. A pool `task` (`command` or `command\\ndescription`) is
/// accepted the same way.
#[must_use]
pub fn lane_family(command: &str) -> &str {
    let command = command.split('\n').next().unwrap_or(command);
    command.split('.').next().filter(|part| !part.is_empty()).unwrap_or(command)
}

fn usable_turns(turns: Option<u64>) -> Option<u64> {
    turns.filter(|turns| *turns > 0).map(|turns| turns.min(TURN_OBSERVATION_CAP))
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
    aether_bloomery::encode_hex(bytes)
}

/// Stamp the acquire plan (and the result record's actuals) onto the
/// evidence envelope so the reuse rate is auditable from the journaled bytes.
///
/// Token columns and the priced pair are sealed-table figures, never the
/// harness's self-reported dollar amount — the same honesty rule the study
/// path already enforces. The counterfactual reprices the same calls under
/// the other arm by swapping each call's cache-read and cache-write columns.
#[must_use]
pub fn stamp_reuse(
    bytes: &[u8],
    plan: &ReusePlan,
    actuals: &ReuseActuals,
    prices: &PriceTable,
    calls: Option<&[StudyCall]>,
) -> Vec<u8> {
    let Ok(mut value) = serde_json::from_slice::<serde_json::Value>(bytes) else {
        return bytes.to_vec();
    };
    let Some(object) = value.as_object_mut() else {
        return bytes.to_vec();
    };
    let priced_micro_usd = prices.price_dispatch(&plan.key.model, &actuals.as_cost(), calls);
    let counterfactual = calls.filter(|calls| !calls.is_empty()).and_then(|calls| {
        let replayed: Vec<StudyCall> = calls.iter().copied().map(|call| replay_other_arm(plan.arm, call)).collect();
        prices.price_dispatch(&plan.key.model, &sum_calls(&replayed), Some(&replayed))
    });
    object.insert(
        "session_reuse".to_owned(),
        serde_json::json!({
            "arm": plan.arm.as_str(),
            "miss": plan.miss.map(MissReason::as_str),
            "actual_turns": actuals.turns,
            "input_tokens": actuals.input_tokens,
            "cache_read_tokens": actuals.cache_read_tokens,
            "cache_write_tokens": actuals.cache_write_tokens,
            "output_tokens": actuals.output_tokens,
            "duration_millis": actuals.duration_millis,
            "priced_micro_usd": priced_micro_usd,
            "counterfactual_micro_usd": counterfactual,
            "edge": plan.edge,
        }),
    );
    serde_json::to_vec_pretty(&value).unwrap_or_else(|_| bytes.to_vec())
}

/// Reprice the other arm: a resumed lap's cache-reads would have been writes
/// if it had launched fresh, and a fresh lap's writes would have been reads.
fn replay_other_arm(arm: ReuseArm, call: StudyCall) -> StudyCall {
    match arm {
        ReuseArm::Resumed => StudyCall {
            cache_write_tokens: call.cache_write_tokens.saturating_add(call.cache_read_tokens),
            cache_read_tokens: 0,
            ..call
        },
        ReuseArm::Fresh => StudyCall {
            cache_read_tokens: call.cache_read_tokens.saturating_add(call.cache_write_tokens),
            cache_write_tokens: 0,
            ..call
        },
    }
}

fn sum_calls(calls: &[StudyCall]) -> StudyCost {
    calls.iter().fold(StudyCost::default(), |cost, call| StudyCost {
        input_tokens: cost.input_tokens.saturating_add(call.input_tokens),
        cache_read_tokens: cost.cache_read_tokens.saturating_add(call.cache_read_tokens),
        cache_write_tokens: cost.cache_write_tokens.saturating_add(call.cache_write_tokens),
        cache_write_1h_tokens: cost.cache_write_1h_tokens.saturating_add(call.cache_write_1h_tokens),
        cache_write_5m_tokens: cost.cache_write_5m_tokens.saturating_add(call.cache_write_5m_tokens),
        output_tokens: cost.output_tokens.saturating_add(call.output_tokens),
        ..cost
    })
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
    /// Wall-clock duration the harness reported, in milliseconds.
    pub duration_millis: Option<u64>,
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

/// Terminal context: the last call's prompt (uncached input plus both cache
/// classes), which is what the next resume would re-read. `None` when `calls`
/// is absent or empty — the aggregate columns are a billed sum, not a prompt.
#[must_use]
pub fn parse_context_tokens(bytes: &[u8]) -> Option<u64> {
    let value: serde_json::Value = serde_json::from_slice(bytes).ok()?;
    let last = value.get("result_record")?.get("calls")?.as_array()?.last()?;
    let number = |key| last.get(key).and_then(serde_json::Value::as_u64).unwrap_or(0);
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
        duration_millis: record.get("duration_ms").and_then(serde_json::Value::as_u64),
        priced_micro_usd: None,
    }
}

#[cfg(test)]
mod tests {
    use super::{MissReason, ReuseArm, SessionReuse, replay_other_arm, stamp_reuse, sum_calls};
    use crate::session::{SessionBackend, SessionConfig, SessionKey, SessionManifest, SqliteSessionStore};
    use aether_bloomery::{PriceRates, PriceTable, StudyCall};
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
    fn a_grok_key_reaches_the_pool_like_any_other() {
        // Tripwire: grok used to short-circuit to a named miss before the pool
        // was ever consulted, so the construct/refine volume seats relaunched
        // cold on every lap. The remaining invariant is that the harness name
        // is not a gate — a warm grok row leases like any other.
        let reuse = SessionReuse::memory();
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&grok_key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let plan = reuse.acquire(&grok_request());
        assert_eq!(plan.arm, ReuseArm::Resumed, "a warm grok session resumes");
        assert_eq!(plan.resume.as_deref(), Some("sess-1"));
        assert_eq!(plan.miss, None);
    }

    #[test]
    fn a_second_lap_acquires_the_deposited_session() {
        let reuse = SessionReuse::memory();
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let plan = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(plan.arm, ReuseArm::Resumed, "lap 2 resumes the deposited session");
        assert_eq!(plan.resume.as_deref(), Some("sess-1"));
    }

    #[test]
    fn each_named_miss_falls_back_to_fresh() {
        let reuse = SessionReuse::memory();
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

        let capped = SessionReuse::memory();
        capped.set_head_hash("head-A");
        capped.set_now(1_000);
        capped.seed(&key(), "sess-1", &manifest("head-A", 150_001, 1_000), "/slot-0");
        let over = capped.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(over.miss, Some(MissReason::ContextCap));

        let slotted = SessionReuse::memory();
        slotted.set_head_hash("head-A");
        slotted.set_now(1_000);
        slotted.seed(&key(), "sess-1", &manifest("head-A", 8_000, 1_000), "/slot-0");
        let mismatch = slotted.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-1"));
        assert_eq!(mismatch.miss, Some(MissReason::SlotMismatch));
        assert!(mismatch.resume.is_none());
    }

    fn call(input: u64, cache_read: u64, cache_write: u64, output: u64) -> StudyCall {
        StudyCall {
            input_tokens: input,
            cache_read_tokens: cache_read,
            cache_write_tokens: cache_write,
            output_tokens: output,
            ..StudyCall::default()
        }
    }

    #[test]
    fn stamp_reuse_sits_beside_the_result_record() {
        // Plausible bug: pricing the counterfactual off the aggregate columns,
        // or emitting the actual twice under two names, leaves a ledger that
        // looks complete and measures nothing.
        let plan = reuse_plan_for_stamp();
        let prices = table("claude-opus-5", row(500_000, 10_000_000, 25_000_000));
        let calls = [call(1_000, 8_000, 0, 100), call(2_000, 4_000, 500, 100)];
        let actuals = super::ReuseActuals {
            turns: Some(12),
            input_tokens: 3_000,
            cache_read_tokens: 12_000,
            cache_write_tokens: 500,
            output_tokens: 200,
            duration_millis: Some(1_200),
            priced_micro_usd: None,
        };
        let envelope = br#"{"command":"construct.implement","result_record":{"num_turns":12,"input":3000,"cache_read":12000,"cache_write":500,"duration_ms":1200}}"#;
        let stamped = stamp_reuse(envelope, &plan, &actuals, &prices, Some(&calls));
        let value: serde_json::Value =
            serde_json::from_slice(&stamped).expect("stamp_reuse emits JSON beside the result record");

        let priced_micro_usd = prices
            .price_dispatch("claude-opus-5", &actuals.as_cost(), Some(&calls))
            .expect("the fixture table prices this model");
        let replayed: Vec<StudyCall> = calls.iter().copied().map(|call| replay_other_arm(plan.arm, call)).collect();
        let counterfactual = prices
            .price_dispatch("claude-opus-5", &sum_calls(&replayed), Some(&replayed))
            .expect("the fixture table prices the replayed calls");
        assert_ne!(priced_micro_usd, counterfactual, "read and write columns differ, so the arms must price apart");
        assert_eq!(value["session_reuse"]["arm"], "resumed");
        assert_eq!(value["session_reuse"]["actual_turns"], 12);
        assert_eq!(value["session_reuse"]["priced_micro_usd"], priced_micro_usd);
        assert_eq!(value["session_reuse"]["counterfactual_micro_usd"], counterfactual);
        assert_eq!(value["session_reuse"]["input_tokens"], 3_000);
        assert_eq!(value["session_reuse"]["output_tokens"], 200);
        assert_eq!(value["session_reuse"]["duration_millis"], 1_200);
        assert_eq!(value["result_record"]["num_turns"], 12, "the actuals stay on the record");
        assert_eq!(value["result_record"]["input"], 3_000, "result_record columns stay untouched");
        assert_eq!(value["result_record"]["cache_read"], 12_000);
        assert_eq!(value["result_record"]["cache_write"], 500);
    }

    #[test]
    fn a_journaled_construct_session_resumes_whatever_context_it_carries() {
        // Plausible bug: reintroducing a context gate on the same-member refine
        // path, which reads prudent and makes every findings lap re-read the
        // member cold; or treating an empty handle as resumable, so a missing
        // parse threads a garbage `--resume` and wedges the lap.
        assert_eq!(super::decide_refine_resume("sess-1"), super::RefineResume::Resumed("sess-1".to_owned()));
        assert_eq!(
            super::decide_refine_resume("sess-huge"),
            super::RefineResume::Resumed("sess-huge".to_owned()),
            "a refine resumes its own construct session however large that context grew",
        );
        assert_eq!(
            super::decide_refine_resume("   "),
            super::RefineResume::Fresh { miss: None },
            "whitespace is an unparseable handle, not a session to resume",
        );
        assert_eq!(super::decide_refine_resume(""), super::RefineResume::Fresh { miss: None });
    }

    fn predecessor(session_id: &str, context_tokens: u64, deposited_unix: Option<u64>) -> super::PredecessorCandidate {
        super::PredecessorCandidate {
            session_id: session_id.to_owned(),
            context_tokens,
            deposited_unix,
            continues_tree: true,
        }
    }

    #[test]
    fn a_session_a_sibling_continues_is_not_resumed_however_warm_it_is() {
        // Plausible bug: the second dependent of one predecessor resumes the
        // conversation the first one inherited. The handle is warm, under the
        // cliff, and the cheapest thing on offer — and resuming it would put
        // this member's edits in the tree that session is bound to, which its
        // sibling is building in right now.
        let now = 10_000;
        let taken = super::PredecessorCandidate { continues_tree: false, ..predecessor("sess-a", 8_000, Some(now)) };

        assert_eq!(
            super::decide_predecessor_resume(&[taken], now, 55 * 60, 56_000, 200_000),
            super::RefineResume::Fresh { miss: Some(MissReason::SessionTaken) },
            "a session bound to a sibling's tree is not this member's to resume",
        );
    }

    #[test]
    fn a_predecessor_session_resumes_only_when_warm_and_under_the_cliff() {
        // Plausible bug: resuming a stale or over-cliff predecessor, or picking
        // the first named parent at a join instead of the largest stored context.
        let now = 10_000;
        let warmth = 55 * 60;
        let increment = 56_000;
        let cliff = 200_000;
        let decide = |candidates: &[super::PredecessorCandidate]| {
            super::decide_predecessor_resume(candidates, now, warmth, increment, cliff)
        };

        assert_eq!(
            decide(&[predecessor("sess-a", 8_000, Some(now))]),
            super::RefineResume::Resumed("sess-a".to_owned()),
        );
        assert_eq!(
            decide(&[predecessor("sess-a", 144_000, Some(now))]),
            super::RefineResume::Fresh { miss: Some(MissReason::PricingCliff) },
            "144k + the 56k link increment is the cliff, not under it",
        );
        assert_eq!(
            decide(&[predecessor("sess-a", 8_000, Some(now - warmth - 1))]),
            super::RefineResume::Fresh { miss: Some(MissReason::Age) },
            "a deposit older than the cache TTL is stale",
        );
        assert_eq!(
            decide(&[predecessor("sess-a", 8_000, None)]),
            super::RefineResume::Fresh { miss: Some(MissReason::Age) },
            "a row that never stamped a deposit time is not assumed warm",
        );
        assert_eq!(
            decide(&[predecessor("   ", 8_000, Some(now))]),
            super::RefineResume::Fresh { miss: None },
            "whitespace is an unparseable handle, not a session to resume",
        );
        assert_eq!(decide(&[]), super::RefineResume::Fresh { miss: None }, "missing sessions launch fresh");
        assert_eq!(
            decide(&[predecessor("sess-small", 8_000, Some(now)), predecessor("sess-large", 40_000, Some(now))]),
            super::RefineResume::Resumed("sess-large".to_owned()),
            "a join resumes the largest stored context",
        );
    }

    #[test]
    fn parse_context_tokens_reads_the_last_call() {
        // Plausible bug: summing every call's prompt (the aggregate columns)
        // re-inflates the deposit ~48× past context_cap_tokens and every later
        // acquire is a silent ContextCap miss.
        let earlier = 9_527_000_u64;
        let last = call(50_000, 140_000, 13_000, 200);
        let last_prompt = last.input_tokens + last.cache_read_tokens + last.cache_write_tokens;
        let fixture = format!(
            r#"{{"result_record":{{"input":{sum},"cache_read":0,"cache_write":0,"calls":[{{"input":{earlier},"cache_read":0,"cache_write":0}},{{"input":{input},"cache_read":{read},"cache_write":{write}}}]}}}}"#,
            sum = earlier + last_prompt,
            input = last.input_tokens,
            read = last.cache_read_tokens,
            write = last.cache_write_tokens,
        );
        assert_eq!(super::parse_context_tokens(fixture.as_bytes()), Some(last_prompt));
        assert!(
            earlier + last_prompt > last_prompt.saturating_mul(40),
            "the fixture's aggregate must be far enough from the last prompt to catch a sum"
        );
    }

    #[test]
    fn three_concluding_laps_still_resume() {
        // Plausible bug: keeping the deposit-counting semantics under the new
        // name, which reads correct and censors every healthy thread at its
        // third lap.
        let reuse = SessionReuse::memory();
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);

        let first = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(first.arm, ReuseArm::Fresh);
        reuse.deposit(&first, "sess-1", 8_000, true);

        let second = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(second.arm, ReuseArm::Resumed, "lap 2 resumes after a conclusion");
        reuse.deposit(&second, "sess-2", 8_000, true);

        let third = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(third.arm, ReuseArm::Resumed, "lap 3 still resumes after two conclusions");
        reuse.deposit(&third, "sess-3", 8_000, true);

        let fourth = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(fourth.arm, ReuseArm::Resumed, "a healthy thread is not cut off at lap 3");
        assert_eq!(fourth.resume.as_deref(), Some("sess-3"));
    }

    #[test]
    fn two_consecutive_non_concluding_laps_go_fresh() {
        // Plausible bug: resetting the counter on every deposit, or counting
        // total deposits, so two failures never trip the rule (or a healthy
        // thread trips it).
        let reuse = SessionReuse::memory();
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);

        let first = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        reuse.deposit(&first, "sess-1", 8_000, false);

        let second = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(second.arm, ReuseArm::Resumed, "one failure still resumes");
        reuse.deposit(&second, "sess-2", 8_000, false);

        let third = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(third.arm, ReuseArm::Fresh, "two consecutive failures go fresh");
        assert_eq!(third.miss, None, "the two-failures rule, not a pool miss");
        assert!(third.resume.is_none());
    }

    #[test]
    fn an_unrelated_member_in_the_same_slot_starts_fresh() {
        // Acceptance for #5427: on 2026-08-21 two members with no declared edge
        // between them were dispatched into slot-3 six minutes apart. The pool
        // kept the slot's last builder session and handed it to the second
        // whenever model and effort matched — no workpiece check — so the second
        // construct opened carrying the first's whole conversation (188k of
        // context against 79k) and then deposited the first's session id as its
        // own. A thread is (workpiece x role); a directory is not a member.
        let reuse = SessionReuse::memory();
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&key(), "sess-A", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let unrelated = reuse.acquire(&construct_request("claude-opus-5", "issue-B", "/slot-0"));
        assert_eq!(unrelated.arm, ReuseArm::Fresh, "a member's first construct is fresh, whoever built here last");
        assert!(unrelated.resume.is_none(), "and inherits nothing to resume");
        assert!(!unrelated.edge, "there is no edge here to name");

        let own = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(own.resume.as_deref(), Some("sess-A"), "the member that opened the thread still continues it");
    }

    #[test]
    fn a_judge_never_acquires_a_builder_session() {
        // Acceptance / ADR-0196: Review and AggregateReview seats always start
        // fresh. A judge's own key is its own — the plausible bug is a judge
        // acquire leasing the builder's row on its way past and leaving the
        // member unable to resume its own thread.
        let reuse = SessionReuse::memory();
        reuse.set_head_hash("head-A");
        reuse.set_now(1_000);
        reuse.seed(&key(), "sess-A", &manifest("head-A", 8_000, 1_000), "/slot-0");

        let judge = reuse.acquire(&super::AcquireRequest {
            model: "claude-opus-5",
            effort: "high",
            task: "review.critic\nissue-4902",
            worktree: Path::new("/slot-0"),
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
    fn reopening_the_pool_restores_a_members_own_session() {
        // Tripwire: ReuseState used to start empty after a coordinator restart
        // even though the SQLite pool still held the session, so a member's own
        // retry lap relaunched cold. The slot guard has to come back with it —
        // a restored row is still bound to the directory it was deposited from.
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

        let unrelated = reuse.acquire(&construct_request("claude-opus-5", "issue-B", "/slot-0"));
        assert_eq!(unrelated.arm, ReuseArm::Fresh, "a restore must not hand one member's thread to another");
        assert!(unrelated.resume.is_none());
        drop(reuse);

        let reuse = file_reuse(&path);
        // The edge acquire above left the row leased at t=1000 (15-min TTL).
        // A restart that still sees that lease must not look like a lost count.
        reuse.set_now(2_000);
        let own = reuse.acquire(&construct_request("claude-opus-5", "issue-4902", "/slot-0"));
        assert_eq!(own.arm, ReuseArm::Resumed, "a restored builder row still resumes");
        assert_eq!(own.resume.as_deref(), Some("sess-A"));
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
