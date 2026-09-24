//! Boot: the shipped bloomery chassis over the seeded journal, plus the reply
//! sink.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;

use aether_bloomery_journal::Batch;
use aether_chassis::boot::{
    ActorRingConfig, ChassisBase, RegistryQueueConfig, RuntimeConfig, SchedulerTuningConfig, SettlementConfig,
};
use aether_chassis_bloomery::chassis::{BloomeryChassis, BloomeryEnv};
use aether_chassis_bloomery::{BloomeryCli, BloomeryConfig};
use aether_http::HttpConfig;
use aether_substrate::Subname;
use aether_substrate::config::{ConfigMember, ConfigSources, SecretsDir, StageArgv};

use crate::BloomeryHarness;
use crate::drive::ReplySink;
use crate::seed::SeededJournal;

impl SeededJournal {
    /// Boot the bloomery chassis over this journal and spawn the harness's
    /// reply sink.
    ///
    /// The chassis is built through `BloomeryChassis::build_mounted` with
    /// default base members and the widest closure limit, and is never
    /// `run()`: mail dispatches on the substrate's own threads, and the
    /// passives tear down when the harness drops. Config resolves
    /// hermetically, so HTTP egress keeps its compiled deny-all default.
    ///
    /// # Panics
    ///
    /// Panics when the chassis refuses to boot or the reply sink does not
    /// spawn.
    #[must_use]
    pub fn boot(self) -> BloomeryHarness {
        self.boot_with(None)
    }

    /// [`SeededJournal::boot`], with the `aether-bloomery` binary's own flags
    /// staged the way the binary stages them.
    ///
    /// `cli` is what `BloomeryCli::try_parse_from` gives for an
    /// `aether-bloomery` argv. Its `--secrets-dir` is located and its argv
    /// overlays are staged onto a hermetic source stack — the path
    /// `ChassisCli::into_sources` takes, minus the environment and the
    /// `--config` file, which are never read. So `--http-allowlist`,
    /// `--http-secrets`, and every other flag resolve exactly as they do in the
    /// binary, and nothing leaks in from the process that runs the harness.
    /// The seed's journal always wins over `--bloomery-journal`.
    ///
    /// A secret an `--http-secrets` binding names is read from the secrets
    /// directory once, at boot, by `aether.http`, and never leaves it: the
    /// harness, the journal, and every mail see only the binding's names.
    ///
    /// # Panics
    ///
    /// Panics when `--secrets-dir` is not an absolute directory (naming the
    /// path and the rule), or as [`SeededJournal::boot`] — a binding whose
    /// secret does not load, or whose host is not allowlisted, refuses boot.
    #[must_use]
    pub fn boot_with_argv(self, cli: BloomeryCli) -> BloomeryHarness {
        let mut sources = ConfigSources::hermetic();
        sources.set_secrets_dir(
            SecretsDir::locate(cli.meta.secrets_dir.clone()).unwrap_or_else(|error| panic!("{error}")),
        );
        cli.stage_argv(&mut sources);
        self.boot_over(sources)
    }

    /// [`SeededJournal::boot`], with `http` staged as the programmatic HTTP
    /// egress config when it is `Some`.
    fn boot_with(self, http: Option<HttpConfig>) -> BloomeryHarness {
        let mut sources = ConfigSources::hermetic();
        if let Some(http) = http {
            sources.set_override(http);
        }
        self.boot_over(sources)
    }

    /// Boot the chassis over this journal off `sources`, a hermetic stack the
    /// caller prepared, and spawn the reply sink.
    fn boot_over(self, sources: ConfigSources) -> BloomeryHarness {
        let (chassis, mounted) = BloomeryChassis::build_mounted(env(&self.journal, sources))
            .unwrap_or_else(|error| panic!("boot the bloomery chassis over {}: {error}", self.journal.display()));
        let (sender, arrivals) = mpsc::channel();
        let sink = chassis
            .spawn_actor::<ReplySink>(Subname::Named("harness"), (), sender)
            .finish()
            .unwrap_or_else(|error| panic!("spawn the harness reply sink: {error:?}"));
        BloomeryHarness { chassis, mounted, sink, arrivals, early: HashMap::new(), correlations: 0, journal: self }
    }
}

impl BloomeryHarness {
    /// Seed a scratch journal with `batches` and boot the bloomery chassis
    /// over it: [`SeededJournal::new`] then [`SeededJournal::boot`].
    ///
    /// # Panics
    ///
    /// Panics when the seed does not append or the chassis does not boot.
    #[must_use]
    pub fn start(batches: impl IntoIterator<Item = Batch>) -> Self {
        SeededJournal::new(batches).boot()
    }

    /// [`BloomeryHarness::start`], with HTTP egress allowed to exactly
    /// `hosts`.
    ///
    /// Config resolves hermetically, so `start` keeps the capability's empty
    /// allowlist and every fetch is answered `AllowlistDenied`. Each host
    /// matches a fetch URL's host exactly, whatever its port. A scenario that
    /// needs any other HTTP knob — a bound secret among them — boots through
    /// [`SeededJournal::boot_with_argv`] with the binary's own flags instead.
    ///
    /// # Panics
    ///
    /// Panics when the seed does not append or the chassis does not boot.
    #[must_use]
    pub fn start_allowing(
        batches: impl IntoIterator<Item = Batch>,
        hosts: impl IntoIterator<Item = impl Into<String>>,
    ) -> Self {
        let http = HttpConfig { allowlist: hosts.into_iter().map(Into::into).collect(), ..HttpConfig::default() };
        SeededJournal::new(batches).boot_with(Some(http))
    }

    /// The journal file the chassis opened.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        self.journal.journal_path()
    }
}

/// A chassis env over `sources`, a hermetic source stack (programmatic over
/// argv over default, never the process environment), with the bloomery
/// journal pointing at `journal`.
///
/// Every member the chassis resolves chassis-side is resolved off `sources`
/// here, as `BloomeryEnv::from_cli` does in the binary, so a staged argv layer
/// is consumed rather than refused as orphaned, and an unstaged member takes
/// its compiled default. The resolved journal is then replaced by `journal`.
fn env(journal: &Path, mut sources: ConfigSources) -> BloomeryEnv {
    let actor_ring = resolve::<ActorRingConfig>(&mut sources);
    let scheduler_tuning = resolve::<SchedulerTuningConfig>(&mut sources);
    let registry_queues = resolve::<RegistryQueueConfig>(&mut sources);
    let settlement = resolve::<SettlementConfig>(&mut sources);
    let runtime = resolve::<RuntimeConfig>(&mut sources);
    let bloomery = resolve::<BloomeryConfig>(&mut sources);
    BloomeryEnv {
        base: ChassisBase { sources, actor_ring, scheduler_tuning, registry_queues, settlement },
        runtime,
        bloomery: BloomeryConfig { journal: Some(journal.display().to_string()), ..bloomery },
    }
}

/// Resolve member `C` off `sources`, panicking with the refusal: a flag the
/// scenario staged that does not parse leaves nothing to test.
fn resolve<C: ConfigMember>(sources: &mut ConfigSources) -> C {
    sources.resolve::<C>().unwrap_or_else(|error| panic!("resolve the harness config: {error}"))
}
