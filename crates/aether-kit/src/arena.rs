//! The survival-game director: an open floor where choreographed hazard
//! shapes telegraph a warning (orange) and then strike (red), and the player
//! must not be standing on a striking cell. This is the rules half of the
//! game — a pure fixed-tick state machine, no rendering or input of its own,
//! and integer-only so a run is deterministic and replays bit-for-bit (the
//! pattern sequence is driven by a hand-rolled xorshift, no `rand` dep).
//!
//! # Resolution
//!
//! Hazards live on a grid [`SUB`]× finer than the tile grid, so a ring reads
//! as a smooth shape rather than a blocky tile run. The player's tile grid is
//! unchanged; this is purely the hazard "bitmask".
//!
//! # Patterns and the telegraph
//!
//! A [`Shape`] is a moving hazard. Each tick the whole field is repainted from
//! the active shapes' current ages. The danger (red) front is the warning
//! (orange) front delayed by [`LEAD_TICKS`]: a shape paints its warning region
//! at its current age and its danger region at `age - LEAD_TICKS`, so a cell is
//! *always* orange for the full lead before it can turn red — nothing ever
//! strikes a cell that wasn't telegraphed. Repainting from position each tick
//! (rather than ageing a per-cell trail) is what lets a moving hazard carry a
//! clean leading telegraph instead of a smear.
//!
//! - **Ring** (outward / inward) — a solid expanding or contracting annulus,
//!   bounded so a safe island always remains: outward, stay clear of the band;
//!   inward, reach the center before it closes.
//! - **Column** (a swept wall with a gap) — line up with the gap as it
//!   crosses; the projectile-like pattern.
//! - **Wave** — a single expanding arc (one ~90° sector of an outward ring);
//!   step out of its path as the radial front sweeps past.

use crate::runtime::{GRID_H, GRID_W};

/// Hazard sub-cells per tile, per axis: the hazard grid is this much finer
/// than the tile grid.
pub const SUB: i32 = 4;
/// Hazard grid width / height, in sub-cells.
pub const HW: i32 = GRID_W * SUB;
pub const HH: i32 = GRID_H * SUB;
/// Hazard sub-cell count.
const HCELLS: usize = (HW * HH) as usize;

/// Telegraph colour: a cell is about to strike.
const WARNING_COLOR: (f32, f32, f32) = (0.95, 0.55, 0.12);
/// Striking colour: standing here is lethal.
const DANGER_COLOR: (f32, f32, f32) = (0.85, 0.13, 0.11);

/// Telegraph lead: every cell glows orange for this many ticks (~1.4 s at
/// 60 Hz) before it can strike. The danger front is the warning front delayed
/// by this, so nothing strikes a cell that wasn't telegraphed.
const LEAD_TICKS: i32 = 84;

/// Difficulty runs `0..=INTENSITY_MAX`. A level ramps it across its duration,
/// and every spawn snapshots its parameters from it (see [`Tuning`]) so each
/// pattern's motion stays consistent even as the ramp moves underneath it.
const INTENSITY_MAX: i32 = 100;

/// Inward rings always collapse from this radius; the safe-island *floor* they
/// stop at is the difficulty-driven part (see [`Tuning::in_min`]).
const RING_IN_START: i32 = 40;

/// The eight compass directions a wave can face.
const COMPASS_DIRS: [(i32, i32); 8] = [
    (1, 0),
    (-1, 0),
    (0, 1),
    (0, -1),
    (1, 1),
    (1, -1),
    (-1, 1),
    (-1, -1),
];

/// Spawn cadence (ticks between patterns) and max concurrent patterns, each an
/// `(easy, hard)` pair interpolated by intensity — harder spawns more often and
/// runs more at once.
const SPAWN_INTERVAL: (i32, i32) = (150, 55);
const CONCURRENT: (i32, i32) = (1, 3);

/// Global speed is `speed_num / SPEED_DEN`: the arena clock advances that many
/// quarter-steps per tick, scaling motion, telegraph, and spawn rate together.
/// `SPEED_DEN` is 1×.
const SPEED_DEN: i32 = 4;
const SPEED_MIN: i32 = 1; // 0.25×
const SPEED_MAX: i32 = 16; // 4×

