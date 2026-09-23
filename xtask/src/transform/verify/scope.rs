//! Which workspace crates the umbrella's compiling members run over (#4890).
//!
//! Impact is sound by linkage: a target observes a change only if it links a
//! changed crate, so the reverse-dependency closure of the candidate diff's
//! crates is a complete over-approximation of what that change can break —
//! cross-crate `inventory` registries included, since visibility there
//! requires linkage and linkage is what the closure computes. Narrowing
//! `verify.{clippy,docs,test}` to that closure is the difference between a
//! near-leaf member recompiling the render/audio/wgpu tree on every refine lap
//! and recompiling what its change can reach.
//!
//! The narrowing is opt-in on the work order naming a diff base, and that is
//! what keeps it off the stage that proves the landing. A member `Verify`
//! carries one — its candidate is the committed range `base..HEAD` — while the
//! whole-bloom aggregate verify carries none, so it resolves
//! [`Scope::Workspace`] and runs the identical workspace-wide argv it always
//! did. The stage that repeats gets the speed; the stage that decides what
//! lands keeps the whole tree.
//!
//! Everything the closure cannot see fails open, and the blind spots are
//! enumerated rather than assumed: a workspace-level input (lint config,
//! cargo/nextest config, the gate code itself, or a lockfile whose moved
//! packages cannot be attributed), a path matching no
//! package and no rule, a component crate anywhere in the closure, and any
//! error at all reaching for git or the package graph. Most of xtask is *not*
//! such an input — only the transform tree that decides a verdict and the dist
//! builder whose artifacts the suites open by path are. See [`super::inputs`]
//! for where that line is drawn and what a tool change compiles instead.
//!
//! The one direction that does not widen is a diff that entered no crate at
//! all — [`Scope::Outside`]. There the whole tree is not the safe answer but
//! the expensive one: nothing a workspace run compiles differs from the base,
//! so it re-proves the base at the candidate's expense and lends it every
//! unrelated flake it meets. That scope is claimed only when every path in the
//! diff lies outside every workspace crate root, and the members that narrow to
//! a closure record an explicit empty-closure pass instead of running.
//!
//! The closure decides one thing besides the argv: whether `verify.test`'s
//! `cargo xtask dist` pre-build runs at all. That step cross-builds
//! every component package in its own cargo invocation, plus the behaviour
//! variants and the chassis binaries, and it is the single largest compile in
//! the lane — but what it produces is read only by tests that resolve a dist
//! artifact through the filesystem. Whether any crate in the closure does that
//! is the same question `cargo xtask affected` answers as
//! [`Selection::wasm_needed`](crate::affected::select::Selection::wasm_needed),
//! so the scope carries that answer through rather than deriving a second one.

use std::collections::BTreeSet;
use std::process::Command;
use std::slice::from_ref;

use anyhow::{Context, Result, bail};

use super::inputs::{self, SMOKE_PACKAGE, ToolChange};
use crate::affected::graph::Workspace;
use crate::affected::rules::global_screen;
use crate::transform::verify::lockfile;

/// How many of the diff's paths an [`Scope::Outside`] receipt names before it
/// says how many more there are. The reader needs to recognize the diff, not to
/// enumerate a docs sweep that moved two hundred files.
const MAX_NAMED_PATHS: usize = 12;

/// The lockfile path as the diff names it: the one workspace-level input the
/// scope attributes instead of widening on (issue #5951). A lockfile change is
/// a workspace-level input only for the crates whose resolved dependency set
/// actually moved, so the diff's moved packages verify their own
/// reverse-dependency closure unioned with the path-based one.
const LOCKFILE_PATH: &str = "Cargo.lock";

/// The crate set a member of the umbrella compiles over.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::transform) enum Scope {
    /// Every workspace crate, for the stated reason.
    Workspace {
        /// Why the closure was not trusted — or not asked for — stated in the
        /// run's own evidence so a fail-open is read rather than inferred.
        reason: String,
    },
    /// The candidate diff's reverse-dependency closure — unioned with the
    /// lockfile attribution's when the diff touched the lockfile.
    Closure {
        /// The crates the compiling members run over, sorted.
        packages: Vec<String>,
        /// The workspace crates they do not, sorted. The half of the receipt
        /// that makes a wrong closure legible: a crate that should have been
        /// reached appears here by name instead of vanishing.
        skipped: Vec<String>,
        /// Whether any crate in the closure resolves a `cargo xtask dist`
        /// artifact by filesystem path at test time — the selection's own
        /// `wasm_needed`, carried rather than recomputed so the lane and the
        /// gate it predicts cannot answer it two ways.
        wasm_needed: bool,
        /// What the lockfile half moved and reached, when the diff touched the
        /// lockfile — the attribution the receipt states alongside the closure
        /// so a narrowed lockfile run is read rather than inferred.
        /// `None` for a diff without one.
        lock: Option<LockAttribution>,
        /// Why this closure holds a crate the diff cannot reach, when it does —
        /// today only the smoke crate an xtask tool change adds. A package in
        /// the argv that the linkage story does not account for is exactly the
        /// kind of thing a reader must not have to guess at.
        note: Option<String>,
    },
    /// The candidate diff entered no workspace crate at all, so the compiling
    /// members have an empty closure and nothing of the candidate's to build.
    Outside {
        /// The diff's own paths, so the verdict is checkable rather than
        /// trusted: a reader confirms that none of them is a crate path.
        paths: Vec<String>,
    },
}

/// What the lockfile half of a narrowed run moved and reached.
#[derive(Debug, PartialEq, Eq)]
pub(in crate::transform) struct LockAttribution {
    /// The resolved packages whose version, source, or dependency set moved
    /// between the base and candidate lockfiles, sorted.
    changed: Vec<String>,
    /// The workspace crates depending on one of them, transitively, sorted.
    reached: Vec<String>,
}

impl LockAttribution {
    fn of(changed: &BTreeSet<String>, reached: &BTreeSet<String>) -> Self {
        Self { changed: changed.iter().cloned().collect(), reached: reached.iter().cloned().collect() }
    }

