//! The per-cell measurement vocabulary every latency section is built from:
//! which span a cell reports ([`Metric`]), the percentile grid it carries
//! ([`CellJson`] over [`Pct`]), and the [`CellKey`] the comparator pairs cells
//! by across trials.

use serde::{Deserialize, Serialize};

/// Which per-mail span a cell reports (iamacoffeepot/aether#1150). Each
/// measures one property, so a regression points at a mechanism rather
/// than a smeared rollup.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[serde(rename_all = "snake_case")]
pub enum Metric {
    /// iamacoffeepot/aether#1158: `t_sent − t_construct_start`: blob open →
    /// flush-begin — the producer-side time spent building the blob, the
    /// first leg of the four-stage lifecycle. ~0 on eager (non-buffered)
    /// paths.
    Construct,
    /// `t_enqueue − t_sent`: flush-begin → the worker picks up the blob —
    /// wakeup / scheduling latency. Tight on a warm worker.
    Queued,
    /// `t_received − t_enqueue`: blob pickup → this mail's handler entry —
    /// where in the blob's drain it landed. The only cardinality-sensitive
    /// span (a serial fan-out's late leaf waited behind its siblings), so
    /// high-variance by design.
    Drain,
    /// `t_finished − t_received`: the recipient's own handler work.
    Handler,
}

impl Metric {
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Construct => "construct",
            Self::Queued => "queued",
            Self::Drain => "drain",
            Self::Handler => "handler",
        }
    }
}

/// One cell's percentiles in a single trial. All latency values are
/// nanoseconds.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct CellJson {
    pub workers: usize,
    pub topo: String,
    pub metric: Metric,
    pub p50: u64,
    pub p90: u64,
    pub p99: u64,
    pub max: u64,
    pub n: usize,
    /// Mode indicator (iamacoffeepot/aether#4265): the fraction of samples past
    /// [`TAIL_MASS_MULTIPLE`](crate::perf::harness::TAIL_MASS_MULTIPLE) times
    /// this cell's own `p50`. A cell whose value flips between trials is
    /// bistable, and has no single number to compare.
    ///
    /// Defaulted on decode so a base trial built before this field existed
    /// still ingests — it reads as "no tail", which is also how a cell with no
    /// tail reports, so the comparator treats an old base as simply not
    /// carrying the signal rather than as evidence of stability.
    #[serde(default)]
    pub tail_mass: f64,
}

impl CellJson {
    #[allow(clippy::cast_precision_loss)]
    pub(super) fn percentile(&self, p: Pct) -> f64 {
        let ns = match p {
            Pct::P50 => self.p50,
            Pct::P90 => self.p90,
            Pct::P99 => self.p99,
        };
        ns as f64
    }

    pub(super) fn key(&self) -> CellKey {
        CellKey { workers: self.workers, topo: self.topo.clone(), metric: self.metric }
    }
}

#[derive(Clone, Copy)]
pub(super) enum Pct {
    P50,
    P90,
    P99,
}

impl Pct {
    pub(super) fn label(self) -> &'static str {
        match self {
            Self::P50 => "p50",
            Self::P90 => "p90",
            Self::P99 => "p99",
        }
    }
    pub(super) const ALL: [Self; 3] = [Self::P50, Self::P90, Self::P99];
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub(super) struct CellKey {
    pub(super) workers: usize,
    pub(super) topo: String,
    pub(super) metric: Metric,
}

/// Find the cell matching `key` in one trial's latency cells (a free fn,
/// not a closure, so the borrow of the returned `&CellJson` ties to the
/// slice's lifetime).
pub(super) fn find_cell<'a>(cells: &'a [CellJson], key: &CellKey) -> Option<&'a CellJson> {
    cells.iter().find(|c| c.workers == key.workers && c.topo == key.topo && c.metric == key.metric)
}