/// Fixed non-zero RNG seed: every run faces the same pattern sequence, which is
/// both fair across players and exactly replayable.
const SEED: u64 = 0x2545_F491_4F6C_DD1D;

/// A sub-cell's current hazard state.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Phase {
    Safe,
    Warning,
    Danger,
}

/// A choreographed hazard. Positions / centers are in sub-cells.
#[derive(Clone, Copy)]
enum Shape {
    /// A solid expanding (`inward == false`) or contracting annulus about a
    /// center, bounded so a safe island always remains.
    Ring { cx: i32, cz: i32, inward: bool },
    /// A wall sweeping along one axis with a gap. `horizontal` sweeps a
    /// full-width row along Z; `reverse` flips the travel direction. `gap_lo`
    /// is the near edge of the gap on the spanning axis.
    Column {
        horizontal: bool,
        gap_lo: i32,
        reverse: bool,
    },
    /// A single expanding arc — one sector of an outward ring, facing
    /// `(ux, uz)`. A radial wave to dodge out of the way of.
    Wave { cx: i32, cz: i32, ux: i32, uz: i32 },
}

/// The three hazard families a level is built from. A level confines spawns to
/// one class; `Shape` is the concrete instance (a ring can collapse or expand, a
/// wall sweep either axis, and so on).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShapeClass {
    Ring,
    Wall,
    Wave,
}

/// A shape's parameters at a given difficulty. Snapshotted when a pattern spawns
/// so its motion stays consistent even as the level's intensity ramps underneath
/// it. Fields a class doesn't use stay at their defaults.
#[derive(Clone, Copy)]
struct Tuning {
    /// Band depth, sub-cells.
    thick: i32,
    /// Ticks per sub-cell of front travel — smaller is faster.
    speed: i32,
    /// Ring: outward frontier cap (safe band beyond it).
    out_max: i32,
    /// Ring: inward collapse floor (safe island inside it).
    in_min: i32,
    /// Wall: gap width, sub-cells.
    gap: i32,
    /// Wave: radial reach.
    reach: i32,
    /// Wave: arc half-angle as a `(num, den)` slope bound on `|cross| / dot`.
    wedge: (i32, i32),
}

impl Tuning {
    /// The parameter envelope for a class at `intensity`, interpolating each
    /// knob from its easy value (intensity 0) to its hard value
    /// ([`INTENSITY_MAX`]). Harder means faster, thicker, and tighter margins.
    fn for_class(class: ShapeClass, intensity: i32) -> Self {
        let t = intensity.clamp(0, INTENSITY_MAX);
        let lerp = |easy, hard| lerp_i(easy, hard, t, INTENSITY_MAX);
        let base = Self {
            thick: 0,
            speed: 5,
            out_max: 0,
            in_min: 0,
            gap: 0,
            reach: 0,
            wedge: (1, 1),
        };
        match class {
            ShapeClass::Ring => Self {
                thick: lerp(2, 5),
                speed: lerp(8, 3),
                out_max: lerp(20, 28),
                in_min: lerp(16, 8),
                ..base
            },
            ShapeClass::Wall => Self {
                thick: lerp(2, 20),
                speed: lerp(7, 3),
                gap: lerp(18, 6),
                ..base
            },
            ShapeClass::Wave => Self {
                thick: lerp(2, 4),
                speed: lerp(7, 3),
                reach: lerp(40, 60),
                wedge: (lerp(1, 5), 2),
                ..base
            },
        }
    }
}

/// One live pattern: a shape, the ticks since it spawned, and the tuning it was
/// born with.
struct Pattern {
    shape: Shape,
    age: i32,
    tuning: Tuning,
}

impl Pattern {
    /// Whether the pattern has finished — its danger front has run off the end
    /// of its animation — and should be retired.
    fn done(&self) -> bool {
        let tn = &self.tuning;
        match self.shape {
            Shape::Ring { inward, .. } => {
                let travel = if inward {
                    RING_IN_START - tn.in_min
                } else {
                    tn.out_max
                };
                self.age > LEAD_TICKS + (travel + tn.thick) * tn.speed
            }
            Shape::Column { horizontal, .. } => {
                let span = if horizontal { HH } else { HW };
                self.age > LEAD_TICKS + (span + tn.thick) * tn.speed
            }
            Shape::Wave { .. } => self.age > LEAD_TICKS + (tn.reach + tn.thick) * tn.speed,
        }
    }