    /// The receipt line stating the attribution the way the scope line states
    /// the closure: which packages moved and which crates that reached.
    fn receipt_line(&self) -> String {
        if self.changed.is_empty() {
            return String::from(
                "lockfile: Cargo.lock moved no resolved package, so it reached no additional crate.\n",
            );
        }
        let moved = self.changed.join(" ");
        if self.reached.is_empty() {
            return format!(
                "lockfile: Cargo.lock moved package(s) ({}): {moved} reaching no workspace crate.\n",
                self.changed.len(),
            );
        }
        format!(
            "lockfile: Cargo.lock moved package(s) ({}): {moved} reaching crate(s) ({}): {}.\n",
            self.changed.len(),
            self.reached.len(),
            self.reached.join(" "),
        )
    }
}

impl Scope {
    /// The scope a run with this work-order diff base computes.
    ///
    /// Total by construction: every way the computation can fail — an
    /// unreadable diff, a package graph that will not build, a `cargo
    /// metadata` that will not run — resolves to the whole workspace with the
    /// error as its stated reason. A narrowing that guesses when it cannot
    /// compute is the false-green direction, and this is the one function
    /// deciding how much of the tree the gate looks at.
    pub(super) fn resolve(diff_base: Option<&str>) -> Self {
        Self::compute(diff_base)
            .unwrap_or_else(|error| Self::workspace(format!("the closure could not be computed — {error:#}")))
    }

    /// Every workspace crate, for `reason`.
    fn workspace(reason: impl Into<String>) -> Self {
        Self::Workspace { reason: reason.into() }
    }

    fn compute(diff_base: Option<&str>) -> Result<Self> {
        let Some(base) = diff_base else {
            return Ok(Self::workspace("the work order names no diff base, so this run has no candidate to narrow by"));
        };

        let changed = changed_paths(base)?;
        if changed.is_empty() {
            return Ok(Self::workspace(format!("the candidate diff against {base} is empty")));
        }

        Self::over_changed_at_base(base, &changed)
    }

    /// The scope a candidate diff of these paths computes — the whole decision
    /// past the git read, so it is exercisable against a stated diff.
    ///
    /// A diff naming the lockfile fail-opens here: without the base and
    /// candidate lockfile contents no moved package can be attributed, so the
    /// blunt workspace rule stands. The production path reads both contents
    /// and narrows through [`Self::over_changed_with_locks`].
    fn over_changed(changed: &[String]) -> Result<Self> {
        let tool = ToolChange::of(changed);
        if let ToolChange::Gate(path) = tool {
            return Ok(Self::workspace(format!("a workspace-level input changed: {path}")));
        }
        if let Some(hit) = outside_the_tool(changed) {
            return Ok(Self::workspace(format!("a workspace-level input changed: {hit}")));
        }

        let workspace = Workspace::load()?;
        let (extra, note) = smoke_check(&tool);
        Self::closure_over(&workspace, changed, changed, extra, None, note)
    }

    /// The scope a candidate diff against `base` computes — the lockfile-aware
    /// entry the production path takes once the diff is known to be non-empty.
    fn over_changed_at_base(base: &str, changed: &[String]) -> Result<Self> {
        if !changed.iter().any(|path| path.as_str() == LOCKFILE_PATH) {
            return Self::over_changed(changed);
        }

        let base_lock = match lockfile_at(base) {
            Ok(text) => text,
            Err(error) => {
                return Ok(Self::workspace(format!(
                    "Cargo.lock changed but the base lockfile could not be read for attribution — {error:#}"
                )));
            }
        };
        let candidate_lock = match lockfile_at("HEAD") {
            Ok(text) => text,
            Err(error) => {
                return Ok(Self::workspace(format!(
                    "Cargo.lock changed but the candidate lockfile could not be read for attribution — {error:#}"
                )));
            }
        };

        Self::over_changed_with_locks(changed, &base_lock, &candidate_lock)
    }

    /// The scope a candidate diff touching the lockfile computes from the base
    /// and candidate lockfile contents — the narrowed rule, exercisable
    /// against fixture lockfiles.
    ///
    /// The moved externals verify their reverse-dependency closure over the
    /// package graph, unioned with the path-based closure of every other
    /// changed path. Only an unattributable lockfile widens the run: bytes no
    /// parser accepts, or a workspace member's own identity moving.
    fn over_changed_with_locks(changed: &[String], base_lock: &str, candidate_lock: &str) -> Result<Self> {
        let tool = ToolChange::of(changed);
        if let ToolChange::Gate(path) = tool {
            return Ok(Self::workspace(format!("a workspace-level input changed: {path}")));
        }

        let workspace = Workspace::load()?;

        let members = workspace.members();
        let attributed = match lockfile::diff(base_lock, candidate_lock, &members) {
            Ok(attributed) => attributed,
            Err(error) => {
                return Ok(Self::workspace(format!(
                    "Cargo.lock changed but its resolved packages could not be attributed — {error:#}"
                )));
            }
        };
        if !attributed.members_changed.is_empty() {
            let moved: Vec<&str> = attributed.members_changed.iter().map(String::as_str).collect();
            return Ok(Self::workspace(format!("Cargo.lock changed workspace-member package(s): {}", moved.join(" "))));
        }

        let reached = match workspace.reverse_closure_of_external(&attributed.changed) {
            Ok(reached) => reached,
            Err(error) => {
                let moved: Vec<&str> = attributed.changed.iter().map(String::as_str).collect();
                return Ok(Self::workspace(format!(
                    "Cargo.lock moved package(s) {} but their dependents could not be computed — {error:#}",
                    moved.join(" "),
                )));
            }
        };

        let rest: Vec<String> = changed.iter().filter(|path| path.as_str() != LOCKFILE_PATH).cloned().collect();
        if let Some(hit) = outside_the_tool(&rest) {
            return Ok(Self::workspace(format!("a workspace-level input changed: {hit}")));
        }

        let attribution = LockAttribution::of(&attributed.changed, &reached);
        let (mut extra, note) = smoke_check(&tool);
        extra.extend(reached);
        Self::closure_over(&workspace, &rest, changed, extra, Some(attribution), note)
    }

