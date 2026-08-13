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
//! enumerated rather than assumed: a workspace-level input (lockfile, lint
//! config, cargo/nextest config, this tool's own crate), a path matching no
//! package and no rule, a component crate anywhere in the closure, and any
//! error at all reaching for git or the package graph.

use std::collections::BTreeSet;
use std::process::Command;

use anyhow::{Context, Result, bail};

use crate::affected::graph::Workspace;
use crate::affected::rules::global_screen;

/// Workspace-level inputs on top of the ones [`global_screen`] already names.
///
/// That screen answers "which tests must run", so it names only what can move
/// a test outcome; this one answers "which crates does the whole mechanical
/// gate run over", and `rustfmt.toml` reshapes what `cargo fmt -- --check`
/// says about every file in the tree. The member it moves is workspace-wide
/// whatever the diff touched, so the entry changes no verdict today — it is
/// here so the list reads as the complete set of workspace inputs rather than
/// as the subset that happens to matter under the current member breakdown.
const VERIFY_RUN_ALL_EXACT: &[&str] = &["rustfmt.toml"];

/// The crate set a member of the umbrella compiles over.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Scope {
    /// Every workspace crate, for the stated reason.
    Workspace {
        /// Why the closure was not trusted — or not asked for — stated in the
        /// run's own evidence so a fail-open is read rather than inferred.
        reason: String,
    },
    /// The candidate diff's reverse-dependency closure.
    Closure {
        /// The crates the compiling members run over, sorted.
        packages: Vec<String>,
        /// The workspace crates they do not, sorted. The half of the receipt
        /// that makes a wrong closure legible: a crate that should have been
        /// reached appears here by name instead of vanishing.
        skipped: Vec<String>,
    },
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

        Self::over_changed(&changed)
    }

    /// The scope a candidate diff of these paths computes — the whole decision
    /// past the git read, so it is exercisable against a stated diff.
    fn over_changed(changed: &[String]) -> Result<Self> {
        if let Some(hit) = global_screen(changed).or_else(|| verify_screen(changed)) {
            return Ok(Self::workspace(format!("a workspace-level input changed: {hit}")));
        }

        let workspace = Workspace::load()?;
        let selection = workspace.select(changed)?;
        if let Some(reason) = selection.run_all {
            return Ok(Self::workspace(reason));
        }
        // A closure of nothing is a diff the path rules resolved to no crate
        // at all — prose, agent state, a non-`ci.yml` workflow. Narrowing to
        // an empty package list would ask cargo to compile nothing and report
        // that as a verdict, so the empty case takes the whole tree like every
        // other case the closure cannot speak for.
        if selection.packages.is_empty() {
            return Ok(Self::workspace("the diff resolved to no workspace crate"));
        }
        if let Some(source) = wasm_source_in(&selection.packages, workspace.wasm_sources()) {
            return Ok(Self::workspace(format!(
                "{source} compiles to component wasm, which tests load by path rather than by linkage"
            )));
        }

        let members = workspace.members();
        let skipped = members.difference(&selection.packages).cloned().collect();
        Ok(Self::Closure { packages: selection.packages.into_iter().collect(), skipped })
    }

    /// The crates a scoped member narrows to, or `None` when this run compiles
    /// the whole workspace.
    pub(super) fn packages(&self) -> Option<&[String]> {
        match self {
            Self::Workspace { .. } => None,
            Self::Closure { packages, .. } => Some(packages),
        }
    }

    /// The receipt this run writes alongside its members' logs: what the
    /// compiling gates ran over, and what they did not.
    ///
    /// Both halves, because only the pair makes a wrong closure visible. A
    /// selection that silently lost a crate reads as an ordinary pass unless
    /// the run states which crates it declined to look at.
    pub(super) fn receipt(&self) -> String {
        match self {
            Self::Workspace { reason } => format!("verify scope: every workspace crate — {reason}\n"),
            Self::Closure { packages, skipped } => format!(
                "verify scope: the candidate diff's reverse-dependency closure.\n\
                 crates in ({}): {}\ncrates skipped ({}): {}\n",
                packages.len(),
                packages.join(" "),
                skipped.len(),
                skipped.join(" "),
            ),
        }
    }

    /// The line a scoped member's own log opens with, so a reader who opens
    /// `verify.clippy.log` learns what it looked at without correlating files.
    /// `None` for a workspace run, whose log needs no qualification.
    pub(super) fn member_notice(&self) -> Option<String> {
        let Self::Closure { packages, skipped } = self else {
            return None;
        };
        Some(format!(
            "note: scoped to {} of {} workspace crates — the candidate diff's reverse-dependency closure \
             (see verify.scope.log)\n",
            packages.len(),
            packages.len() + skipped.len(),
        ))
    }
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

/// Screen for the workspace-level inputs this lane names beyond the
/// affected-package screen's, returning the first hit.
fn verify_screen(changed: &[String]) -> Option<&str> {
    changed.iter().map(String::as_str).find(|path| VERIFY_RUN_ALL_EXACT.contains(path))
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

    use super::{Scope, verify_screen, wasm_source_in};
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
        let scope = Scope::over_changed(&strings(&["crates/aether-chassis-bloomery/src/lib.rs"]))
            .expect("compute the closure over a near-leaf change");

        let packages = scope.packages().expect("a near-leaf change narrows");
        assert!(packages.contains(&"aether-chassis-bloomery".to_owned()), "the changed crate is in its own closure");
        let Scope::Closure { skipped, .. } = &scope else {
            unreachable!("packages() already proved the closure arm")
        };
        assert!(!skipped.is_empty(), "a near-leaf change must skip something, or nothing was narrowed");
        assert!(
            skipped.iter().any(|name| name == "aether-math"),
            "a crate the change cannot reach must be skipped: {skipped:?}",
        );
    }

    #[test]
    fn a_workspace_level_input_runs_the_whole_workspace() {
        // Tripwire for acceptance 2. Each of these reshapes the build graph,
        // the lint configuration, or the selection machinery itself, so a
        // closure computed from it is not a statement about what the change
        // can reach. A missed entry is a narrowed run whose premise no longer
        // holds — the exact false green the narrowing is only permitted to
        // exist without.
        for path in [
            "Cargo.toml",
            "Cargo.lock",
            "rustfmt.toml",
            "clippy.toml",
            "rust-toolchain.toml",
            ".config/nextest.toml",
            ".cargo/config.toml",
            "xtask/src/transform/verify/scope.rs",
            ".github/workflows/ci.yml",
        ] {
            let scope = Scope::over_changed(&strings(&[path])).expect("screen the changed path");
            assert_eq!(scope.packages(), None, "{path} must run the whole workspace");
            assert!(scope.receipt().contains(path), "the receipt names what forced it: {}", scope.receipt());
        }

        assert!(
            verify_screen(&strings(&["crates/aether-math/src/lib.rs"])).is_none(),
            "an ordinary crate source must not be screened",
        );
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
        let scope =
            Scope::over_changed(&strings(&["crates/aether-chassis-bloomery/src/lib.rs"])).expect("compute the closure");
        let receipt = scope.receipt();

        assert!(receipt.contains("crates in ("), "{receipt}");
        assert!(receipt.contains("crates skipped ("), "{receipt}");
        assert!(receipt.contains("aether-chassis-bloomery"), "the changed crate is named: {receipt}");
        assert!(scope.member_notice().expect("a scoped run qualifies its members' logs").contains("workspace crates"));
    }
}
