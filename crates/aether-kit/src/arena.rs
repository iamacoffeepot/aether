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

// Ring motion: the front advances one sub-cell of radius every
// `RING_TICKS_PER_STEP` ticks. Rings stop short of the arena so there is
// always a safe island — outward rings stop at `RING_OUT_MAX` (safe band
// outside), inward rings contract from `RING_IN_START` only to `RING_IN_MIN`
// (safe center).
const RING_TICKS_PER_STEP: i32 = 6;
const RING_THICK: i32 = 3;
const RING_OUT_MAX: i32 = 24;
const RING_IN_START: i32 = 40;
const RING_IN_MIN: i32 = 14;

// Column (swept wall) motion along one axis, with a gap to aim for.
const COLUMN_TICKS_PER_STEP: i32 = 5;
const COLUMN_THICK: i32 = 2;
/// Gap width in the wall, in sub-cells (~3 tiles of breathing room).
const COLUMN_GAP: i32 = 12;

// Wave: a single expanding arc — one sector of an outward ring.
const WAVE_TICKS_PER_STEP: i32 = 5;
const WAVE_THICK: i32 = 3;
const WAVE_MAX: i32 = 50;
/// Wave half-angle as a `(numerator, denominator)` slope bound on `|cross| /
/// dot` (see [`paint_arc`]): `(1, 1)` is a ±45° wedge, a 90° sector.
const WAVE_WEDGE: (i32, i32) = (1, 1);

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

/// Ticks between pattern spawns.
const SPAWN_INTERVAL_TICKS: u64 = 110;
/// Most patterns running at once.
const MAX_CONCURRENT: usize = 2;

/// Global speed is `speed_num / SPEED_DEN`: the arena clock advances that many
/// quarter-steps per tick, scaling motion, telegraph, and spawn rate together.
/// `SPEED_DEN` is 1×.
const SPEED_DEN: i32 = 4;
const SPEED_MIN: i32 = 1; // 0.25×
const SPEED_MAX: i32 = 16; // 4×
/// Adjustable wall-thickness bounds, in sub-cells. The ceiling is high enough
/// for a wall to read as a thick advancing slab (40 sub = 10 tiles, most of the
/// 16-tile field), not just a thin line.
const WALL_MIN: i32 = 1;
const WALL_MAX: i32 = 40;

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
    /// A single expanding arc — one ~90° sector of an outward ring, facing
    /// `(ux, uz)`. A radial wave to dodge out of the way of.
    Wave { cx: i32, cz: i32, ux: i32, uz: i32 },
}

/// One live pattern: a shape plus the ticks since it spawned.
struct Pattern {
    shape: Shape,
    age: i32,
}

impl Pattern {
    /// Whether the pattern has finished — its danger front has run off the end
    /// of its animation — and should be retired.
    fn done(&self, wall_thickness: i32) -> bool {
        match self.shape {
            Shape::Ring { inward, .. } => {
                let travel = if inward {
                    RING_IN_START - RING_IN_MIN
                } else {
                    RING_OUT_MAX
                };
                self.age > LEAD_TICKS + (travel + RING_THICK) * RING_TICKS_PER_STEP
            }
            Shape::Column { horizontal, .. } => {
                let span = if horizontal { HH } else { HW };
                self.age > LEAD_TICKS + (span + wall_thickness) * COLUMN_TICKS_PER_STEP
            }
            Shape::Wave { .. } => {
                self.age > LEAD_TICKS + (WAVE_MAX + WAVE_THICK) * WAVE_TICKS_PER_STEP
            }
        }
    }

