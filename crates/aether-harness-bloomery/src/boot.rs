//! Boot: the shipped bloomery chassis over the seeded journal, plus the reply
//! sink.

use std::collections::HashMap;
use std::path::Path;
use std::sync::mpsc;

use aether_bloomery_journal::Batch;
use aether_bloomery_kinds::ClosureLimit;
use aether_chassis::boot::{ChassisBase, RuntimeConfig};
use aether_chassis_bloomery::BloomeryConfig;
use aether_chassis_bloomery::chassis::{BloomeryChassis, BloomeryEnv};
use aether_substrate::Subname;
use aether_substrate::config::ConfigSources;

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
    /// passives tear down when the harness drops.
    ///
    /// # Panics
    ///
    /// Panics when the chassis refuses to boot or the reply sink does not
    /// spawn.
    #[must_use]
    pub fn boot(self) -> BloomeryHarness {
        let Self { scratch, journal } = self;
        let (chassis, mounted) = BloomeryChassis::build_mounted(env(&journal))
            .unwrap_or_else(|error| panic!("boot the bloomery chassis over {}: {error}", journal.display()));
        let (sender, arrivals) = mpsc::channel();
        let sink = chassis
            .spawn_actor::<ReplySink>(Subname::Named("harness"), (), sender)
            .finish()
            .unwrap_or_else(|error| panic!("spawn the harness reply sink: {error:?}"));
        BloomeryHarness {
            chassis,
            mounted,
            sink,
            arrivals,
            early: HashMap::new(),
            correlations: 0,
            journal,
            _scratch: scratch,
        }
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

    /// The journal file the chassis opened.
    #[must_use]
    pub fn journal_path(&self) -> &Path {
        &self.journal
    }
}

/// A chassis env with default base members and the bloomery knobs pointing at
/// `journal`.
fn env(journal: &Path) -> BloomeryEnv {
    BloomeryEnv {
        base: ChassisBase { sources: ConfigSources::new(None), ..Default::default() },
        runtime: RuntimeConfig::default(),
        bloomery: BloomeryConfig {
            journal: Some(journal.display().to_string()),
            closure_limit_bytes: ClosureLimit::MAX_BYTES,
        },
    }
}