    /// Paint this shape's warning band (current age) and danger front (age
    /// minus the lead) into the field.
    fn paint(&self, field: &mut [Phase]) {
        let tn = &self.tuning;
        let age = self.age;
        let danger_age = (age - LEAD_TICKS).max(0);
        match self.shape {
            Shape::Ring { cx, cz, inward } => {
                let wf = ring_radius(age, inward, tn);
                let df = ring_radius(danger_age, inward, tn);
                // Warning spans the whole band from the danger front to the
                // leading edge; danger is the thin front, painted only once the
                // lead has elapsed.
                let (lo, hi) = if inward {
                    (wf, df + tn.thick)
                } else {
                    (df, wf + tn.thick)
                };
                paint_annulus(field, cx, cz, lo, hi, Phase::Warning);
                if age >= LEAD_TICKS {
                    paint_annulus(field, cx, cz, df, df + tn.thick, Phase::Danger);
                }
            }
            Shape::Column {
                horizontal,
                gap_lo,
                reverse,
            } => {
                let span = if horizontal { HH } else { HW };
                let wpos = column_pos(age, span, reverse, tn.speed);
                let dpos = column_pos(danger_age, span, reverse, tn.speed);
                let dir = if reverse { -1 } else { 1 };
                let gap_hi = gap_lo + tn.gap;
                let (lo, hi) = if wpos <= dpos {
                    (wpos, dpos)
                } else {
                    (dpos, wpos)
                };
                for pos in lo..=hi {
                    paint_line(field, horizontal, pos, gap_lo, gap_hi, Phase::Warning);
                }
                if age >= LEAD_TICKS {
                    for k in 0..tn.thick {
                        paint_line(
                            field,
                            horizontal,
                            dpos - dir * k,
                            gap_lo,
                            gap_hi,
                            Phase::Danger,
                        );
                    }
                }
            }
            Shape::Wave { cx, cz, ux, uz } => {
                let wf = (age / tn.speed).min(tn.reach);
                let df = (danger_age / tn.speed).min(tn.reach);
                let arc = ArcSpec {
                    cx,
                    cz,
                    dir: (ux, uz),
                    wedge: tn.wedge,
                };
                paint_arc(field, df, wf + tn.thick, Phase::Warning, arc);
                if age >= LEAD_TICKS {
                    paint_arc(field, df, df + tn.thick, Phase::Danger, arc);
                }
            }
        }
    }
}

/// Ring front radius at a given age, clamped to its safe-island limit: outward
/// grows from 0 up to `tn.out_max`, inward shrinks from `RING_IN_START` down to
/// `tn.in_min`.
fn ring_radius(age: i32, inward: bool, tn: &Tuning) -> i32 {
    let step = age / tn.speed;
    if inward {
        (RING_IN_START - step).max(tn.in_min)
    } else {
        step.min(tn.out_max)
    }
}

/// Column front position at a given age along a `span`-long axis.
fn column_pos(age: i32, span: i32, reverse: bool, speed: i32) -> i32 {
    let step = age / speed;
    if reverse { span - 1 - step } else { step }
}

/// Integer linear interpolation: `a` at `t = 0`, `b` at `t = tmax`. Works for a
/// descending range (`a > b`), and rounds toward `a`.
fn lerp_i(a: i32, b: i32, t: i32, tmax: i32) -> i32 {
    a + (b - a) * t / tmax
}

/// The hazard field plus its deterministic spawn clock and live patterns.
pub struct Arena {
    phases: [Phase; HCELLS],
    patterns: Vec<Pattern>,
    /// xorshift64 state — the pattern sequence is a pure function of it.
    rng: u64,
    /// Game ticks since this arena was (re)started; drives the spawn cadence.
    elapsed: u64,
    /// Global speed numerator over [`SPEED_DEN`]; the clock advances this many
    /// quarter-steps per tick.
    speed_num: i32,
    /// Fractional carry for sub-1× speeds.
    time_accum: i32,
    /// The class a level confines spawns to; `None` is free-play (a random class
    /// each spawn).
    director: Option<ShapeClass>,
    /// Current difficulty, `0..=INTENSITY_MAX`. Drives each spawn's tuning and
    /// the spawn cadence/concurrency.
    intensity: i32,
}