    /// Paint this shape's warning band (current age) and danger front (age
    /// minus the lead) into the field.
    fn paint(&self, field: &mut [Phase], wall_thickness: i32) {
        let age = self.age;
        let danger_age = (age - LEAD_TICKS).max(0);
        match self.shape {
            Shape::Ring { cx, cz, inward } => {
                let wf = ring_radius(age, inward);
                let df = ring_radius(danger_age, inward);
                // Warning spans the whole band from the danger front to the
                // leading edge; danger is the thin front, painted only once the
                // lead has elapsed.
                let (lo, hi) = if inward {
                    (wf, df + RING_THICK)
                } else {
                    (df, wf + RING_THICK)
                };
                paint_annulus(field, cx, cz, lo, hi, Phase::Warning);
                if age >= LEAD_TICKS {
                    paint_annulus(field, cx, cz, df, df + RING_THICK, Phase::Danger);
                }
            }
            Shape::Column {
                horizontal,
                gap_lo,
                reverse,
            } => {
                let span = if horizontal { HH } else { HW };
                let wpos = column_pos(age, span, reverse);
                let dpos = column_pos(danger_age, span, reverse);
                let dir = if reverse { -1 } else { 1 };
                let gap_hi = gap_lo + COLUMN_GAP;
                let (lo, hi) = if wpos <= dpos {
                    (wpos, dpos)
                } else {
                    (dpos, wpos)
                };
                for pos in lo..=hi {
                    paint_line(field, horizontal, pos, gap_lo, gap_hi, Phase::Warning);
                }
                if age >= LEAD_TICKS {
                    for k in 0..wall_thickness {
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
                let wf = (age / WAVE_TICKS_PER_STEP).min(WAVE_MAX);
                let df = (danger_age / WAVE_TICKS_PER_STEP).min(WAVE_MAX);
                let arc = ArcSpec {
                    cx,
                    cz,
                    dir: (ux, uz),
                    wedge: WAVE_WEDGE,
                };
                paint_arc(field, df, wf + WAVE_THICK, Phase::Warning, arc);
                if age >= LEAD_TICKS {
                    paint_arc(field, df, df + WAVE_THICK, Phase::Danger, arc);
                }
            }
        }
    }
}

/// Ring front radius at a given age, clamped to its safe-island limit:
/// outward grows from 0 up to `RING_OUT_MAX`, inward shrinks from
/// `RING_IN_START` down to `RING_IN_MIN`.
fn ring_radius(age: i32, inward: bool) -> i32 {
    let step = age / RING_TICKS_PER_STEP;
    if inward {
        (RING_IN_START - step).max(RING_IN_MIN)
    } else {
        step.min(RING_OUT_MAX)
    }
}

/// Column front position at a given age along a `span`-long axis.
fn column_pos(age: i32, span: i32, reverse: bool) -> i32 {
    let step = age / COLUMN_TICKS_PER_STEP;
    if reverse { span - 1 - step } else { step }
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
    /// Swept-wall thickness, in sub-cells.
    wall_thickness: i32,
}

impl Arena {
    pub fn new() -> Self {
        Self {
            phases: [Phase::Safe; HCELLS],
            patterns: Vec::new(),
            // Fixed non-zero seed: every run faces the same sequence, which is
            // both fair across players and exactly replayable.
            rng: 0x2545_F491_4F6C_DD1D,
            elapsed: 0,
            speed_num: SPEED_DEN,
            time_accum: 0,
            wall_thickness: COLUMN_THICK,
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
            pattern.paint(&mut self.phases, self.wall_thickness);
        }
    }

    /// One logical step: spawn on cadence, age the patterns, retire finished
    /// ones.
    fn step(&mut self) {
        if self.elapsed.is_multiple_of(SPAWN_INTERVAL_TICKS) && self.patterns.len() < MAX_CONCURRENT
        {
            self.spawn();
        }
        for pattern in &mut self.patterns {
            pattern.age += 1;
        }
        let wall = self.wall_thickness;
        self.patterns.retain(|pattern| !pattern.done(wall));
        self.elapsed += 1;
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

    /// Thicken / thin the swept walls; returns the new thickness in sub-cells.
    pub fn walls_thicker(&mut self) -> i32 {
        self.wall_thickness = (self.wall_thickness + 1).min(WALL_MAX);
        self.wall_thickness
    }

    pub fn walls_thinner(&mut self) -> i32 {
        self.wall_thickness = (self.wall_thickness - 1).max(WALL_MIN);
        self.wall_thickness
    }

    /// Pick and place a fresh pattern from the RNG.
    fn spawn(&mut self) {
        let shape = match self.next_rng() % 5 {
            // Expanding ring: dodge the band.
            0 => Shape::Ring {
                cx: self.rand_between(SUB * 4, HW - SUB * 4),
                cz: self.rand_between(SUB * 4, HH - SUB * 4),
                inward: false,
            },
            // Collapsing ring: get to the center before it closes.
            1 => Shape::Ring {
                cx: self.rand_between(SUB * 4, HW - SUB * 4),
                cz: self.rand_between(SUB * 4, HH - SUB * 4),
                inward: true,
            },
            2 => Shape::Column {
                horizontal: true,
                gap_lo: self.rand_between(0, HW - COLUMN_GAP),
                reverse: self.next_rng().is_multiple_of(2),
            },
            3 => Shape::Column {
                horizontal: false,
                gap_lo: self.rand_between(0, HH - COLUMN_GAP),
                reverse: self.next_rng().is_multiple_of(2),
            },
            // Single expanding arc — a radial wave.
            _ => {
                let (ux, uz) = self.rand_dir();
                Shape::Wave {
                    cx: self.rand_between(SUB * 4, HW - SUB * 4),
                    cz: self.rand_between(SUB * 4, HH - SUB * 4),
                    ux,
                    uz,
                }
            }
        };
        self.patterns.push(Pattern { shape, age: 0 });
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
                paint_annulus(&mut self.phases, cx, cz, radius - thick, radius, Phase::Danger);
            }
        }
    }

    /// Walls: a wall spans the whole arena, so it reads as a barrier rather than
    /// a panel tile. Three full-width walls stacked down the field, thickness
    /// growing top to bottom at a fixed gap; orange leads downward (`+z`), the
    /// sweep direction. Each wall is anchored at the top of its band so the
    /// thick one grows down into its own space without overlapping the next.
    #[allow(clippy::cast_possible_wrap)] // row is a 0..3 loop index
    fn paint_wall_matrix(&mut self) {
        const THICKS: [i32; 3] = [4, 10, 16];
        const GAP: i32 = 10;
        let gap_lo = HW / 2 - GAP / 2;
        let gap_hi = HW / 2 + GAP / 2;
        let band = HH / 3;
        for (row, &thick) in THICKS.iter().enumerate() {
            let z0 = row as i32 * band + 1;
            for k in 0..thick {
                paint_line(&mut self.phases, true, z0 + k, gap_lo, gap_hi, Phase::Danger);
            }
            for k in thick..(thick + 3) {
                paint_line(&mut self.phases, true, z0 + k, gap_lo, gap_hi, Phase::Warning);
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
#[allow(clippy::cast_possible_wrap)] // col/row are 0..3 loop indices
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
        let mut arena = Arena::new();
        for _ in 0..3_000 {
            arena.tick();
            assert!(arena.patterns.len() <= MAX_CONCURRENT);
        }
    }
}
