//! The saturated cell's steady-state completion rate, estimated over the
//! trimmed middle of a run so the fill ramp and the drain tail stay out of the
//! denominator.

use aether_data::Kind;
use aether_kinds::trace::MailNodeWire;

use super::Ping;

/// Minimum completion count for the trimmed-window throughput estimate
/// (iamacoffeepot/aether#1227). Below this, dropping 10% off each end would
/// leave too few completions to characterise the saturated regime, so the cell
/// falls back to the full window — the ramp/tail are then an unavoidable but
/// small fraction of a short run.
const THROUGHPUT_TRIM_FLOOR: usize = 50;

/// Completed mails/sec from a cell's folded trace nodes — a **steady-state
/// estimate** over the saturated middle of the run
/// (iamacoffeepot/aether#1202, refined by iamacoffeepot/aether#1227).
///
/// A makespan average — `completed / (max(t_finished) − min(t_construct
/// start))` — folds the unsaturated **ramp-up** (the source still building
/// roots while most workers are cold) and the **drain-down tail** (a few mails
/// left, the pool under-utilised) into the denominator, so it systematically
/// understates the rate the pool sustains while the queue is actually deep,
/// with topology-dependent contamination. Instead, order the completions by
/// `t_finished`, trim the first and last 10% (the ramp and the tail), and take
/// the inter-completion rate over the inner window:
/// `completions_in_window / (t_finished_high − t_finished_low)`. Both sides of
/// the paired ADR-0085 comparison compute the same statistic, so the
/// higher-is-better delta semantics are unchanged.
///
/// A cell with fewer than [`THROUGHPUT_TRIM_FLOOR`] completions falls back to
/// the full window (no trim). Returns `None` when fewer than two `Ping`s
/// completed, or the window collapsed to a single instant (clock-coincident),
/// so the caller stores `None` rather than a divide-by-zero or infinite rate.
#[allow(clippy::cast_precision_loss)]
pub(super) fn throughput_from_nodes(mails: &[MailNodeWire]) -> Option<f64> {
    let mut finishes: Vec<u64> = mails
        .iter()
        .filter(|node| node.kind.0 == Ping::ID.0)
        .filter_map(|node| node.t_finished.map(|fin| fin.0))
        .collect();
    if finishes.len() < 2 {
        return None;
    }
    finishes.sort_unstable();
    let n = finishes.len();

    // Trim 10% off each end to drop the fill ramp and the drain tail — but
    // only when enough completions remain for the inner window to still
    // represent the deep-queue regime. `n / 10` is `floor(0.10 · n)`.
    let (lo, hi) = if n >= THROUGHPUT_TRIM_FLOOR {
        let cut = n / 10;
        (cut, n - cut)
    } else {
        (0, n)
    };
    let window = &finishes[lo..hi];
    let completions = window.len();
    if completions < 2 {
        return None;
    }
    let span_nanos = window[completions - 1] - window[0];
    if span_nanos == 0 {
        return None;
    }
    let span_secs = span_nanos as f64 / 1e9;
    Some(completions as f64 / span_secs)
}

#[cfg(test)]
mod tests {
    use aether_data::MailboxId;

    use super::*;

    #[test]
    fn throughput_from_nodes_handles_degenerate_windows() {
        // No completed nodes → no rate (rather than a divide-by-zero).
        assert!(throughput_from_nodes(&[]).is_none());
    }

    /// A completed `Ping` node finishing at `t_finished_nanos`. Only `kind`
    /// (must be `Ping`) and `t_finished` drive `throughput_from_nodes`; every
    /// other field is filler.
    fn finished_ping(correlation: u64, t_finished_nanos: u64) -> MailNodeWire {
        use aether_data::MailId;
        use aether_kinds::trace::Nanos;
        MailNodeWire {
            mail_id: MailId { sender: MailboxId(0), correlation_id: correlation },
            parent: None,
            sender: MailboxId(0),
            recipient: MailboxId(0),
            kind: Ping::ID,
            t_construct_start: Nanos(0),
            t_sent: Nanos(0),
            t_enqueue: None,
            enqueue_depth: None,
            t_received: None,
            t_finished: Some(Nanos(t_finished_nanos)),
            thread_name: None,
        }
    }