impl Arena {
    pub fn new() -> Self {
        Self {
            phases: [Phase::Safe; HCELLS],
            patterns: Vec::new(),
            rng: SEED,
            elapsed: 0,
            speed_num: SPEED_DEN,
            time_accum: 0,
            director: None,
            intensity: INTENSITY_MAX / 2,
        }
    }

    /// Advance one display tick: step the arena clock by the current speed
    /// (zero or more logical steps), then repaint the field once.
    pub fn tick(&mut self) {
        self.time_accum += self.speed_num;
        while self.time_accum >= SPEED_DEN {
            self.time_accum -= SPEED_DEN;
            self.step();
        }
        self.phases.fill(Phase::Safe);
        for pattern in &self.patterns {
            pattern.paint(&mut self.phases);
        }
    }

    /// One logical step: spawn on the intensity-scaled cadence, age the
    /// patterns, retire finished ones.
    #[allow(clippy::cast_sign_loss)] // interval/concurrent lerp from non-negative endpoints
    fn step(&mut self) {
        let interval = lerp_i(
            SPAWN_INTERVAL.0,
            SPAWN_INTERVAL.1,
            self.intensity,
            INTENSITY_MAX,
        );
        let concurrent = lerp_i(CONCURRENT.0, CONCURRENT.1, self.intensity, INTENSITY_MAX);
        if self.elapsed.is_multiple_of(interval.max(1) as u64)
            && self.patterns.len() < concurrent.max(1) as usize
        {
            self.spawn();
        }
        for pattern in &mut self.patterns {
            pattern.age += 1;
        }
        self.patterns.retain(|pattern| !pattern.done());
        self.elapsed += 1;
    }

    /// Confine spawns to one class at a fixed difficulty — what a level drives
    /// each tick as its clock ramps the intensity.
    pub fn set_level(&mut self, class: ShapeClass, intensity: i32) {
        self.director = Some(class);
        self.intensity = intensity.clamp(0, INTENSITY_MAX);
    }

    /// Clear the field back to a fresh start (no live patterns, clock zeroed,
    /// RNG re-seeded) — used when the game restarts after a death.
    pub fn reset(&mut self) {
        self.phases.fill(Phase::Safe);
        self.patterns.clear();
        self.rng = SEED;
        self.elapsed = 0;
        self.time_accum = 0;
    }

    /// Whether a sub-cell is currently lethal (red). Out-of-bounds reads safe.
    #[allow(clippy::cast_sign_loss)] // caller passes in-bounds coords
    pub fn is_danger(&self, sx: i32, sz: i32) -> bool {
        (0..HW).contains(&sx)
            && (0..HH).contains(&sz)
            && self.phases[(sz * HW + sx) as usize] == Phase::Danger
    }

    /// Nudge global speed up / down a quarter-step; returns the new speed as a
    /// percentage (100 = 1×).
    pub fn speed_up(&mut self) -> i32 {
        self.speed_num = (self.speed_num + 1).min(SPEED_MAX);
        self.speed_num * 100 / SPEED_DEN
    }

    pub fn speed_down(&mut self) -> i32 {
        self.speed_num = (self.speed_num - 1).max(SPEED_MIN);
        self.speed_num * 100 / SPEED_DEN
    }

    /// Pick the class (the level's, or a random one in free-play), snapshot its
    /// tuning at the current intensity, and place a fresh pattern.
    fn spawn(&mut self) {
        let class = self.director.unwrap_or_else(|| match self.next_rng() % 3 {
            0 => ShapeClass::Ring,
            1 => ShapeClass::Wall,
            _ => ShapeClass::Wave,
        });
        let tuning = Tuning::for_class(class, self.intensity);
        let shape = self.make_shape(class, &tuning);
        self.patterns.push(Pattern {
            shape,
            age: 0,
            tuning,
        });
    }

