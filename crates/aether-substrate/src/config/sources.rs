//! The builder's config source stack (ADR-0156 §5): the layers below `default`
//! each composed member resolves against, in precedence programmatic > argv >
//! env > file > default, plus the [`StageArgv`] handoff a chassis CLI root
//! stages its overlays through and the [`ConfigProvenance`] label reporting
//! which layer won.

use std::any::{Any, TypeId, type_name};
use std::collections::HashMap;
use std::collections::HashSet;
use std::env;
use std::fmt;

use confique::meta::Meta;

use super::error::ConfigError;
use super::known_keys::collect_meta_env_keys;
use super::member::ConfigMember;
use super::resolve::{FromArgvThenEnv, file_section};

/// Which layer of the source stack supplied a resolved member's value
/// (ADR-0156 §5). Reported per member by
/// [`ConfigManifest::provenance`](crate::ConfigManifest::provenance) — the
/// highest-precedence layer present in the stack for that member, in the
/// resolution order programmatic > argv > file > env > default.
///
/// Precision note: [`Programmatic`](Self::Programmatic), [`File`](Self::File),
/// [`Env`](Self::Env), and [`Default`](Self::Default) are detected exactly (an
/// override in the stack, a present file section, a set env key, else the
/// literal default). [`Argv`](Self::Argv) is reported when a non-empty argv
/// overlay is staged for the member; an empty overlay (no flag passed) falls
/// through to the lower layers, matching how resolution treats it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConfigProvenance {
    /// A programmatic explicit value (`Builder::with_actor_configured`) — the
    /// top layer, how the harnesses and tests construct configs in code.
    Programmatic,
    /// A non-empty argv overlay staged for this member.
    Argv,
    /// A present `[section]` in the loaded chassis config file.
    File,
    /// At least one of the member's `AETHER_*` env keys is set in the process
    /// environment.
    Env,
    /// No source supplied a value — the member resolves to its literal
    /// defaults.
    Default,
}

impl fmt::Display for ConfigProvenance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let label = match self {
            Self::Programmatic => "programmatic",
            Self::Argv => "argv",
            Self::File => "file",
            Self::Env => "env",
            Self::Default => "default",
        };
        f.write_str(label)
    }
}

/// ADR-0156 §5 (issue 3872): an argv overlay's own knowledge of which config
/// domain it stages onto the source stack. Closes the last hand-maintained edge
/// of the chassis config flow — the per-cap
/// `sources.set_argv::<HttpConfig>(http.into_layer())` block each chassis used
/// to assemble by hand (and could silently forget, discarding the operator's
/// flag).
///
/// Two derives emit the impls, so staging is derived from the declarations that
/// already exist:
///
/// - `#[derive(aether_substrate::Config)]` emits the **leaf** impl on each cap's
///   `*Overlay`, calling [`ConfigSources::set_argv`] with the domain type.
/// - `#[derive(aether_substrate::StageArgv)]` emits the **container** impl on
///   each hand-written chassis CLI root ([`crate::config`] docs), delegating to
///   every field's [`stage_argv`](Self::stage_argv). A field that is not
///   stageable must carry an explicit `#[stage(skip)]` or fail to compile — the
///   hole cannot reopen through the derive itself.
///
/// A chassis then stages its whole CLI in one `cli.stage_argv(&mut sources)`
/// call: adding an overlay field to a root IS staging it, and the converse
/// [`ConfigSources::validate_no_orphan_argv`] tripwire makes a
/// staged-but-never-composed layer a hard boot error.
pub trait StageArgv {
    /// Stage this overlay's argv layer(s) onto `sources`. A leaf overlay stages
    /// its own member (`sources.set_argv::<Domain>(self.into_layer())`); a
    /// container delegates to each of its fields' `stage_argv`.
    fn stage_argv(self, sources: &mut ConfigSources);
}