    /// The reverse-dependency closure over `resolved` unioned with the `extra`
    /// packages the lockfile attribution reached and the smoke crate a tool
    /// change adds, carrying `lock` and `note` for the receipt when either
    /// half of the widening story needs stating.
    fn closure_over(
        workspace: &Workspace,
        resolved: &[String],
        named: &[String],
        extra: BTreeSet<String>,
        lock: Option<LockAttribution>,
        note: Option<String>,
    ) -> Result<Self> {
        let selection = workspace.select(resolved)?;
        if let Some(reason) = selection.run_all {
            return Ok(Self::workspace(reason));
        }

        let mut packages = selection.packages;
        packages.extend(extra);
        if packages.is_empty() {
            return Ok(Self::outside(named, &workspace.crate_roots()));
        }
        if let Some(source) = wasm_source_in(&packages, workspace.wasm_sources()) {
            return Ok(Self::workspace(format!(
                "{source} compiles to component wasm, which tests load by path rather than by linkage"
            )));
        }

        let members = workspace.members();
        let skipped = members.difference(&packages).cloned().collect();
        let wasm_needed = packages.iter().any(|name| workspace.needs_dist_prepare(name));
        Ok(Self::Closure { packages: packages.into_iter().collect(), skipped, wasm_needed, lock, note })
    }

    /// The scope a diff the path rules resolved to no package computes.
    ///
    /// A closure of nothing is a diff that reached no crate — prose, agent
    /// state, a non-`ci.yml` workflow. Compiling the whole tree for it is a
    /// fail-open in the expensive direction and only that: the crates a
    /// workspace run would build are byte-for-byte the base's, so the suite
    /// re-proves the base at the candidate's expense and lends the candidate
    /// every unrelated flake it meets on the way. The gate this lane predicts
    /// already declines that work — CI's pull-request lane skips its test steps
    /// outright on an empty affected set — so the empty case states an empty
    /// closure rather than the widest possible one.
    ///
    /// What the empty selection does *not* prove on its own is that the diff
    /// stayed outside the crates. The path rules also deselect a few files that
    /// live inside one (`README*`, `LICENSE*`, `.gitignore`), and such a file
    /// can be `include_str!`d into a crate that compiles it. So a path under any
    /// crate root takes the whole tree with its reason stated, and the empty
    /// closure is claimed only for a diff every path of which is outside every
    /// crate root.
    fn outside(changed: &[String], crate_roots: &BTreeSet<String>) -> Self {
        changed.iter().find(|path| crate_roots.iter().any(|root| path.starts_with(root))).map_or_else(
            || Self::Outside { paths: changed.to_vec() },
            |inside| {
                Self::workspace(format!(
                    "{inside} sits inside a crate root and the path rules resolved it to no package, so what it \
                     can reach is unstated"
                ))
            },
        )
    }

    /// The crates a scoped member narrows to, or `None` when this run compiles
    /// the whole workspace.
    ///
    /// [`Self::Outside`] answers `None` with the rest: it names no crates to
    /// compile, and a member that reached a command under it anyway must run
    /// the stated workspace argv rather than one with `--workspace` traded for
    /// no `-p` at all, which cargo reads as the whole workspace regardless.
    pub(in crate::transform) fn packages(&self) -> Option<&[String]> {
        match self {
            Self::Workspace { .. } | Self::Outside { .. } => None,
            Self::Closure { packages, .. } => Some(packages),
        }
    }

    /// Whether the tests this run selects read a `cargo xtask dist` artifact,
    /// and so whether `verify.test`'s pre-build has anything to produce for
    /// them.
    ///
    /// A workspace run says yes for the same reason it compiles everything: it
    /// selects the suites that load component wasm and fork the dist-resolved
    /// chassis binaries, so the pre-build is theirs. A closure says what its
    /// selection said — a crate resolving a dist artifact by path is a
    /// structural property of its dependency list, recomputed from the
    /// workspace sources on every push by the `affected::invariants` module's
    /// `dist_consumers` scan rather than trusted to a list here.
    ///
    /// Declining the pre-build is the narrow direction, so it is worth naming
    /// what happens when this is wrong. `verify.test` runs under
    /// `AETHER_REQUIRE_RUNTIME=1`, which turns a missing artifact into a failed
    /// test rather than a silent skip — a misclassification is loud on the next
    /// run, never a candidate integrating on a suite that quietly did not
    /// execute.
    pub(super) fn wasm_needed(&self) -> bool {
        match self {
            Self::Workspace { .. } => true,
            Self::Closure { wasm_needed, .. } => *wasm_needed,
            // A diff that entered no crate compiles nothing; the compiling
            // members record the empty-closure pass before any prepare runs.
            Self::Outside { .. } => false,
        }
    }

    /// Whether a diagnostic this run's compiler emitted about `package` is a
    /// statement about the candidate.
    ///
    /// A workspace run judges everything it compiled. A closure run judges the
    /// crates the diff can have broken — by construction every crate it changed
    /// plus everything linking one — and not the crates underneath them. A
    /// dependency the candidate never touched compiles the same source it
    /// compiles at the base, so a warning in it is a standing property of the
    /// tree rather than a finding about this candidate (#5411).
    ///
    /// Narrowing the package selection is what makes those diagnostics appear
    /// at all. Cargo unifies features across the packages one invocation
    /// selects, so the whole-workspace build the CI gate and `verify.base` run
    /// turns on every feature some member's dev-dependency asks for, while a
    /// closure that leaves that member out compiles the crate underneath it
    /// feature-poor — and an item the missing feature gates is then an unused
    /// one. Judging that is a member door red on a tree the base door passed
    /// minutes earlier, which is the failure this predicate exists to stop.
    pub(in crate::transform) fn judges(&self, package: &str) -> bool {
        match self {
            Self::Workspace { .. } | Self::Outside { .. } => true,
            Self::Closure { packages, .. } => packages.iter().any(|name| name == package),
        }
    }