    /// A concrete shape of `class` — random placement / orientation from the
    /// RNG, with the gap sized to `tuning`.
    fn make_shape(&mut self, class: ShapeClass, tuning: &Tuning) -> Shape {
        let margin = SUB * 4;
        match class {
            ShapeClass::Ring => Shape::Ring {
                cx: self.rand_between(margin, HW - margin),
                cz: self.rand_between(margin, HH - margin),
                inward: self.next_rng().is_multiple_of(2),
            },
            ShapeClass::Wall => {
                let horizontal = self.next_rng().is_multiple_of(2);
                // Horizontal walls span X (gap on X); vertical span Z (gap on Z).
                let gap_span = if horizontal { HW } else { HH };
                Shape::Column {
                    horizontal,
                    gap_lo: self.rand_between(0, (gap_span - tuning.gap).max(1)),
                    reverse: self.next_rng().is_multiple_of(2),
                }
            }
            ShapeClass::Wave => {
                let (ux, uz) = self.rand_dir();
                Shape::Wave {
                    cx: self.rand_between(margin, HW - margin),
                    cz: self.rand_between(margin, HH - margin),
                    ux,
                    uz,
                }
            }
        }
    }

    /// Deterministic xorshift64 step.
    fn next_rng(&mut self) -> u64 {
        let mut x = self.rng;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.rng = x;
        x
    }

    /// A random one of the eight compass directions.
    #[allow(clippy::cast_possible_truncation)]
    fn rand_dir(&mut self) -> (i32, i32) {
        COMPASS_DIRS[(self.next_rng() % 8) as usize]
    }

    /// A deterministic integer in `[lo, hi)` (or `lo` if the range is empty).
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    fn rand_between(&mut self, lo: i32, hi: i32) -> i32 {
        if hi <= lo {
            return lo;
        }
        lo + (self.next_rng() % (hi - lo) as u64) as i32
    }

    /// Overlay colour to paint a sub-cell, or `None` when it is safe (the
    /// floor shows through). With `show_warnings` off, the orange telegraph is
    /// hidden too — only the red strike renders. Callers pass in-bounds
    /// sub-cell coords.
    #[allow(clippy::cast_sign_loss)]
    pub fn subcell_color(&self, sx: i32, sz: i32, show_warnings: bool) -> Option<(f32, f32, f32)> {
        match self.phases[(sz * HW + sx) as usize] {
            Phase::Warning if show_warnings => Some(WARNING_COLOR),
            Phase::Danger => Some(DANGER_COLOR),
            Phase::Safe | Phase::Warning => None,
        }
    }

    /// Design aid (not gameplay): freeze the field into a static 3×3 contact
    /// sheet of one shape's parameter variations, so the look of each parameter
    /// reads at a glance under the top-down preview camera. Thickness varies
    /// down the rows; the shape's spatial parameter varies across the columns.
    /// `shape`: `1` ring, `2` wall, `3` wave; anything else clears the field.
    /// The danger band shows red with its leading-edge orange telegraph, the
    /// same as in play.
    pub fn show_matrix(&mut self, shape: u32) {
        self.phases.fill(Phase::Safe);
        match shape {
            1 => self.paint_ring_matrix(),
            2 => self.paint_wall_matrix(),
            3 => self.paint_wave_matrix(),
            _ => {}
        }
    }

    /// Rings: band thickness down the rows, expansion-frontier radius across the
    /// columns. Orange leads outward — where the ring is about to reach.
    fn paint_ring_matrix(&mut self) {
        const RADII: [i32; 3] = [4, 6, 8];
        const THICKS: [i32; 3] = [2, 3, 4];
        for (row, &thick) in THICKS.iter().enumerate() {
            for (col, &radius) in RADII.iter().enumerate() {
                let (cx, cz) = panel_center(col, row);
                paint_annulus(&mut self.phases, cx, cz, radius, radius + 3, Phase::Warning);
                paint_annulus(
                    &mut self.phases,
                    cx,
                    cz,
                    radius - thick,
                    radius,
                    Phase::Danger,
                );
            }
        }
    }

