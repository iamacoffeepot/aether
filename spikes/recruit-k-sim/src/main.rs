//! Standalone spike: cost-aware adaptive worker recruitment (K).
//!
//! Models the mechanism abstractly (no `aether-substrate` import) so the
//! endogeneity — recruit count K feeding back into measured handler time via
//! turbo/cache/bandwidth contention — can be dialed and observed. Companion to
//! `wishes/2026-05-25-cost-aware-recruit-k/`.
//!
//! The loop, per blob: estimate the feature `(T, w_max)` from the per-kind cost
//! EWMA -> bin it -> choose K (deterministic `T/w_max`, or sampled from a
//! per-bin `(mu, sigma)` policy warm-started at that formula) -> "execute"
//! (LPT pack groups onto K workers, contention inflates per-group time with K)
//! -> reward each recruited worker `work_claimed - C_wake` -> update the EWMA
//! and the policy cell (+ permeate to neighbour bins). Regret = chosen makespan
//! / brute-force-optimal makespan.
//!
//! Run: `cargo run --release` (zero deps).

const W: usize = 8; // worker count
const C_WAKE: f64 = 4.3; // wake cost, microseconds
const CONTENTION_PER: f64 = 0.10; // per-extra-active-worker slowdown at dial=1 (exaggerated for visibility)

const EWMA_ALPHA: f64 = 0.1;
const ALPHA: f64 = 0.04; // policy mean/variance step
const BETA: f64 = 0.04; // baseline step
const SIGMA_INIT: f64 = 1.6;
const SIGMA_FLOOR: f64 = 0.45; // exploration floor (EXP3-flavoured)
const PERM_RATE: f64 = 0.25; // permeation diffusion rate to neighbour bins
const LAMBDA: f64 = 0.5; // waste penalty per recruited worker (µs-equivalent) — breaks makespan-flat ties toward fewer workers

const ITERS: usize = 30_000;
const SWITCH: usize = 15_000; // regime switch (load gets heavier)
const REGIME_SCALE: f64 = 1.6;

/// Per-kind intrinsic handler cost (µs): trivial, light, moderate, heavy, fat.
const KIND_US: [f64; 5] = [0.05, 0.5, 2.0, 8.0, 10.0];

const NT: usize = 8;
const NW: usize = 7;
const T_LO: f64 = 0.1;
const T_HI: f64 = 200.0;
const WMAX_LO: f64 = 0.02;
const WMAX_HI: f64 = 20.0;

