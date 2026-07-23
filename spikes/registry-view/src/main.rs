//! Prices the registry redesign: `RwLock<FxHashMap>` (production) vs
//! `ArcSwap` snapshot views with a single-writer owner, under the three
//! loads that decide it — read scaling, reads during churn, and
//! write-path throughput with mail-batched application.

mod scenarios;
mod tables;

use std::time::Duration;

fn main() {
    let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0);
    println!("registry-view spike — table size {}, host logical cores {cores}\n", scenarios::TABLE_SIZE);

    println!("A. read scaling, no churn (ns/lookup)");
    println!("{:<18} {:>8} {:>8} {:>8}", "table", "1 thr", "4 thr", "8 thr");
    let runs: Vec<Vec<(&'static str, f64)>> =
        [1usize, 4, 8].iter().map(|&t| scenarios::read_scaling(t, 2_000_000)).collect();
    for i in 0..runs[0].len() {
        println!("{:<18} {:>8.1} {:>8.1} {:>8.1}", runs[0][i].0, runs[0][i].1, runs[1][i].1, runs[2][i].1);
    }

    println!("\nB. 4 readers vs one flat-out writer, 800 millis window (clone-per-batch = 64)");
    println!("{:<24} {:>14} {:>16}", "config", "read ns/op", "writer ops/s");
    for r in scenarios::read_under_churn(Duration::from_millis(800), 64) {
        println!("{:<24} {:>14.1} {:>16.0}", r.label, r.reader_nanos, r.writer_ops_per_sec);
    }

    println!("\nC. write path — 200k inserts from 4 producers (owner drains, applies, publishes per cycle)");
    println!("{:<26} {:>12} {:>10} {:>14}", "config", "ns/update", "publishes", "drained/cycle");
    for batch in [1usize, 32, 256] {
        for r in scenarios::write_path(200_000, batch) {
            if batch == 1 || !r.label.starts_with("lock") {
                println!(
                    "{:<26} {:>12.1} {:>10} {:>14.1}",
                    r.label, r.per_update_nanos, r.publishes, r.mean_drained_batch
                );
            }
        }
    }

    println!("\nD. publish-strategy scaling with table size (single-threaded costs)");
    println!(
        "{:>10} {:>16} {:>14} {:>14} {:>12} {:>12}",
        "entries", "fx clone µs", "fx ins ns", "im ins ns", "fx read ns", "im read ns"
    );
    for n in [10_000u64, 100_000, 1_000_000] {
        let r = scenarios::publish_scale(n);
        println!(
            "{:>10} {:>16.1} {:>14.1} {:>14.1} {:>12.1} {:>12.1}",
            r.table_size, r.fx_clone_micros, r.fx_insert_nanos, r.im_insert_nanos, r.fx_read_nanos, r.im_read_nanos
        );
    }
}
