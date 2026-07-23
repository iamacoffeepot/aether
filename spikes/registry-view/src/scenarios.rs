//! The three measurements: read scaling with no churn, reads under
//! writer interference, and write-path throughput (direct locking vs
//! mail-batched single owner).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use crate::tables::{Entry, Ids, ImTable, LockTable, SwapTable, seed_fx};

pub const TABLE_SIZE: u64 = 10_000;

/// Scenario A: T reader threads, no writer. ns per lookup.
pub fn read_scaling(threads: usize, per_thread: u64) -> Vec<(&'static str, f64)> {
    let lock = Arc::new(LockTable::seeded(TABLE_SIZE));
    let swap = Arc::new(SwapTable::seeded(TABLE_SIZE));
    let im = Arc::new(ImTable::seeded(TABLE_SIZE));

    vec![
        ("lock/clone-out", time_readers(threads, per_thread, move |ids| lock.read(ids.next()).is_some())),
        ("swap/clone-out", {
            let swap = Arc::clone(&swap);
            time_readers(threads, per_thread, move |ids| swap.read(ids.next()).is_some())
        }),
        ("swap/in-place", time_readers(threads, per_thread, move |ids| swap.read_in_place(ids.next()).is_some())),
        ("im/clone-out", time_readers(threads, per_thread, move |ids| im.read(ids.next()).is_some())),
    ]
}

fn time_readers(threads: usize, per_thread: u64, op: impl Fn(&mut Ids) -> bool + Send + Sync) -> f64 {
    let started = Instant::now();
    thread::scope(|scope| {
        for t in 0..threads {
            let op = &op;
            scope.spawn(move || {
                let mut ids = Ids::new(t as u64 + 1, TABLE_SIZE);
                let mut hits = 0u64;
                for _ in 0..per_thread {
                    hits += op(&mut ids) as u64;
                }
                assert!(hits > 0);
            });
        }
    });
    started.elapsed().as_nanos() as f64 / (threads as u64 * per_thread) as f64
}

pub struct ChurnResult {
    pub label: &'static str,
    pub reader_nanos: f64,
    pub writer_ops_per_sec: f64,
}

/// Scenario B: 4 readers for a fixed window while one writer churns
/// insert/remove pairs flat out. The lock writer takes the write guard
/// per operation; the snapshot owners mutate a working map and publish
/// per `batch` operations (clone-per-batch) or per operation (im).
pub fn read_under_churn(window: Duration, batch: usize) -> Vec<ChurnResult> {
    let mut results = Vec::new();

    let lock = Arc::new(LockTable::seeded(TABLE_SIZE));
    results.push(churn_run(
        "lock/write-per-op",
        window,
        {
            let lock = Arc::clone(&lock);
            move |ids| lock.read(ids.next()).is_some()
        },
        {
            let mut cursor = TABLE_SIZE;
            move || {
                lock.insert(cursor, Entry::new(cursor));
                lock.remove(cursor);
                cursor += 1;
                2
            }
        },
    ));

    let swap = Arc::new(SwapTable::seeded(TABLE_SIZE));
    results.push(churn_run(
        "swap/publish-per-batch",
        window,
        {
            let swap = Arc::clone(&swap);
            move |ids| swap.read(ids.next()).is_some()
        },
        {
            let mut working = seed_fx(TABLE_SIZE);
            let mut cursor = TABLE_SIZE;
            move || {
                for _ in 0..batch / 2 {
                    working.insert(cursor, Entry::new(cursor));
                    working.remove(&cursor);
                    cursor += 1;
                }
                swap.publish(working.clone());
                batch as u64
            }
        },
    ));

    let im = Arc::new(ImTable::seeded(TABLE_SIZE));
    results.push(churn_run(
        "im/publish-per-op",
        window,
        {
            let im = Arc::clone(&im);
            move |ids| im.read(ids.next()).is_some()
        },
        {
            let mut working = im.snapshot();
            let mut cursor = TABLE_SIZE;
            move || {
                working.insert(cursor, Entry::new(cursor));
                im.publish(working.clone());
                working.remove(&cursor);
                im.publish(working.clone());
                cursor += 1;
                2
            }
        },
    ));

    results
}