/// xorshift64 + Box-Muller, zero-dep.
struct Rng(u64);
impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed ^ 0x9E37_79B9_7F4A_7C15)
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }
    fn unit(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }
    fn gauss(&mut self, mu: f64, sigma: f64) -> f64 {
        let u1 = self.unit().max(1e-12);
        let u2 = self.unit();
        mu + sigma * (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

#[derive(Clone, Copy)]
enum Arch {
    TrivialWide,
    HeavyNarrow,
    BalancedWide,
    SkewedWide,
}
const ARCHES: [Arch; 4] = [
    Arch::TrivialWide,
    Arch::HeavyNarrow,
    Arch::BalancedWide,
    Arch::SkewedWide,
];
impl Arch {
    /// The recipient-groups of a blob, as handler-kind indices.
    fn kinds(self) -> Vec<usize> {
        match self {
            Arch::TrivialWide => vec![0; 10],
            Arch::HeavyNarrow => vec![3; 3],
            Arch::BalancedWide => vec![2; 8],
            Arch::SkewedWide => {
                let mut v = vec![4usize];
                v.extend([1usize; 7]);
                v
            }
        }
    }
    fn name(self) -> &'static str {
        match self {
            Arch::TrivialWide => "trivial-wide",
            Arch::HeavyNarrow => "heavy-narrow",
            Arch::BalancedWide => "balanced-wide",
            Arch::SkewedWide => "skewed-wide",
        }
    }
}

fn sample_blob(arch: Arch, regime_scale: f64, rng: &mut Rng) -> (Vec<usize>, Vec<f64>) {
    let kinds = arch.kinds();
    let intrinsic: Vec<f64> = kinds
        .iter()
        .map(|&k| (KIND_US[k] * regime_scale * (1.0 + 0.15 * (rng.unit() - 0.5))).max(0.001))
        .collect();
    (kinds, intrinsic)
}

/// Contention: more active workers -> each handler measured slower (turbo/cache/bandwidth).
fn contention_factor(active: usize, dial: f64) -> f64 {
    1.0 + dial * CONTENTION_PER * active.saturating_sub(1) as f64
}

fn effective(intrinsic: &[f64], k: usize, dial: f64) -> Vec<f64> {
    let f = contention_factor(k.min(intrinsic.len()), dial);
    intrinsic.iter().map(|&w| w * f).collect()
}

/// LPT pack groups onto k workers (worker 0 = producer, ready at 0; recruited
/// workers ready at `wake`), with **co-location coupling**: a worker holding
/// `m` groups runs them `(1 + coloc·(m−1))×` slower (cache/bandwidth
/// interference — superadditive packing). `coloc = 0` recovers plain LPT.
/// Returns the makespan. This is the effect `Σw/w_max` is blind to.
fn lpt(groups: &[f64], k: usize, wake: f64, coloc: f64) -> f64 {
    let k = k.max(1).min(groups.len().max(1));
    let start = |w: usize| if w == 0 { 0.0 } else { wake };
    let mut raw = vec![0.0f64; k];
    let mut cnt = vec![0usize; k];
    let mut gs = groups.to_vec();
    gs.sort_by(|a, b| b.partial_cmp(a).unwrap());
    for g in gs {
        let mut best = 0usize;
        let mut best_fin = f64::INFINITY;
        for w in 0..k {
            let f = start(w) + (raw[w] + g) * (1.0 + coloc * cnt[w] as f64);
            if f < best_fin {
                best_fin = f;
                best = w;
            }
        }
        raw[best] += g;
        cnt[best] += 1;
    }
    (0..k)
        .map(|w| start(w) + raw[w] * (1.0 + coloc * cnt[w].saturating_sub(1) as f64))
        .fold(0.0, f64::max)
}

/// Brute-force optimal K (accounts for contention + co-location at each K).
fn optimal(intrinsic: &[f64], dial: f64, coloc: f64) -> (usize, f64) {
    let kmax = intrinsic.len().clamp(1, W);
    let mut best = (1usize, f64::INFINITY);
    for k in 1..=kmax {
        let m = lpt(&effective(intrinsic, k, dial), k, C_WAKE, coloc);
        if m < best.1 {
            best = (k, m);
        }
    }
    best
}

fn log_bin(v: f64, lo: f64, hi: f64, n: usize) -> usize {
    let v = v.clamp(lo, hi);
    let t = (v.ln() - lo.ln()) / (hi.ln() - lo.ln());
    ((t * n as f64) as usize).min(n - 1)
}
fn log_center(i: usize, lo: f64, hi: f64, n: usize) -> f64 {
    (lo.ln() + (i as f64 + 0.5) / n as f64 * (hi.ln() - lo.ln())).exp()
}

#[derive(Clone)]
struct Cell {
    mu: f64,
    sigma: f64,
    baseline: f64,
    touched: bool,
}
impl Default for Cell {
    fn default() -> Self {
        Cell {
            mu: 1.0,
            sigma: SIGMA_INIT,
            baseline: 0.0,
            touched: false,
        }
    }
}

/// Deterministic recruiter: K = round(T / w_max), gated by T > C_wake.
fn det_k(t: f64, wmax: f64, g: usize) -> usize {
    let k0 = if t <= C_WAKE {
        1.0
    } else {
        (t / wmax.max(1e-9)).round()
    };
    (k0 as i64).clamp(1, g.min(W) as i64) as usize
}

/// Warm-start a cell's mean to the deterministic K of its bin centre.
fn ensure_warm(grid: &mut [Cell], it: usize, iw: usize) {
    let idx = it * NW + iw;
    if grid[idx].touched {
        return;
    }
    let tc = log_center(it, T_LO, T_HI, NT);
    let wc = log_center(iw, WMAX_LO, WMAX_HI, NW);
    let k0 = if tc <= C_WAKE {
        1.0
    } else {
        (tc / wc.max(1e-9)).round().clamp(1.0, W as f64)
    };
    grid[idx].mu = k0;
    grid[idx].sigma = SIGMA_INIT;
    grid[idx].touched = true;
}

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Det,
    Learn,
}

struct Config {
    mode: Mode,
    permeation: bool,
    dial: f64,
    coloc: f64,
    label: &'static str,
}

struct Report {
    label: String,
    regret_converged: f64,
    regret_recovery: f64,
    regret_settled: f64,
    avg_k: [f64; 4],
    opt_k: [f64; 4],
    spark: String,
    grid: Vec<Cell>,
}