/// The builder's config source stack (ADR-0156 §5): the layers below `default`
/// that the composition boundary resolves each member against, in precedence
/// programmatic > argv > env > file > default. Assembled adjacent to
/// composition — the chassis loads the config file and stages each member's
/// argv overlay + any programmatic override — and handed to the builder, which
/// resolves every composed member off it ahead of `init`. Section identity
/// comes from each member's [`ConfigMember`] declaration, so no chassis-side
/// section string survives.
///
/// The programmatic layer is an explicit-value layer *inside* the stack rather
/// than a bypass around it: `SubstrateHarness` and unit tests stage config
/// values here (via `Builder::with_actor_configured`, which composes the actor
/// and stages its value together) and resolution short-circuits to them, so an
/// in-code construction never reads process env.
#[derive(Default)]
pub struct ConfigSources {
    file: Option<toml::Table>,
    /// ADR-0156 §5: when set, resolution runs the [hermetic](Self::hermetic)
    /// path — no env layer (and no file, forced `None` by the constructor), so
    /// a composed-but-unstaged member falls through to its compiled default
    /// rather than a stray process env var. `SubstrateHarness` sets it.
    hermetic: bool,
    /// Programmatic explicit values keyed by config `TypeId`. Boxed `dyn Any`
    /// because members are heterogeneously typed. Assembled and consumed on the
    /// one chassis-boot thread — no `Send`, matching the `Builder` itself
    /// (whose `!Send` driver-boot closure already keeps it thread-local). The
    /// parallel `override_names` keeps each override's `type_name` for the
    /// staged-but-never-composed boot error.
    overrides: HashMap<TypeId, Box<dyn Any>>,
    override_names: HashMap<TypeId, &'static str>,
    /// Per-member argv overlay layers keyed by config `TypeId`. Each holds the
    /// member's `<C::Layer as confique::Config>::Layer` produced by its
    /// `Overlay::into_layer` (a confique partial layer, not necessarily
    /// `Send`). The parallel `argv_names` keeps each staged layer's `type_name`
    /// for the staged-but-never-composed boot error, mirroring
    /// `override_names`; both are kept in lockstep by `set_argv` / `take_argv`,
    /// so a layer still present after resolution names its orphaned config type.
    argv: HashMap<TypeId, Box<dyn Any>>,
    argv_names: HashMap<TypeId, &'static str>,
}

impl ConfigSources {
    /// A source stack over the optional loaded chassis config file, with no
    /// argv overlays or programmatic overrides staged yet. Resolution runs the
    /// full stack — programmatic > argv > env > file > default.
    #[must_use]
    pub fn new(file: Option<toml::Table>) -> Self {
        Self {
            file,
            hermetic: false,
            overrides: HashMap::new(),
            override_names: HashMap::new(),
            argv: HashMap::new(),
            argv_names: HashMap::new(),
        }
    }

    /// ADR-0156 §5: a **hermetic** source stack — no env layer, no file layer.
    /// Resolution collapses to programmatic > argv > default; the process
    /// environment is never read. `SubstrateHarness` uses this so a member it
    /// composes but forgets to stage falls through to its compiled default
    /// (deterministic) rather than leaking a process env var into a test
    /// (before this inversion, harness-composed members never read env — their
    /// values were constructed directly — and this preserves that).
    #[must_use]
    pub fn hermetic() -> Self {
        Self {
            file: None,
            hermetic: true,
            overrides: HashMap::new(),
            override_names: HashMap::new(),
            argv: HashMap::new(),
            argv_names: HashMap::new(),
        }
    }