fn churn_run(
    label: &'static str,
    window: Duration,
    read: impl Fn(&mut Ids) -> bool + Send + Sync,
    mut write: impl FnMut() -> u64 + Send,
) -> ChurnResult {
    let stop = AtomicBool::new(false);
    let reads = AtomicU64::new(0);
    let read_nanos = AtomicU64::new(0);
    let writes = AtomicU64::new(0);

    thread::scope(|scope| {
        for t in 0..4usize {
            let (stop, reads, read_nanos, read) = (&stop, &reads, &read_nanos, &read);
            scope.spawn(move || {
                let mut ids = Ids::new(t as u64 + 11, TABLE_SIZE);
                let mut hits = 0u64;
                let mut ops = 0u64;
                let started = Instant::now();
                while !stop.load(Ordering::Relaxed) {
                    for _ in 0..1024 {
                        hits += read(&mut ids) as u64;
                    }
                    ops += 1024;
                }
                assert!(hits > 0);
                read_nanos.fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
                reads.fetch_add(ops, Ordering::Relaxed);
            });
        }
        let (stop, writes) = (&stop, &writes);
        scope.spawn(move || {
            let mut local = 0u64;
            while !stop.load(Ordering::Relaxed) {
                local += write();
            }
            writes.store(local, Ordering::Relaxed);
        });
        thread::sleep(window);
        stop.store(true, Ordering::Relaxed);
    });

    ChurnResult {
        label,
        reader_nanos: read_nanos.load(Ordering::Relaxed) as f64 / reads.load(Ordering::Relaxed) as f64,
        writer_ops_per_sec: writes.load(Ordering::Relaxed) as f64 / window.as_secs_f64(),
    }
}

pub struct WriteResult {
    pub label: String,
    pub per_update_nanos: f64,
    pub publishes: u64,
    pub mean_drained_batch: f64,
}

/// Scenario C: 4 producers push `total` inserts. Direct mode: each takes
/// the write lock per insert. Mail mode: producers send `batch`-sized
/// envelopes over an mpsc to one owner, which drains everything queued,
/// applies, and publishes once per drain cycle — the self-batching the
/// design predicts under load.
pub fn write_path(total: u64, batch: usize) -> Vec<WriteResult> {
    let mut results = Vec::new();

    let lock = Arc::new(LockTable::seeded(TABLE_SIZE));
    let started = Instant::now();
    thread::scope(|scope| {
        for p in 0..4u64 {
            let lock = Arc::clone(&lock);
            scope.spawn(move || {
                let base = 1_000_000 * (p + 1);
                for i in 0..total / 4 {
                    lock.insert(base + i, Entry::new(base + i));
                }
            });
        }
    });
    results.push(WriteResult {
        label: "lock/direct".into(),
        per_update_nanos: started.elapsed().as_nanos() as f64 / total as f64,
        publishes: 0,
        mean_drained_batch: 0.0,
    });

    for (label, per_op_publish) in [("mail+swap", false), ("mail+im", true)] {
        let (tx, rx) = mpsc::channel::<Vec<(u64, Entry)>>();
        let swap = SwapTable::seeded(TABLE_SIZE);
        let im = ImTable::seeded(TABLE_SIZE);
        let started = Instant::now();
        let owner = thread::spawn(move || {
            let mut working_fx = seed_fx(TABLE_SIZE);
            let mut working_im = im.snapshot();
            let mut publishes = 0u64;
            let mut cycles = 0u64;
            let mut drained = 0u64;
            while let Ok(first) = rx.recv() {
                let mut updates = first;
                while let Ok(more) = rx.try_recv() {
                    updates.extend(more);
                }
                drained += updates.len() as u64;
                cycles += 1;
                if per_op_publish {
                    for (id, entry) in updates {
                        working_im.insert(id, entry);
                        im.publish(working_im.clone());
                        publishes += 1;
                    }
                } else {
                    for (id, entry) in updates {
                        working_fx.insert(id, entry);
                    }
                    swap.publish(working_fx.clone());
                    publishes += 1;
                }
            }
            (publishes, drained as f64 / cycles as f64)
        });
        thread::scope(|scope| {
            for p in 0..4u64 {
                let tx = tx.clone();
                scope.spawn(move || {
                    let base = 1_000_000 * (p + 1);
                    let mut staged = Vec::with_capacity(batch);
                    for i in 0..total / 4 {
                        staged.push((base + i, Entry::new(base + i)));
                        if staged.len() == batch {
                            tx.send(std::mem::take(&mut staged)).unwrap();
                            staged = Vec::with_capacity(batch);
                        }
                    }
                    if !staged.is_empty() {
                        tx.send(staged).unwrap();
                    }
                });
            }
        });
        drop(tx);
        let (publishes, mean_drained_batch) = owner.join().unwrap();
        results.push(WriteResult {
            label: format!("{label}/flush-batch-{batch}"),
            per_update_nanos: started.elapsed().as_nanos() as f64 / total as f64,
            publishes,
            mean_drained_batch,
        });
    }

    results
}