    /// The receipt this run writes alongside its members' logs: what the
    /// compiling gates ran over, and what they did not.
    ///
    /// Both halves, because only the pair makes a wrong closure visible. A
    /// selection that silently lost a crate reads as an ordinary pass unless
    /// the run states which crates it declined to look at.
    pub(in crate::transform) fn receipt(&self) -> String {
        match self {
            Self::Workspace { reason } => format!("verify scope: every workspace crate — {reason}\n"),
            Self::Closure { packages, skipped, wasm_needed, lock, note } => {
                let mut receipt = format!(
                    "verify scope: the candidate diff's reverse-dependency closure.\n\
                     crates in ({}): {}\ncrates skipped ({}): {}\ndist pre-build: {}\n",
                    packages.len(),
                    packages.join(" "),
                    skipped.len(),
                    skipped.join(" "),
                    if *wasm_needed {
                        "needed — a crate in the closure resolves a dist artifact by path"
                    } else {
                        "not needed — no crate in the closure resolves a dist artifact by path"
                    },
                );
                if let Some(note) = note {
                    receipt.push_str("smoke check: ");
                    receipt.push_str(note);
                    receipt.push('\n');
                }
                if let Some(attribution) = lock {
                    receipt.push_str(&attribution.receipt_line());
                }
                receipt
            }
            Self::Outside { paths } => format!(
                "verify scope: no workspace crate — every one of the candidate diff's {} path(s) lies outside \
                 every crate root, so the closure is empty and the compiling members record a pass without \
                 running.\npaths ({}): {}\n",
                paths.len(),
                paths.len(),
                named(paths),
            ),
        }
    }

    /// The line a scoped member's own log opens with, so a reader who opens
    /// `verify.clippy.log` learns what it looked at without correlating files.
    /// `None` for a workspace run, whose log needs no qualification, and for an
    /// empty closure, whose members write [`Self::empty_closure_verdict`]
    /// instead of a log to qualify.
    pub(super) fn member_notice(&self) -> Option<String> {
        let Self::Closure { packages, skipped, .. } = self else {
            return None;
        };
        Some(format!(
            "note: scoped to {} of {} workspace crates — the candidate diff's reverse-dependency closure \
             (see verify.scope.log)\n",
            packages.len(),
            packages.len() + skipped.len(),
        ))
    }

    /// The whole log a compiling member writes when this run's closure is
    /// empty, or `None` when there is work for it to do.
    ///
    /// Stated at length because it is the only thing standing where a member's
    /// output would be, and a pass nobody can account for is worse than the
    /// full-suite run it replaces. It names the verdict, why the member did not
    /// run, and what the pass rests on: the crates it would have compiled are
    /// the base's own, unchanged by a diff that never entered one.
    pub(super) fn empty_closure_verdict(&self) -> Option<String> {
        let Self::Outside { paths } = self else {
            return None;
        };
        Some(format!(
            "pass: not run — no workspace crate in the diff; the closure this member compiles is empty.\n\
             Every one of the candidate's {} changed path(s) lies outside every workspace crate root, so the \
             crates this member would build are byte-for-byte the base's and this run would re-prove the base \
             rather than judge the candidate. Nothing was compiled and no test was executed (see \
             verify.scope.log).\npaths ({}): {}\n",
            paths.len(),
            paths.len(),
            named(paths),
        ))
    }
}

#[cfg(test)]
impl Scope {
    /// The scope of a diff that entered no crate, for exercising the members'
    /// empty-closure verdict without a git repository or a package graph.
    pub(super) fn outside_of(paths: &[&str]) -> Self {
        Self::Outside { paths: paths.iter().map(|path| (*path).to_owned()).collect() }
    }
}

/// A bounded rendering of a diff's paths: the first [`MAX_NAMED_PATHS`], then a
/// count of what was left out. A silently truncated list would make a diff that
/// touched something look like one that did not.
fn named(paths: &[String]) -> String {
    let listed: Vec<&str> = paths.iter().take(MAX_NAMED_PATHS).map(String::as_str).collect();
    let omitted = paths.len() - listed.len();
    let list = listed.join(" ");
    if omitted == 0 {
        return list;
    }
    format!("{list}, and {omitted} more")
}

/// The paths `base..HEAD` changed — the member candidate's own diff, which is
/// committed by the time the mechanical lane sees it.
fn changed_paths(base: &str) -> Result<Vec<String>> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "-z", base, "HEAD"])
        .output()
        .with_context(|| format!("spawn git diff {base}..HEAD"))?;
    if !output.status.success() {
        bail!("git diff {base}..HEAD failed ({}): {}", output.status, String::from_utf8_lossy(&output.stderr).trim());
    }

    Ok(String::from_utf8(output.stdout)
        .context("git diff produced non-UTF-8 output")?
        .split('\0')
        .filter(|path| !path.is_empty())
        .map(str::to_owned)
        .collect())
}