fn run(cfg: &Config) -> Report {
    let mut rng = Rng::new(
        0x00C0_FFEE
            ^ cfg.dial.to_bits()
            ^ cfg.coloc.to_bits()
            ^ cfg.permeation as u64
            ^ ((cfg.mode == Mode::Learn) as u64) << 1,
    );
    let mut ewma = vec![1.0f64; KIND_US.len()]; // measured-time estimate per kind, neutral seed
    let mut grid = vec![Cell::default(); NT * NW];

    let mut sum_regret = [0.0f64; 3];
    let mut cnt_regret = [0usize; 3];
    let mut sum_k = [0.0f64; 4];
    let mut sum_optk = [0.0f64; 4];
    let mut cnt_arch = [0usize; 4];

    const BUCKETS: usize = 60;
    let mut bsum = [0.0f64; BUCKETS];
    let mut bcnt = [0usize; BUCKETS];

    for iter in 0..ITERS {
        let regime_scale = if iter < SWITCH { 1.0 } else { REGIME_SCALE };
        let ai = rng.below(4);
        let arch = ARCHES[ai];
        let (kinds, intrinsic) = sample_blob(arch, regime_scale, &mut rng);
        let g = kinds.len();

        let t_est: f64 = kinds.iter().map(|&k| ewma[k]).sum();
        let wmax_est: f64 = kinds.iter().map(|&k| ewma[k]).fold(0.0, f64::max);
        let it = log_bin(t_est, T_LO, T_HI, NT);
        let iw = log_bin(wmax_est, WMAX_LO, WMAX_HI, NW);

        let (k, x_sample) = match cfg.mode {
            Mode::Det => (det_k(t_est, wmax_est, g), 0.0),
            Mode::Learn => {
                ensure_warm(&mut grid, it, iw);
                let cell = &grid[it * NW + iw];
                let x = rng.gauss(cell.mu, cell.sigma);
                ((x.round() as i64).clamp(1, g.min(W) as i64) as usize, x)
            }
        };

        let eff = effective(&intrinsic, k, cfg.dial);
        let makespan_chosen = lpt(&eff, k, C_WAKE, cfg.coloc);
        // Reward on the LONGEST POLE (−makespan, low-noise from per-worker sums) minus a small
        // per-recruited-worker waste penalty (breaks the makespan-flat tie on skew toward fewer
        // workers). The two terms the spike derived: spread for latency, don't waste wakes.
        let reward = -makespan_chosen - LAMBDA * k.saturating_sub(1) as f64;

        let (optk, makespan_opt) = optimal(&intrinsic, cfg.dial, cfg.coloc);
        let regret = makespan_chosen / makespan_opt.max(1e-9);

        for (gi, &kind) in kinds.iter().enumerate() {
            ewma[kind] += EWMA_ALPHA * (eff[gi] - ewma[kind]);
        }

        if cfg.mode == Mode::Learn {
            let idx = it * NW + iw;
            let adv = reward - grid[idx].baseline;
            let s = grid[idx].sigma;
            grid[idx].mu += ALPHA * adv * (x_sample - grid[idx].mu) / (s * s);
            let mu_new = grid[idx].mu;
            grid[idx].sigma += ALPHA * adv * ((x_sample - mu_new).powi(2) - s * s) / (s * s * s);
            grid[idx].sigma = grid[idx].sigma.clamp(SIGMA_FLOOR, SIGMA_INIT * 1.5);
            grid[idx].baseline += BETA * adv;
            grid[idx].mu = grid[idx].mu.clamp(1.0, W as f64);

            if cfg.permeation {
                let mu_c = grid[idx].mu;
                for dit in -1i64..=1 {
                    for diw in -1i64..=1 {
                        if dit == 0 && diw == 0 {
                            continue;
                        }
                        let nt = it as i64 + dit;
                        let nw = iw as i64 + diw;
                        if nt < 0 || nt >= NT as i64 || nw < 0 || nw >= NW as i64 {
                            continue;
                        }
                        let (nt, nw) = (nt as usize, nw as usize);
                        ensure_warm(&mut grid, nt, nw);
                        let weight = if dit.abs() + diw.abs() == 1 { 1.0 } else { 0.5 };
                        let nidx = nt * NW + nw;
                        grid[nidx].mu += weight * PERM_RATE * (mu_c - grid[nidx].mu);
                    }
                }
            }
        }

        if iter >= 28_000 {
            sum_k[ai] += k as f64;
            sum_optk[ai] += optk as f64;
            cnt_arch[ai] += 1;
        }
        let win = if (13_000..15_000).contains(&iter) {
            Some(0)
        } else if (15_000..17_000).contains(&iter) {
            Some(1)
        } else if (28_000..30_000).contains(&iter) {
            Some(2)
        } else {
            None
        };
        if let Some(wn) = win {
            sum_regret[wn] += regret;
            cnt_regret[wn] += 1;
        }
        let b = (iter * BUCKETS) / ITERS;
        bsum[b] += regret;
        bcnt[b] += 1;
    }

    let avg = |s: f64, c: usize| if c > 0 { s / c as f64 } else { 0.0 };
    let bars = [' ', '▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];
    let mut spark = String::new();
    for (su, &cn) in bsum.iter().zip(bcnt.iter()) {
        if cn == 0 {
            spark.push(' ');
            continue;
        }
        let r = su / cn as f64;
        let lvl = (((r - 1.0) * 8.0).round().clamp(0.0, 8.0)) as usize;
        spark.push(bars[lvl]);
    }

    Report {
        label: cfg.label.to_string(),
        regret_converged: avg(sum_regret[0], cnt_regret[0]),
        regret_recovery: avg(sum_regret[1], cnt_regret[1]),
        regret_settled: avg(sum_regret[2], cnt_regret[2]),
        avg_k: std::array::from_fn(|i| avg(sum_k[i], cnt_arch[i])),
        opt_k: std::array::from_fn(|i| avg(sum_optk[i], cnt_arch[i])),
        spark,
        grid,
    }
}

fn print_surface(grid: &[Cell]) {
    println!(
        "  learned K surface  (rows: w_max bin, high→low; cols: T bin, low→high; '·' = untouched)"
    );
    for iw in (0..NW).rev() {
        print!("  w_max~{:6.2} │", log_center(iw, WMAX_LO, WMAX_HI, NW));
        for it in 0..NT {
            let c = &grid[it * NW + iw];
            if c.touched {
                print!(" {:>3.0}", c.mu);
            } else {
                print!("   ·");
            }
        }
        println!();
    }
    print!("       T~(µs) │");
    for it in 0..NT {
        print!(" {:>3.0}", log_center(it, T_LO, T_HI, NT));
    }
    println!();
}

fn main() {
    let configs = [
        Config {
            mode: Mode::Det,
            permeation: false,
            dial: 0.0,
            coloc: 0.0,
            label: "deterministic       · coloc 0.0",
        },
        Config {
            mode: Mode::Det,
            permeation: false,
            dial: 0.0,
            coloc: 0.3,
            label: "deterministic       · coloc 0.3",
        },
        Config {
            mode: Mode::Learn,
            permeation: true,
            dial: 0.0,
            coloc: 0.0,
            label: "learned (perm)      · coloc 0.0",
        },
        Config {
            mode: Mode::Learn,
            permeation: true,
            dial: 0.0,
            coloc: 0.3,
            label: "learned (perm)      · coloc 0.3",
        },
        Config {
            mode: Mode::Learn,
            permeation: true,
            dial: 0.0,
            coloc: 0.6,
            label: "learned (perm)      · coloc 0.6",
        },
    ];
    let reports: Vec<Report> = configs.iter().map(run).collect();

    println!("\n=== regret (chosen makespan / optimal; 1.00 = optimal) ===");
    println!(
        "{:<36}  {:>10} {:>10} {:>10}",
        "config", "converged", "recovery", "settled"
    );
    for r in &reports {
        println!(
            "{:<36}  {:>10.3} {:>10.3} {:>10.3}",
            r.label, r.regret_converged, r.regret_recovery, r.regret_settled
        );
    }
    println!(
        "  (converged = pre-switch steady; recovery = first 2k after the load spike; settled = final 2k)"
    );

    println!("\n=== regret over time (taller = worse; load spike at midpoint ↓) ===");
    let mid = " ".repeat(30) + "↓";
    println!("  {}", mid.trim_end());
    for r in &reports {
        println!("  {}  {}", r.spark, r.label);
    }

    println!("\n=== converged avg K chosen, by workload (settled window) ===");
    println!(
        "{:<36} {:>13} {:>13} {:>13} {:>13}",
        "config",
        ARCHES[0].name(),
        ARCHES[1].name(),
        ARCHES[2].name(),
        ARCHES[3].name()
    );
    for r in &reports {
        println!(
            "{:<36} {:>13.2} {:>13.2} {:>13.2} {:>13.2}",
            r.label, r.avg_k[0], r.avg_k[1], r.avg_k[2], r.avg_k[3]
        );
    }
    // optimal K is the same target regardless of policy at a given dial; show dial 0 and dial 1.
    println!(
        "{:<36} {:>13.2} {:>13.2} {:>13.2} {:>13.2}   <- optimal @ coloc 0.0",
        "optimal",
        reports[2].opt_k[0],
        reports[2].opt_k[1],
        reports[2].opt_k[2],
        reports[2].opt_k[3]
    );
    println!(
        "{:<36} {:>13.2} {:>13.2} {:>13.2} {:>13.2}   <- optimal @ coloc 0.3",
        "optimal",
        reports[3].opt_k[0],
        reports[3].opt_k[1],
        reports[3].opt_k[2],
        reports[3].opt_k[3]
    );

    println!("\n=== learned K surface (config: perm ON, coloc 0.3) ===");
    print_surface(&reports[3].grid);

    println!(
        "\nnote: co-location penalty {}%/extra co-located group; this is the regime Σw/w_max is blind to.",
        (0.3 * 100.0) as i64
    );
}
