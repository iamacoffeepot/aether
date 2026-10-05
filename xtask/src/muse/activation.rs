//! What the journal says about the muse reactor: live, rejected, or never
//! activated.
//!
//! The driver records every activation attempt for a reactor head, an
//! `Activated` when the reactor came live and an `ActivationRejected` when it
//! did not. [`Activations`] is the driver's own fold of those entries, so the
//! verbs read the engine's current state from the same record instead of
//! asking it over mail. The fold drops a rejection's reason, so this module
//! keeps the latest one for the muse head beside it.

use aether_bloomery_kinds::{Activated, ActivationRejected, JournalEntry};
use aether_bloomery_muse::MUSE;
use aether_bloomery_program::{Activations, HeadActivation};
use aether_data::Digest;
use anyhow::{Result, anyhow, bail};

use crate::bloomery::{Reads, decode_entry, page, wait_past};

/// Where the muse head's reactor stands.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum MuseActivation {
    /// The driver brought a reactor live for `bundle`.
    Live { bundle: Digest },
    /// The driver's last attempt failed: the entry at `seq` rejected `bundle`
    /// for `reason`.
    Rejected { seq: u64, bundle: Digest, reason: String },
    /// No attempt is recorded: the muse head was never bound.
    Never,
}

impl MuseActivation {
    /// Succeed when a reactor is live.
    ///
    /// # Errors
    /// The reactor is not live; the error names the recorded reason and the
    /// remedy, or says to run `muse bind` when nothing was ever recorded.
    pub(super) fn refuse_unless_live(&self, unit: &str) -> Result<()> {
        match self {
            Self::Live { .. } => Ok(()),
            Self::Rejected { seq, bundle, reason } => bail!(
                "the muse reactor is not live: the driver rejected bundle {bundle} at journal entry {seq}: {reason}\n\
                 journal forks are expected after a stored-encoding change: stop the engine, move the directory \
                 `--bloomery-units {unit}=<dir>` names aside, restart, and run `muse bind` again"
            ),
            Self::Never => bail!("no muse reactor was ever activated on unit {unit}: run `muse bind` first"),
        }
    }
}

/// The muse head's state over the whole journal as it stands.
///
/// # Errors
/// A read failed or an activation entry did not fold.
pub(super) fn muse_activation(reads: &mut impl Reads) -> Result<MuseActivation> {
    drive(reads, Fold::new(None))
}

/// The muse head's state once the driver answers the head move published at
/// `fence`: the first activation entry for it caused past `fence`. Blocks on
/// the journal head until one is recorded.
///
/// # Errors
/// A read failed or an activation entry did not fold.
pub(super) fn verdict_after(reads: &mut impl Reads, fence: u64) -> Result<MuseActivation> {
    drive(reads, Fold::new(Some(fence)))
}

/// Fold the journal from its start until `fold` decides, or, with no fence to
/// decide on, until the head.
fn drive(reads: &mut impl Reads, mut fold: Fold) -> Result<MuseActivation> {
    let mut cursor = 0;
    loop {
        let (head, entries) = page(reads, cursor)?;
        for entry in &entries {
            fold.apply(entry)?;
            if fold.decided {
                return Ok(fold.state());
            }
        }

        cursor = entries.last().map_or(cursor, |entry| entry.seq);
        if cursor >= head {
            if fold.fence.is_none() {
                return Ok(fold.state());
            }
            wait_past(reads, cursor)?;
        }
    }
}

/// A rejection of the muse head, kept because [`Activations`] drops its
/// reason.
struct Rejection {
    seq: u64,
    bundle: Digest,
    reason: String,
}

struct Fold {
    activations: Activations,
    rejection: Option<Rejection>,
    /// The seq a verdict's cause must pass, when one is awaited.
    fence: Option<u64>,
    decided: bool,
}

impl Fold {
    fn new(fence: Option<u64>) -> Self {
        Self { activations: Activations::new(), rejection: None, fence, decided: false }
    }

    fn apply(&mut self, entry: &JournalEntry) -> Result<()> {
        self.activations.apply(&entry.to_entry()).map_err(|error| anyhow!("folding entry {}: {error}", entry.seq))?;

        let rejected = decode_entry::<ActivationRejected>(entry)?.filter(|rejected| rejected.head == MUSE);
        let activated = decode_entry::<Activated>(entry)?.filter(|activated| *activated.head() == MUSE);
        let answers = rejected.is_some() || activated.is_some();
        let past_fence = matches!((self.fence, entry.cause), (Some(fence), Some(cause)) if cause > fence);
        self.decided = self.decided || (answers && past_fence);

        if let Some(rejected) = rejected {
            self.rejection = Some(Rejection {
                seq: entry.seq,
                bundle: rejected.bundle,
                reason: rejected.reason.as_str().to_owned(),
            });
        }
        Ok(())
    }

    fn state(&self) -> MuseActivation {
        match self.activations.get(&MUSE) {
            Some(HeadActivation::Live(activated)) => MuseActivation::Live { bundle: activated.bundle() },
            Some(HeadActivation::Owed { .. }) => {
                self.rejection.as_ref().map_or(MuseActivation::Never, |rejection| MuseActivation::Rejected {
                    seq: rejection.seq,
                    bundle: rejection.bundle,
                    reason: rejection.reason.clone(),
                })
            }
            None => MuseActivation::Never,
        }
    }
}