    /// Walls: a wall spans the whole arena, so it reads as a barrier rather than
    /// a panel tile. Three full-width walls stacked down the field, thickness
    /// growing top to bottom at a fixed gap; orange leads downward (`+z`), the
    /// sweep direction. Each wall is anchored at the top of its band so the
    /// thick one grows down into its own space without overlapping the next.
    #[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // row is 0..3
    fn paint_wall_matrix(&mut self) {
        const THICKS: [i32; 3] = [4, 10, 16];
        const GAP: i32 = 10;
        let gap_lo = HW / 2 - GAP / 2;
        let gap_hi = HW / 2 + GAP / 2;
        let band = HH / 3;
        for (row, &thick) in THICKS.iter().enumerate() {
            let z0 = row as i32 * band + 1;
            for k in 0..thick {
                paint_line(
                    &mut self.phases,
                    true,
                    z0 + k,
                    gap_lo,
                    gap_hi,
                    Phase::Danger,
                );
            }
            for k in thick..(thick + 3) {
                paint_line(
                    &mut self.phases,
                    true,
                    z0 + k,
                    gap_lo,
                    gap_hi,
                    Phase::Warning,
                );
            }
        }
    }

    /// Waves: band thickness down the rows, arc width across the columns. Each
    /// arc opens downward (`+z`); orange leads outward along the radius.
    fn paint_wave_matrix(&mut self) {
        const WEDGES: [(i32, i32); 3] = [(1, 2), (1, 1), (2, 1)];
        const THICKS: [i32; 3] = [2, 3, 4];
        const RADIUS: i32 = 8;
        for (row, &thick) in THICKS.iter().enumerate() {
            for (col, &wedge) in WEDGES.iter().enumerate() {
                let (cx, panel_cz) = panel_center(col, row);
                // Centre the arc's apex in the panel's upper quarter so the
                // sector opens down into the panel.
                let cz = panel_cz - HH / 12;
                let arc = ArcSpec {
                    cx,
                    cz,
                    dir: (0, 1),
                    wedge,
                };
                paint_arc(&mut self.phases, RADIUS, RADIUS + 3, Phase::Warning, arc);
                paint_arc(&mut self.phases, RADIUS - thick, RADIUS, Phase::Danger, arc);
            }
        }
    }
}

/// Sub-cell center of preview panel `(col, row)` in a 3×3 grid over the field.
#[allow(clippy::cast_possible_truncation, clippy::cast_possible_wrap)] // col/row are 0..3
fn panel_center(col: usize, row: usize) -> (i32, i32) {
    let (pw, ph) = (HW / 3, HH / 3);
    (col as i32 * pw + pw / 2, row as i32 * ph + ph / 2)
}

/// Set a sub-cell to `phase`, with danger overriding an existing warning but a
/// warning never overwriting a danger. Out-of-bounds coords are ignored.
#[allow(clippy::cast_sign_loss)]
fn set(field: &mut [Phase], sx: i32, sz: i32, phase: Phase) {
    if !(0..HW).contains(&sx) || !(0..HH).contains(&sz) {
        return;
    }
    let i = (sz * HW + sx) as usize;
    if phase == Phase::Danger || field[i] == Phase::Safe {
        field[i] = phase;
    }
}

/// Paint the annulus `r_lo <= dist < r_hi` about a center with `phase`,
/// comparing squared distances so it stays integer-only.
fn paint_annulus(field: &mut [Phase], cx: i32, cz: i32, r_lo: i32, r_hi: i32, phase: Phase) {
    if r_hi <= 0 {
        return;
    }
    let r_lo = r_lo.max(0);
    let (lo2, hi2) = (r_lo * r_lo, r_hi * r_hi);
    for sz in (cz - r_hi).max(0)..=(cz + r_hi).min(HH - 1) {
        for sx in (cx - r_hi).max(0)..=(cx + r_hi).min(HW - 1) {
            let d2 = (sx - cx) * (sx - cx) + (sz - cz) * (sz - cz);
            if d2 >= lo2 && d2 < hi2 {
                set(field, sx, sz, phase);
            }
        }
    }
}

/// A wave arc: a sector of an annulus about `(cx, cz)` opening toward `dir`,
/// `wedge`-wide. `wedge` is a `(numerator, denominator)` bound on the slope
/// `|cross| / dot` from the facing axis — `(1, 1)` is a ±45° half-angle (a 90°
/// sector), `(1, 2)` narrows it (~±27°), `(2, 1)` widens it (~±63°).
#[derive(Clone, Copy)]
struct ArcSpec {
    cx: i32,
    cz: i32,
    dir: (i32, i32),
    wedge: (i32, i32),
}