    /// Stage a programmatic explicit value for member `C` — the top layer of
    /// the stack. The internal mechanism `Builder::with_actor_configured` rides
    /// (which also composes the actor, so the override is never orphaned); a
    /// chassis test may also call it directly on a `ConfigSources` handed over
    /// via `with_config_sources`.
    pub fn set_override<C: 'static>(&mut self, value: C) {
        self.overrides.insert(TypeId::of::<C>(), Box::new(value));
        self.override_names.insert(TypeId::of::<C>(), type_name::<C>());
    }

    /// Stage member `C`'s argv overlay layer (its `Overlay::into_layer`
    /// output) — the typed argv handoff the chassis makes adjacent to
    /// composition, folded into the bulk stack that rides `Builder::with_config_sources`.
    pub fn set_argv<C: FromArgvThenEnv + 'static>(&mut self, layer: <C::Layer as confique::Config>::Layer) {
        self.argv.insert(TypeId::of::<C>(), Box::new(layer));
        self.argv_names.insert(TypeId::of::<C>(), type_name::<C>());
    }

    /// Resolve member `C` off the stack. Sugar for
    /// [`ConfigMember::resolve`] so a chassis can read a driver-only or
    /// chassis-declared member's resolved value (tick period, window mode,
    /// workers, ring capacities) from the same stack the builder resolves cap
    /// configs against — one resolution path, no chassis-side sections.
    ///
    /// # Errors
    ///
    /// Propagates [`ConfigError`] from the member's resolution.
    pub fn resolve<C: ConfigMember>(&mut self) -> Result<C, ConfigError> {
        C::resolve(self)
    }

    /// The highest-precedence source layer supplying member `C`'s value, or
    /// [`ConfigProvenance::Default`] when nothing above the compiled defaults
    /// does. The typed counterpart to [`ConfigManifest::provenance`](crate::ConfigManifest::provenance)'s
    /// whole-fleet rollup, for the caller that needs to slot a value *between*
    /// the stack and the defaults — a depot package manifest's cadence, which
    /// must lose to an operator's argv/env/file pin and win over the compiled
    /// default (issue 4006).
    ///
    /// Read it **before** [`resolve`](Self::resolve): resolution consumes the
    /// staged argv layer and programmatic override, so a provenance read after
    /// it reports `Default` for a member those layers supplied.
    ///
    /// Member-granular, not field-granular: a multi-field config reports the
    /// winning layer for the member as a whole, so one pinned field reads as
    /// "supplied" for all of them. Callers wanting per-field precision want
    /// the layer itself.
    ///
    /// A derive-`Config` type declares exactly one record, so the search below
    /// is really a lookup; a hand impl declaring several reports the first that
    /// any layer supplied.
    #[must_use]
    pub fn provenance_of<C: ConfigMember>(&self) -> ConfigProvenance {
        C::members()
            .iter()
            .map(|record| self.provenance(record.type_id, record.section, record.meta))
            .find(|provenance| *provenance != ConfigProvenance::Default)
            .unwrap_or(ConfigProvenance::Default)
    }

    fn take_override<C: 'static>(&mut self) -> Option<C> {
        self.overrides.remove(&TypeId::of::<C>()).map(|boxed| *boxed.downcast::<C>().expect("override TypeId keys C"))
    }

    fn take_argv<C: FromArgvThenEnv + 'static>(&mut self) -> Option<<C::Layer as confique::Config>::Layer> {
        // Drop the name in lockstep with the layer so the orphan-argv tripwire
        // only ever sees layers no member consumed.
        self.argv_names.remove(&TypeId::of::<C>());
        self.argv.remove(&TypeId::of::<C>()).map(|boxed| *boxed.downcast().expect("argv-layer TypeId keys C"))
    }

    /// The derive-`Config` resolution path: programmatic override, else the
    /// staged argv overlay atop env atop the named file section atop the
    /// literal defaults. The derive-emitted [`ConfigMember::resolve`] calls
    /// this with the type's own section, so precedence is byte-identical to
    /// the pre-inversion `resolve_with_file::<C>(argv, file, section)`.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError`] when the file section or a known env/argv value
    /// is malformed.
    pub fn resolve_layered<C: FromArgvThenEnv + 'static>(&mut self, section: &str) -> Result<C, ConfigError> {
        use confique::Layer as _;
        // Consume the staged argv layer unconditionally — even when a
        // programmatic override outranks it — so the orphan-argv tripwire
        // (which keys off layers left behind) never mistakes a legitimately
        // shadowed layer for an orphan. The override still wins; the argv value
        // is dropped, exactly as it was when the override short-circuited before.
        let argv = self.take_argv::<C>();
        if let Some(value) = self.take_override::<C>() {
            return Ok(value);
        }
        let argv = argv.unwrap_or_else(<<C::Layer as confique::Config>::Layer>::empty);
        // Hermetic mode (`SubstrateHarness`) skips both the env and the file
        // layers, so an unstaged member resolves to its compiled default.
        if self.hermetic {
            return C::try_resolve_hermetic(argv, None);
        }
        let file = match &self.file {
            Some(table) => file_section::<C>(table, section)?,
            None => None,
        };
        C::try_resolve(argv, file)
    }

    /// ADR-0156 §5: reject any staged programmatic override whose config type is
    /// not in `composed` — the set of `TypeId`s of every composed member's
    /// `Config` (each `with_actor`'s `A::Config`, each `declare_config_member`, and
    /// the driver's members). A typo'd or orphaned `with_config::<T>(value)` —
    /// staged but matching no composed member — is a hard boot error naming `T`
    /// rather than a value silently left behind. Run at build / claim time,
    /// before resolution.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::OrphanOverride`] naming the first override type
    /// that matches no composed member.
    pub fn validate_overrides(&self, composed: &HashSet<TypeId>) -> Result<(), ConfigError> {
        for (type_id, name) in &self.override_names {
            if !composed.contains(type_id) {
                return Err(ConfigError::OrphanOverride { type_name: (*name).to_owned() });
            }
        }
        Ok(())
    }

    /// ADR-0156 §5 converse tripwire (issue 3872): reject any staged argv layer
    /// that no composed member consumed. The resolve path removes each layer as
    /// its member resolves (`take_argv`), so a layer still present after boot
    /// resolution was staged for a config type no composed member resolves — a
    /// typo'd or orphaned `set_argv` (or, now, a `stage` delegating to a field
    /// whose overlay was flattened into a root the chassis never composes). The
    /// argv analogue of [`Self::validate_overrides`], failing as loudly rather
    /// than silently discarding the operator's flag (which resolution would
    /// otherwise treat as indistinguishable from the flag never being passed).
    /// Run once, after the Pass 0 resolve loop consumes every composed member's
    /// layer.
    ///
    /// # Errors
    ///
    /// Returns [`ConfigError::OrphanArgv`] naming the first argv layer left
    /// unconsumed.
    pub fn validate_no_orphan_argv(&self) -> Result<(), ConfigError> {
        if let Some(name) = self.argv_names.values().next() {
            return Err(ConfigError::OrphanArgv { type_name: (*name).to_owned() });
        }
        Ok(())
    }

    /// Whether a programmatic override is staged for member `C`.
    #[must_use]
    fn has_override(&self, type_id: TypeId) -> bool {
        self.overrides.contains_key(&type_id)
    }

    /// Whether an argv overlay is staged for member `C`.
    #[must_use]
    fn has_argv(&self, type_id: TypeId) -> bool {
        self.argv.contains_key(&type_id)
    }

    /// The highest-precedence source layer present for a member declaring
    /// `section` + `meta` and identified by `type_id`. Reads the stack plus
    /// live env; see [`ConfigProvenance`] for the precision contract.
    #[must_use]
    pub(super) fn provenance(&self, type_id: TypeId, section: &str, meta: &'static Meta) -> ConfigProvenance {
        if self.has_override(type_id) {
            return ConfigProvenance::Programmatic;
        }
        if self.has_argv(type_id) {
            return ConfigProvenance::Argv;
        }
        // A hermetic stack (`SubstrateHarness`) resolves programmatic > argv >
        // default and never reads env or file, so attributing a member to
        // either layer here would report a source resolution does not consult.
        if self.hermetic {
            return ConfigProvenance::Default;
        }
        if self.file.as_ref().is_some_and(|table| table.contains_key(section)) {
            return ConfigProvenance::File;
        }
        let mut keys = HashSet::new();
        collect_meta_env_keys(meta, &mut keys);
        // The sanctioned ADR-0090 config machinery: reading env to attribute a
        // resolved member's winning source layer for the discovery surface, the
        // central config-read path, not a cap reading its own knob.
        #[allow(clippy::disallowed_methods)]
        if keys.iter().any(|key| env::var_os(key).is_some_and(|value| !value.is_empty())) {
            return ConfigProvenance::Env;
        }
        ConfigProvenance::Default
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::test_fixtures::{HermeticFixture, PrecedenceConfig, precedence_file_table};

    #[test]
    fn config_sources_programmatic_override_short_circuits_argv_and_file() {
        use confique::Layer as _;
        // Tripwire: the programmatic layer is the top of the stack — a staged
        // override wins over an argv overlay (and never touches env/file). Drifts
        // if `resolve_layered`'s override short-circuit is dropped or reordered.
        let mut sources = ConfigSources::new(Some(precedence_file_table(11)));
        let mut argv = <PrecedenceConfig as confique::Config>::Layer::empty();
        argv.count = Some(33);
        sources.set_argv::<PrecedenceConfig>(argv);
        sources.set_override(PrecedenceConfig { count: 99 });
        let resolved = sources.resolve_layered::<PrecedenceConfig>("precedence").expect("resolve");
        assert_eq!(resolved.count, 99, "programmatic override wins over argv and file");
    }

    #[test]
    fn config_sources_provenance_reports_highest_present_layer() {
        // Tripwire: per-member provenance reports the highest-precedence layer
        // present — programmatic > argv > file (each checked before the env
        // branch, so present-layer detection is deterministic regardless of
        // ambient env). Drifts if the layer-detection precedence in
        // `ConfigSources::provenance` changes. A file section is staged in each
        // case, so a higher layer must still win.
        use confique::Layer as _;
        let meta = &<PrecedenceConfig as confique::Config>::META;
        let type_id = TypeId::of::<PrecedenceConfig>();

        let file_sources = ConfigSources::new(Some(precedence_file_table(11)));
        assert_eq!(file_sources.provenance(type_id, "precedence", meta), ConfigProvenance::File);

        let mut argv_sources = ConfigSources::new(Some(precedence_file_table(11)));
        argv_sources.set_argv::<PrecedenceConfig>(<PrecedenceConfig as confique::Config>::Layer::empty());
        assert_eq!(argv_sources.provenance(type_id, "precedence", meta), ConfigProvenance::Argv);

        let mut override_sources = ConfigSources::new(Some(precedence_file_table(11)));
        override_sources.set_override(PrecedenceConfig { count: 1 });
        assert_eq!(override_sources.provenance(type_id, "precedence", meta), ConfigProvenance::Programmatic);
    }

    #[test]
    fn validate_overrides_rejects_orphan_and_accepts_composed() {
        // Tripwire: an override whose type is in the composed set validates; one
        // that is not is a hard `OrphanOverride` naming the type. Drifts if the
        // staged-but-never-composed guard is dropped or inverted.
        let mut sources = ConfigSources::new(None);
        sources.set_override(PrecedenceConfig { count: 1 });

        let mut composed: HashSet<TypeId> = HashSet::new();
        composed.insert(TypeId::of::<PrecedenceConfig>());
        assert!(sources.validate_overrides(&composed).is_ok(), "a composed override is accepted");

        let orphan = sources.validate_overrides(&HashSet::new());
        match orphan {
            Err(ConfigError::OrphanOverride { type_name }) => {
                assert!(type_name.contains("PrecedenceConfig"), "the error names the orphan type: {type_name}");
            }
            other => panic!("expected OrphanOverride, got {other:?}"),
        }
    }

    #[test]
    fn orphan_argv_layer_is_rejected_then_cleared_by_resolution() {
        use confique::Layer as _;
        // Tripwire (issue 3872): the converse of the override guard on the argv
        // channel. A staged argv layer no member consumes is a hard `OrphanArgv`
        // naming the config type — the silent-drop the ADR-0156 §5 inversion
        // closes — and resolving the member consumes the layer so the stack is
        // then clean. Hermetic so the resolve never touches env (no race).
        // Drifts if `set_argv` / `take_argv` stop tracking the name in lockstep,
        // or `validate_no_orphan_argv` stops reporting a leftover layer.
        let mut sources = ConfigSources::hermetic();
        sources.set_argv::<HermeticFixture>(<HermeticFixture as confique::Config>::Layer::empty());

        match sources.validate_no_orphan_argv() {
            Err(ConfigError::OrphanArgv { type_name }) => {
                assert!(type_name.contains("HermeticFixture"), "the error names the orphan type: {type_name}");
            }
            other => panic!("expected OrphanArgv for a staged-but-unconsumed layer, got {other:?}"),
        }

        sources.resolve_layered::<HermeticFixture>("hermetic_fixture").expect("resolves off the staged layer");
        sources.validate_no_orphan_argv().expect("no orphan once the member consumed its layer");
    }

    #[test]
    fn hermetic_resolution_ignores_env() {
        // Tripwire: hermetic resolution skips the env layer — a set env var does
        // not leak into a hermetic-stack resolve; the member falls through to its
        // compiled default. The non-hermetic path still reads env. Unique env key
        // (no race). Drifts if `try_resolve_hermetic` regains an `.env()` layer.
        // SAFETY: unique key set then removed in this test scope.
        unsafe { env::set_var("AETHER_HERMETIC_FIXTURE_COUNT", "55") };
        let hermetic = ConfigSources::hermetic().resolve_layered::<HermeticFixture>("hermetic");
        let full = ConfigSources::new(None).resolve_layered::<HermeticFixture>("hermetic");
        // SAFETY: same scope.
        unsafe { env::remove_var("AETHER_HERMETIC_FIXTURE_COUNT") };
        assert_eq!(hermetic.expect("hermetic resolve").count, 7, "hermetic skips env → default");
        assert_eq!(full.expect("full resolve").count, 55, "the full stack still reads env");
    }
}