/// The `Cargo.lock` revision `revision` names — the base and candidate inputs
/// the attribution diffs. The candidate is the committed `HEAD`, matching the
/// committed `base..HEAD` range the paths came from.
fn lockfile_at(revision: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["show", &format!("{revision}:Cargo.lock")])
        .output()
        .with_context(|| format!("spawn git show {revision}:Cargo.lock"))?;
    if !output.status.success() {
        bail!(
            "git show {revision}:Cargo.lock failed ({}): {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    String::from_utf8(output.stdout)
        .with_context(|| format!("git show {revision}:Cargo.lock produced non-UTF-8 output"))
}

/// The smoke crate and the receipt note a tool change joins the closure with,
/// or nothing at all for a diff that did not change the tool (#6001).
///
/// A tool change proves itself by compiling xtask's own closure and then
/// running each gate once over one real crate — the smoke check that says the
/// tool still drives a gate end to end.
fn smoke_check(tool: &ToolChange<'_>) -> (BTreeSet<String>, Option<String>) {
    match *tool {
        ToolChange::Tool(path) => (
            BTreeSet::from([SMOKE_PACKAGE.to_owned()]),
            Some(format!(
                "{path} changed the tool the gates run through rather than the gate code, so {SMOKE_PACKAGE} \
                 joins the closure as one run of each gate over a real crate"
            )),
        ),
        ToolChange::Gate(_) | ToolChange::Ordinary => (BTreeSet::new(), None),
    }
}

/// The selection lane's run-everything screen, asked of every path in the diff
/// except the tool's own sources.
///
/// That screen names several xtask paths — the selection machinery, the dist
/// builder, the binary entry — because a change to them moves which tests CI
/// selects. This lane has already decided that question for xtask on its own
/// terms ([`ToolChange`]), so re-asking the shared screen about the same paths
/// would take the widening back one line after declining it.
fn outside_the_tool(changed: &[String]) -> Option<&str> {
    changed.iter().filter(|path| !inputs::is_tool(path)).find_map(|path| global_screen(from_ref(path)))
}

/// The first component crate in `packages`, when the closure reached one.
///
/// The one coupling linkage cannot express (issue #439, ADR-0067): a component
/// crate compiles to a `.wasm` that scenario tests load through the
/// filesystem, so the crates a change to it can break are not its dependents —
/// they link nothing against it at all. Any component crate in the closure
/// means the built wasm moves, and which tests read it is exactly what the
/// package graph cannot name.
fn wasm_source_in<'a>(packages: &'a BTreeSet<String>, wasm_sources: &BTreeSet<String>) -> Option<&'a String> {
    packages.iter().find(|package| wasm_sources.contains(*package))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{SMOKE_PACKAGE, Scope, outside_the_tool, wasm_source_in};
    use crate::affected::graph::Workspace;

    /// The crate the two-hop tripwire starts from: a foundational, widely
    /// depended-on leaf whose dependents themselves have dependents.
    const CLOSURE_ROOT: &str = "aether-math";

    fn strings(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|path| (*path).to_owned()).collect()
    }

    fn string_set(names: &[&str]) -> BTreeSet<String> {
        names.iter().map(|name| (*name).to_owned()).collect()
    }

    /// Direct workspace dependents of each named crate, read from the same
    /// manifests the graph is built from, so the two-hop chain the tripwire
    /// walks is derived rather than pinned to a crate pairing that can move.
    fn direct_dependents(of: &BTreeSet<String>) -> BTreeSet<String> {
        let metadata =
            cargo_metadata::MetadataCommand::new().no_deps().exec().expect("run cargo metadata for the workspace");
        metadata
            .packages
            .iter()
            .filter(|package| package.dependencies.iter().any(|dependency| of.contains(&dependency.name)))
            .map(|package| package.name.to_string())
            .filter(|name| !of.contains(name))
            .collect()
    }

    #[test]
    fn the_closure_reaches_a_reverse_dependency_two_hops_up() {
        // Tripwire for the premise the whole narrowing rests on (#4890): a
        // linkage closure is only a sound over-approximation if it is
        // transitive. A closure that stopped at direct dependents would skip
        // the crate that links the crate that links the change — which
        // compiles the changed code just the same, and whose tests would then
        // pass by never having been run over it.
        let root = string_set(&[CLOSURE_ROOT]);
        let one_hop = direct_dependents(&root);
        let two_hops: BTreeSet<String> =
            direct_dependents(&one_hop).difference(&one_hop).filter(|name| !root.contains(*name)).cloned().collect();
        assert!(!one_hop.is_empty(), "{CLOSURE_ROOT} must have direct workspace dependents to walk from");
        assert!(!two_hops.is_empty(), "{CLOSURE_ROOT}'s dependents must themselves have dependents");

        let scope = Scope::over_changed(&strings(&[&format!("crates/{CLOSURE_ROOT}/src/lib.rs")]))
            .expect("compute the closure over a leaf-crate source change");
        // A foundational crate's closure reaches component crates, which fail
        // open by design — so the reachability assertion is made against the
        // selection the scope computes from, not against a narrowed Scope.
        let workspace = Workspace::load().expect("load the workspace graph");
        let selection = workspace
            .select(&strings(&[&format!("crates/{CLOSURE_ROOT}/src/lib.rs")]))
            .expect("select over a leaf-crate source change");

        assert!(selection.run_all.is_none(), "a crate source change must reach the graph analysis");
        for crate_name in &two_hops {
            assert!(
                selection.packages.contains(crate_name),
                "{crate_name} links {CLOSURE_ROOT} two hops up and must be in the closure",
            );
        }
        assert_eq!(scope.packages(), None, "a foundational crate reaches component wasm, which fails open");
    }

    #[test]
    fn a_near_leaf_change_narrows_to_its_own_closure() {
        // Tripwire for acceptance 1: the payoff case. A member confined to a
        // near-leaf crate must compile that crate's reverse-dependency closure
        // and nothing else — if this ever widens back to the whole workspace,
        // the narrowing has stopped paying for itself and the receipt is the
        // only place that would say so.
        let scope = Scope::over_changed(&strings(&[DIST_FREE_LEAF_SOURCE]))
            .expect("compute the closure over a near-leaf change");

        let packages = scope.packages().expect("a near-leaf change narrows");
        assert!(packages.contains(&DIST_FREE_LEAF.to_owned()), "the changed crate is in its own closure");
        let Scope::Closure { skipped, .. } = &scope else {
            unreachable!("packages() already proved the closure arm")
        };
        assert!(!skipped.is_empty(), "a near-leaf change must skip something, or nothing was narrowed");
        assert!(
            skipped.iter().any(|name| name == "aether-math"),
            "a crate the change cannot reach must be skipped: {skipped:?}",
        );
    }

    /// A crate that resolves a `cargo xtask dist` artifact by filesystem path,
    /// and whose own reverse-dependency closure reaches no component crate — so
    /// the scope narrows instead of failing open and the pre-build question is
    /// actually asked. Derived rather than assumed: the test below refuses to
    /// pass on a narrowing that never happened.
    ///
    /// Was `aether-chassis-desktop` until this crate stopped shipping
    /// `aether-harness-substrate` inside every chassis binary: desktop
    /// qualified only through that normal dependency, and once it went the
    /// remaining deps name no harness and no wasm source, so desktop's closure
    /// correctly stopped asking for the pre-build. `aether-chassis-hub` states
    /// the same shape honestly — it dev-deps `aether-harness-fleet`, which
    /// forks the dist-resolved chassis binary through `dist/manifest.json`.
    const DIST_CONSUMING_LEAF: &str = "aether-chassis-hub";
    const DIST_CONSUMING_LEAF_SOURCE: &str = "crates/aether-chassis-hub/src/lib.rs";

    /// A crate whose closure reads nothing the pre-build produces. `aether-mcp`
    /// is a leaf — nothing in the workspace depends on it — and neither it nor
    /// anything in its closure names a wasm source or a dist-resolving harness.
    const DIST_FREE_LEAF: &str = "aether-mcp";
    const DIST_FREE_LEAF_SOURCE: &str = "crates/aether-mcp/src/rpc.rs";

    #[test]
    fn the_dist_prebuild_follows_the_closure_rather_than_the_whole_tree() {
        // Tripwire. `cargo xtask dist` cross-builds thirteen
        // component packages in thirteen cargo invocations plus the chassis
        // binaries, and `verify.test` ran it before every member however narrow
        // — for a coordinator-side candidate, minutes of cross-build producing
        // wasm that no test in the closure opens. Both directions are the
        // invariant. A closure that stops declining the pre-build puts the cost
        // back; a closure that starts declining one it needs leaves a
        // dist-resolving test with no artifact, and `AETHER_REQUIRE_RUNTIME=1`
        // turns that into a red member on a candidate that did nothing wrong.
        let free = Scope::over_changed(&strings(&[DIST_FREE_LEAF_SOURCE]))
            .expect("compute the closure over a dist-free change");
        let consuming = Scope::over_changed(&strings(&[DIST_CONSUMING_LEAF_SOURCE]))
            .expect("compute the closure over a dist-resolving change");

        assert!(free.packages().is_some(), "{DIST_FREE_LEAF} must narrow, or the question is never asked");
        assert!(consuming.packages().is_some(), "{DIST_CONSUMING_LEAF} must narrow, or the question is never asked");
        assert!(
            !free.wasm_needed(),
            "no crate in {DIST_FREE_LEAF}'s closure opens a dist artifact: {}",
            free.receipt()
        );
        assert!(
            consuming.wasm_needed(),
            "{DIST_CONSUMING_LEAF} resolves a dist artifact by path and its closure must say so: {}",
            consuming.receipt(),
        );

        // The whole tree keeps the pre-build for the same reason it keeps the
        // whole argv: it selects the suites that load component wasm and fork
        // the dist-resolved chassis binaries.
        assert!(Scope::resolve(None).wasm_needed(), "a workspace run must pre-build");

        // Both halves stated where the reader is, so a run that skipped the
        // cross-build is read rather than inferred from a missing log.
        assert!(free.receipt().contains("dist pre-build: not needed"), "{}", free.receipt());
        assert!(consuming.receipt().contains("dist pre-build: needed"), "{}", consuming.receipt());
    }

    #[test]
    fn a_workspace_level_input_runs_the_whole_workspace() {
        // Tripwire for acceptance 2. Each of these reshapes the build graph,
        // the lint configuration, or the selection machinery itself, so a
        // closure computed from it is not a statement about what the change
        // can reach. A missed entry is a narrowed run whose premise no longer
        // holds — the exact false green the narrowing is only permitted to
        // exist without.
        //
        // `Cargo.lock` stays in this list for the paths-only entry: without
        // the base and candidate contents no moved package can be attributed,
        // so the blunt rule stands there. An attributable lockfile diff
        // narrows through `over_changed_with_locks`, covered below.
        //
        // The three xtask entries are the whole of what #6055 left on this
        // side: the transform tree that decides a member's verdict, the dist
        // builder, and its `build-wasm` front. Everything else under
        // `xtask/src/` narrows — the test below holds that half.
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rustfmt.toml",
            "clippy.toml",
            "rust-toolchain.toml",
            ".config/nextest.toml",
            ".cargo/config.toml",
            "xtask/src/transform/verify/mod.rs",
            "xtask/src/transform/mod.rs",
            "xtask/src/dist/mod.rs",
            "xtask/src/build_wasm.rs",
            ".github/workflows/ci.yml",
        ] {
            let scope = Scope::over_changed(&strings(&[path])).expect("screen the changed path");
            assert_eq!(scope.packages(), None, "{path} must run the whole workspace");
            assert!(scope.receipt().contains(path), "the receipt names what forced it: {}", scope.receipt());
            assert!(scope.wasm_needed(), "{path} keeps the dist pre-build the whole tree needs");
        }

        assert!(
            outside_the_tool(&strings(&["crates/aether-math/src/lib.rs"])).is_none(),
            "an ordinary crate source must not be screened",
        );
    }

    #[test]
    fn a_change_to_the_tool_around_the_gate_compiles_xtask_and_the_smoke_crate() {
        // Tripwire for #6001. Nothing in the workspace links xtask, so the only
        // thing that ever widened a tool change to sixty crates was xtask being
        // what runs the gates — true of the gate code, not of the selection
        // graph beside it. Both directions matter: a rule that stops narrowing
        // puts the 13-to-21-minute runs back, and one that starts narrowing the
        // *gate* code lets a change to the scope computation prove itself on
        // the crates it chose to look at.
        let tool = Scope::over_changed(&strings(&["xtask/src/affected/graph.rs"]))
            .expect("compute the scope over a tool change");

        let packages = tool.packages().expect("a tool change narrows");
        assert!(packages.contains(&"xtask".to_owned()), "the tool's own crate is compiled: {packages:?}");
        assert!(packages.contains(&SMOKE_PACKAGE.to_owned()), "and one real crate is run through each gate");
        assert!(
            packages.contains(&"aether-kinds".to_owned()),
            "and the workspace-scanning package every narrowed selection carries (#6408): {packages:?}"
        );
        assert_eq!(packages.len(), 3, "and nothing else: {packages:?}");
        assert!(tool.receipt().contains("smoke check:"), "the receipt accounts for it: {}", tool.receipt());

        let gate = Scope::over_changed(&strings(&["xtask/src/transform/verify/scope.rs"]))
            .expect("compute the scope over a gate change");
        assert_eq!(gate.packages(), None, "the gate code still runs every crate: {}", gate.receipt());
    }

    #[test]
    fn a_bloom_cli_change_narrows_to_xtask_without_a_dist_prepare() {
        // Tripwire for #6055, and for the pair of facts that make the narrowing
        // sound rather than convenient. Nothing in the workspace links xtask, so
        // the operator CLI's reverse-dependency closure is xtask itself; and
        // nothing under `bloom/` builds an artifact a test opens by path, so the
        // `cargo xtask dist` prepare has nothing to produce for the run. Reading
        // it as a workspace-level input instead ran 6871 tests across 301
        // binaries plus that prepare — the 15-to-18-minute shared runs beside
        // 5.5-minute closure-scoped ones.
        //
        // The receipt is asserted alongside the argv because the widening was
        // only ever legible through it: the run that took the whole tree said so
        // in one line naming the path that forced it, and a regression here
        // would put that line back.
        let scope =
            Scope::over_changed(&strings(&["xtask/src/bloom/mod.rs"])).expect("compute the scope over a CLI change");

        let packages = scope.packages().expect("the operator CLI narrows");
        assert!(packages.contains(&"xtask".to_owned()), "the CLI's own crate is compiled: {packages:?}");
        assert!(!scope.wasm_needed(), "nothing under bloom/ feeds the dist prepare: {}", scope.receipt());
        assert!(scope.receipt().contains("dist pre-build: not needed"), "{}", scope.receipt());
        assert!(
            !scope.receipt().contains("a workspace-level input changed"),
            "the CLI is not a workspace-level input: {}",
            scope.receipt(),
        );

        // The half that must not move with it. A verify-transform change still
        // takes the whole tree, and still names the path that forced it, so the
        // one line a reader greps for keeps meaning what it meant.
        let gate = Scope::over_changed(&strings(&["xtask/src/transform/verify/mod.rs"]))
            .expect("compute the scope over a verify-transform change");
        assert_eq!(gate.packages(), None, "the verify transforms keep every crate");
        assert!(gate.wasm_needed(), "and the prepare that feeds the suites they run");
        assert!(
            gate.receipt()
                .contains("every workspace crate — a workspace-level input changed: xtask/src/transform/verify/mod.rs"),
            "{}",
            gate.receipt(),
        );
    }

    #[test]
    fn a_lockfile_bump_to_a_single_crate_dependency_scopes_to_that_crates_closure() {
        // Tripwire for the payoff issue #5951 exists for: a lockfile-only bump
        // must verify the moved package's reverse-dependency closure, not the
        // whole workspace. `notify` is depended on by exactly one workspace
        // crate, so its bump is the crisp case — a return to the blunt rule
        // shows up as this test widening. If the tree moves and `notify` gains
        // dependents or wasm reach, pick another single-user external rather
        // than weakening this.
        let base = "version = 4\n\n[[package]]\nname = \"notify\"\nversion = \"8.2.0\"\n\
                    source = \"registry+https://github.com/rust-lang/crates.io-index\"\nchecksum = \"8.2.0\"\n";
        let candidate = base.replace("8.2.0", "8.2.1");

        let scope = Scope::over_changed_with_locks(&strings(&["Cargo.lock"]), base, &candidate)
            .expect("attribute a single-package bump");

        let workspace = Workspace::load().expect("load the workspace graph");
        let expected: Vec<String> = workspace
            .reverse_closure_of_external(&string_set(&["notify"]))
            .expect("attribute notify")
            .into_iter()
            .collect();
        assert!(
            !expected.is_empty() && expected.len() < workspace.members().len(),
            "notify must reach a proper non-empty closure, or this fixture no longer exercises narrowing",
        );

        let packages = scope.packages().expect("an attributable bump narrows").to_vec();
        assert_eq!(packages, expected, "a bump scopes to its dependents' closure");
        let first_reached = expected.first().expect("the closure is non-empty by the assertion above");
        assert!(
            scope.receipt().contains("notify") && scope.receipt().contains(first_reached),
            "the receipt states which package moved and which crate that reached: {}",
            scope.receipt(),
        );
    }

    #[test]
    fn an_unparseable_lockfile_widens_to_every_crate() {
        // Tripwire for the fail-open half of issue #5951: attribution that
        // guessed over bytes it could not parse would narrow a run whose
        // inputs it never saw — the false-green direction. A lockfile the
        // parser rejects runs the whole workspace with the reason stated.
        let scope = Scope::over_changed_with_locks(&strings(&["Cargo.lock"]), "version = 4\n", "not a lockfile [[[\n")
            .expect("an unparseable lockfile still resolves");

        assert_eq!(scope.packages(), None, "an unattributable lockfile runs the whole workspace");
        assert!(scope.receipt().contains("Cargo.lock"), "the receipt names what forced it: {}", scope.receipt());
    }

    #[test]
    fn a_lockfile_change_moving_no_package_names_no_crate_to_compile() {
        // Tripwire for the seam an empty union would fall through: a lockfile
        // diff that moved no resolved package and arrived with no path input
        // must resolve the empty closure, never a `Closure` over zero crates —
        // the narrowing seam trades `--workspace` for one `-p` per crate, and
        // zero `-p`s reads as the engine default rather than as nothing.
        let lock = "version = 4\n";
        let scope = Scope::over_changed_with_locks(&strings(&["Cargo.lock"]), lock, lock)
            .expect("resolve a lockfile diff that moved nothing");

        assert!(matches!(scope, Scope::Outside { .. }), "a move-nothing lockfile diff reaches no crate: {scope:?}");
        assert!(scope.empty_closure_verdict().is_some(), "the compiling members record a verdict rather than run");
    }

    #[test]
    fn an_absent_diff_base_leaves_the_whole_workspace() {
        // Tripwire for acceptance 3: the aggregate verify and every hand-run
        // `cargo xtask transform verify.check` name no diff base, and that is
        // the only thing keeping their invocation byte-for-byte what it was. A
        // narrowing that defaulted to on would silently move the stage that
        // proves the landing.
        let scope = Scope::resolve(None);

        assert_eq!(scope.packages(), None, "no diff base narrows nothing");
        assert!(scope.receipt().contains("every workspace crate"));
        assert_eq!(scope.member_notice(), None, "a workspace run needs no qualification in its members' logs");
    }

    #[test]
    fn an_unresolvable_diff_base_fails_open_rather_than_narrowing() {
        // Tripwire: the resolution is total. Reaching for a base git cannot
        // resolve must widen the run, never narrow it to whatever partial
        // answer the failure left behind.
        let scope = Scope::resolve(Some("0000000000000000000000000000000000000000"));

        assert_eq!(scope.packages(), None);
        assert!(scope.receipt().contains("could not be computed"), "{}", scope.receipt());
    }

    #[test]
    fn a_diff_that_entered_no_crate_resolves_an_empty_closure() {
        // Tripwire for the fail-open this arm closed: a docs-only member and a
        // non-`ci.yml` workflow member each resolved to no package and were
        // handed the whole workspace, which compiled ninety-six crates and ran
        // the entire suite over a diff that could not move any of it. The
        // expensive direction is not the safe one here — it re-proves the base
        // at the candidate's expense and lends the candidate every unrelated
        // flake the suite meets on the way.
        let scope = Scope::over_changed(&strings(&["docs/adr/0200-verification-is-a-ledger-of-proof-facts.md"]))
            .expect("resolve a docs-only diff");

        assert!(
            matches!(scope, Scope::Outside { .. }),
            "a docs-only diff must not take the whole workspace: {scope:?}"
        );
        assert_eq!(scope.packages(), None, "an empty closure names no crate to compile");
        assert!(scope.empty_closure_verdict().is_some(), "the compiling members record a verdict rather than run");
        assert!(scope.receipt().contains("no workspace crate"), "{}", scope.receipt());
        assert!(
            scope.receipt().contains("docs/adr/0200"),
            "the receipt names the diff it rests on: {}",
            scope.receipt()
        );

        let workflow = Scope::over_changed(&strings(&[".github/workflows/perf-compare.yml"]))
            .expect("resolve a non-ci.yml workflow diff");
        assert!(matches!(workflow, Scope::Outside { .. }), "a workflow that is not ci.yml reaches no crate");
    }

    #[test]
    fn only_a_diff_outside_every_crate_root_may_skip_the_compiling_members() {
        // Tripwire for the direction that would let a code change dodge its
        // tests. The empty-closure pass rests on the compiled content being
        // byte-for-byte the base's, which holds only for a diff that never
        // entered a crate — so a path *inside* a crate root that the path rules
        // happened to deselect (`README*`, `LICENSE*`, `.gitignore`, any rule
        // added later) must widen the run instead of emptying it. Such a file
        // can be `include_str!`d into the crate that ships it.
        let inside = Scope::over_changed(&strings(&["crates/aether-math/README.md"]))
            .expect("resolve a diff inside a crate root");

        assert!(inside.empty_closure_verdict().is_none(), "a path inside a crate root must not empty the closure");
        assert!(inside.receipt().contains("every workspace crate"), "{}", inside.receipt());
        assert!(inside.receipt().contains("crates/aether-math/README.md"), "{}", inside.receipt());

        // The two scopes that do name work keep it: a narrowed run compiles its
        // closure and a workspace run compiles the tree, and neither may answer
        // with a verdict nothing ran for.
        let closure = Scope::over_changed(&strings(&[DIST_FREE_LEAF_SOURCE]))
            .expect("compute the closure over a near-leaf change");
        assert!(closure.empty_closure_verdict().is_none(), "a crate source change must still be compiled and tested");
        assert!(Scope::resolve(None).empty_closure_verdict().is_none(), "a workspace run must still run");
    }

    #[test]
    fn a_component_crate_in_the_closure_runs_the_whole_workspace() {
        // Tripwire for the coupling linkage cannot express: a component crate
        // compiles to a `.wasm` scenario tests load through the filesystem, so
        // its reverse-dependency closure names none of the tests a change to
        // it can break. Narrowing there runs strictly fewer tests than the
        // gate it predicts, over exactly the crates whose coupling is
        // invisible to the graph.
        let workspace = Workspace::load().expect("load the workspace graph");
        let source = workspace.wasm_sources().iter().next().cloned().expect("the workspace builds component wasm");

        assert_eq!(wasm_source_in(&string_set(&[&source]), workspace.wasm_sources()), Some(&source));
        assert_eq!(
            wasm_source_in(&string_set(&["aether-math"]), workspace.wasm_sources()),
            None,
            "a crate that compiles to no wasm must not widen the run",
        );
    }

    #[test]
    fn the_receipt_names_both_the_closure_and_what_it_skipped() {
        // Tripwire for acceptance 1's second half: a wrong closure is only
        // visible if the run states which crates it declined to look at. A
        // receipt naming the selection alone reads as a clean pass whether or
        // not the selection lost something.
        let scope = Scope::over_changed(&strings(&[DIST_FREE_LEAF_SOURCE])).expect("compute the closure");
        let receipt = scope.receipt();

        assert!(receipt.contains("crates in ("), "{receipt}");
        assert!(receipt.contains("crates skipped ("), "{receipt}");
        assert!(receipt.contains(DIST_FREE_LEAF), "the changed crate is named: {receipt}");
        assert!(scope.member_notice().expect("a scoped run qualifies its members' logs").contains("workspace crates"));
    }
}