/// Paint the annulus `r_lo <= dist < r_hi` about the arc's center, but only
/// within the sector facing `arc.dir` and no wider than `arc.wedge`. Integer
/// cross/dot, so it stays deterministic.
fn paint_arc(field: &mut [Phase], r_lo: i32, r_hi: i32, phase: Phase, arc: ArcSpec) {
    if r_hi <= 0 {
        return;
    }
    let ArcSpec {
        cx,
        cz,
        dir: (ux, uz),
        wedge: (wn, wd),
    } = arc;
    let r_lo = r_lo.max(0);
    let (lo2, hi2) = (r_lo * r_lo, r_hi * r_hi);
    for sz in (cz - r_hi).max(0)..=(cz + r_hi).min(HH - 1) {
        for sx in (cx - r_hi).max(0)..=(cx + r_hi).min(HW - 1) {
            let (dx, dz) = (sx - cx, sz - cz);
            let d2 = dx * dx + dz * dz;
            if d2 >= lo2 && d2 < hi2 {
                // On the facing side (dot > 0) and within the wedge:
                // |cross| / dot <= wn / wd, compared by cross-multiplication.
                let dot = dx * ux + dz * uz;
                let cross = dx * uz - dz * ux;
                if dot > 0 && cross.abs() * wd <= dot * wn {
                    set(field, sx, sz, phase);
                }
            }
        }
    }
}

/// Paint one full row (`horizontal`) or column at `pos` with `phase`, skipping
/// the `[gap_lo, gap_hi)` window on the spanning axis.
fn paint_line(
    field: &mut [Phase],
    horizontal: bool,
    pos: i32,
    gap_lo: i32,
    gap_hi: i32,
    phase: Phase,
) {
    if horizontal {
        for sx in 0..HW {
            if sx < gap_lo || sx >= gap_hi {
                set(field, sx, pos, phase);
            }
        }
    } else {
        for sz in 0..HH {
            if sz < gap_lo || sz >= gap_hi {
                set(field, pos, sz, phase);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_strikes_without_a_prior_warning() {
        // The core fairness invariant: a cell can only become Danger if it was
        // already Warning (or Danger) the previous tick — never straight from
        // Safe. This guards every shape's telegraph at once, including at spawn.
        let mut arena = Arena::new();
        let mut prev = arena.phases;
        for _ in 0..2_500 {
            arena.tick();
            for (i, (cur, was)) in arena.phases.iter().zip(prev.iter()).enumerate() {
                if *cur == Phase::Danger {
                    assert_ne!(*was, Phase::Safe, "cell {i} struck with no telegraph");
                }
            }
            prev = arena.phases;
        }
    }

    #[test]
    fn hazards_actually_strike() {
        // Sanity that the game isn't inert: over a run, cells do turn lethal.
        let mut arena = Arena::new();
        let mut ever_struck = false;
        for _ in 0..1_000 {
            arena.tick();
            if arena.phases.contains(&Phase::Danger) {
                ever_struck = true;
                break;
            }
        }
        assert!(ever_struck, "no hazard ever struck");
    }

    #[test]
    fn runs_are_deterministic() {
        let mut a = Arena::new();
        let mut b = Arena::new();
        for _ in 0..600 {
            a.tick();
            b.tick();
        }
        assert!(
            (0..HCELLS).all(|i| a.phases[i] == b.phases[i]),
            "two arenas from the same seed diverged"
        );
    }

    #[test]
    fn patterns_retire_so_the_field_does_not_fill_up() {
        // Patterns finish and are dropped, so the live set stays bounded and
        // hazards never accumulate without end.
        let cap = usize::try_from(CONCURRENT.1).expect("CONCURRENT.1 is a small positive constant");
        let mut arena = Arena::new();
        for _ in 0..3_000 {
            arena.tick();
            assert!(arena.patterns.len() <= cap);
        }
    }

    #[test]
    fn a_level_spawns_only_its_class() {
        // A level confines spawns to its one class — no mixing.
        let mut arena = Arena::new();
        arena.set_level(ShapeClass::Wall, 70);
        for _ in 0..2_500 {
            arena.tick();
            for pattern in &arena.patterns {
                assert!(
                    matches!(pattern.shape, Shape::Column { .. }),
                    "a non-wall pattern spawned in a wall level"
                );
            }
        }
    }
}