    #[test]
    #[allow(clippy::cast_precision_loss)]
    fn throughput_trims_ramp_and_tail_to_recover_steady_rate() {
        // iamacoffeepot/aether#1227: a node set with a slow fill ramp and a
        // slow drain tail bracketing an evenly-paced saturated middle. The
        // makespan average (whole-batch span) understates the deep-queue rate;
        // trimming 10% off each end must recover the injected steady rate.
        //
        // Steady middle: `STEADY` completions at a 1 ms spacing → 1000/sec.
        // Ramp + tail: `TRIM` completions each, spaced 20 ms (slow), so the
        // 10% trim drops exactly the ramp and the tail, leaving the steady run.
        const STEADY: u64 = 208;
        const TRIM: u64 = 26; // floor(0.10 × (208 + 26 + 26)) = 26
        const STEADY_DT_NANOS: u64 = 1_000_000; // 1 ms → 1000 completions/sec
        const SLOW_DT_NANOS: u64 = 20_000_000; // 20 ms ramp/tail spacing

        let mut nodes = Vec::new();
        let mut corr = 0u64;
        // Ramp: earliest finishes, far apart (cold pool filling).
        for i in 0..TRIM {
            nodes.push(finished_ping(corr, i * SLOW_DT_NANOS));
            corr += 1;
        }
        // Steady middle: evenly paced, starting after the ramp.
        let steady_base = TRIM * SLOW_DT_NANOS + SLOW_DT_NANOS;
        for i in 0..STEADY {
            nodes.push(finished_ping(corr, steady_base + i * STEADY_DT_NANOS));
            corr += 1;
        }
        // Tail: latest finishes, far apart (drain under-utilised).
        let tail_base = steady_base + STEADY * STEADY_DT_NANOS + SLOW_DT_NANOS;
        for i in 0..TRIM {
            nodes.push(finished_ping(corr, tail_base + i * SLOW_DT_NANOS));
            corr += 1;
        }

        let rate = throughput_from_nodes(&nodes).expect("a populated cell reports a rate");
        // The trimmed inner window is the evenly-paced steady run; the rate is
        // `completions / span = STEADY / ((STEADY-1)·dt)` ≈ 1000/sec.
        assert!((rate - 1000.0).abs() < 20.0, "trimmed rate must recover the ~1000/sec steady rate, got {rate}");
        // The full-batch makespan average is dragged far lower by the slow
        // ramp/tail — the contamination this fix removes.
        let total = STEADY + 2 * TRIM;
        let makespan_secs = (tail_base + (TRIM - 1) * SLOW_DT_NANOS) as f64 / 1e9;
        let makespan_rate = total as f64 / makespan_secs;
        assert!(
            makespan_rate < 600.0 && rate > makespan_rate * 1.5,
            "the trimmed rate ({rate}) must exceed the contaminated makespan rate ({makespan_rate})"
        );
    }

    #[test]
    #[allow(clippy::cast_precision_loss, clippy::cast_possible_truncation)]
    fn throughput_small_cell_falls_back_to_full_window() {
        // Below the trim floor, a cell uses the full window (no trim): a
        // handful of evenly-paced completions reports `completions / span`
        // over all of them, not a trimmed subset.
        let dt_nanos = 1_000_000u64; // 1 ms
        let count = 5u64;
        let nodes: Vec<MailNodeWire> = (0..count).map(|i| finished_ping(i, i * dt_nanos)).collect();
        assert!(
            (count as usize) < THROUGHPUT_TRIM_FLOOR,
            "this fixture must sit below the trim floor to exercise the fallback"
        );
        let rate = throughput_from_nodes(&nodes).expect("a small cell still reports a rate");
        // Full window: 5 completions over (5-1)×1 ms = 4 ms → 1250/sec.
        let expected = count as f64 / ((count - 1) * dt_nanos) as f64 * 1e9;
        assert!(
            (rate - expected).abs() < 1.0,
            "small-cell fallback rate {rate} must match the full-window rate {expected}"
        );
    }

    #[test]
    fn throughput_one_completion_is_none() {
        // A single completion has no window to measure a rate over → `None`,
        // not a divide-by-zero (the existing degenerate guard, at count 1).
        assert!(throughput_from_nodes(&[finished_ping(0, 42)]).is_none());
    }

    #[test]
    fn throughput_clock_coincident_completions_are_none() {
        // Many completions, all at the same instant → zero span → `None`
        // rather than an infinite rate.
        let nodes: Vec<MailNodeWire> = (0..8).map(|i| finished_ping(i, 5_000)).collect();
        assert!(throughput_from_nodes(&nodes).is_none());
    }
}
