//! `pcb-placer` — simulated annealing placement.
//!
//! Minimises HPWL (half-perimeter wire length: for each net, the
//! perimeter half of the axis-aligned bounding box covering its
//! footprints) plus a quadratic overlap penalty. Standard PCB
//! placement metric since the 1980s.
//!
//! Each `step()` runs a batch of `moves_per_step` annealing trials. A
//! trial picks an unlocked footprint, jitters its position (or swaps
//! it with another), recomputes the cost, and accepts the move if the
//! cost dropped or with Boltzmann probability `exp(-ΔE / T)` if it
//! rose. Temperature `T` decays exponentially from `t_initial` to
//! `t_final` over the configured `total_steps`. After moves the state
//! is legalised: clamped to bounds, then relaxed pair-wise so no two
//! footprints overlap.
//!
//! The frame the caller pulls from `step()` reflects the *current*
//! accepted state, not the best ever seen. We track best-so-far
//! separately and `current()` returns it after a final `finalise()`.

use std::collections::HashMap;

use pcb_core::{Footprint, Length, Point, Rect};

#[derive(Debug, Clone)]
pub struct PlacementInput {
    pub footprints: Vec<PlaceableFootprint>,
    /// References sharing each net. Used by HPWL.
    pub nets: Vec<Vec<String>>,
    pub bounds: Option<Rect>,
}

#[derive(Debug, Clone)]
pub struct PlaceableFootprint {
    pub reference: String,
    pub bbox_w: Length,
    pub bbox_h: Length,
    pub position: Point,
    pub locked: bool,
    pub footprint: Footprint,
}

#[derive(Debug, Clone)]
pub struct PlacementFrame {
    pub positions: HashMap<String, Point>,
    pub iteration: u32,
}

#[derive(Debug, Clone)]
pub struct PlacerOptions {
    /// Trial moves attempted per `step()` call.
    pub moves_per_step: u32,
    /// Total steps the caller plans to run; used to schedule the
    /// temperature ramp.
    pub total_steps: u32,
    /// Starting temperature. Higher = more uphill moves accepted at
    /// the start, more global exploration.
    pub t_initial: f64,
    /// Ending temperature. Lower = greedy refinement at the end.
    pub t_final: f64,
    /// Multiplier on overlap area (mm²) when summing into the cost.
    /// Tuned so even small overlaps dominate over HPWL gains.
    pub overlap_weight: f64,
    /// Maximum jitter in mm at T = t_initial. Shrinks with T so the
    /// late-stage refinement is gentle.
    pub jitter_scale_mm: f64,
    /// Probability of a swap-move vs a jitter-move per trial.
    pub swap_prob: f64,
}

impl Default for PlacerOptions {
    fn default() -> Self {
        Self {
            moves_per_step: 40,
            total_steps: 200,
            t_initial: 60.0,
            t_final: 0.02,
            overlap_weight: 500.0,
            jitter_scale_mm: 12.0,
            swap_prob: 0.20,
        }
    }
}

pub struct Placer {
    input: PlacementInput,
    opts: PlacerOptions,
    iteration: u32,
    /// Reference → footprint index for fast HPWL lookup.
    ref_idx: HashMap<String, usize>,
    /// Cached cost of the current state.
    cost: f64,
    /// Best (lowest-cost) state seen so far.
    best_positions: Vec<Point>,
    best_cost: f64,
    /// Linear congruential PRNG state. Stable, no external dep.
    rng: u64,
}

impl Placer {
    #[must_use]
    pub fn new(input: PlacementInput, opts: PlacerOptions) -> Self {
        let ref_idx: HashMap<String, usize> = input
            .footprints
            .iter()
            .enumerate()
            .map(|(i, fp)| (fp.reference.clone(), i))
            .collect();
        let positions: Vec<Point> = input.footprints.iter().map(|fp| fp.position).collect();
        let mut placer = Self {
            input,
            opts,
            iteration: 0,
            ref_idx,
            cost: 0.0,
            best_positions: positions,
            best_cost: f64::INFINITY,
            rng: 0xa5a5_a5a5_a5a5_a5a5,
        };
        placer.cost = placer.compute_cost();
        placer.best_cost = placer.cost;
        placer
    }

    /// Run a batch of trial moves and return a frame mirroring the
    /// current accepted state.
    pub fn step(&mut self) -> PlacementFrame {
        let t = self.temperature();
        for _ in 0..self.opts.moves_per_step {
            self.try_move(t);
        }
        // Legalise after the batch so the visible frame respects bounds
        // and has no obvious overlaps.
        self.legalise();
        // Recompute cost after legalisation since positions may have
        // shifted; update best-so-far.
        self.cost = self.compute_cost();
        if self.cost < self.best_cost {
            self.best_cost = self.cost;
            self.best_positions = self
                .input
                .footprints
                .iter()
                .map(|fp| fp.position)
                .collect();
        }
        self.iteration += 1;
        self.snapshot()
    }

    /// Restore the best-cost state seen and run a final legalisation.
    /// Call once after the last `step()`; the next `current()` reflects
    /// the answer to ship back.
    pub fn finalise(&mut self) {
        for (i, p) in self.best_positions.iter().enumerate() {
            self.input.footprints[i].position = *p;
        }
        self.legalise();
    }

    #[must_use]
    pub fn current(&self) -> &[PlaceableFootprint] {
        &self.input.footprints
    }

    fn temperature(&self) -> f64 {
        if self.opts.total_steps <= 1 {
            return self.opts.t_final;
        }
        // Exponential schedule: T_initial * (T_final / T_initial)^(p)
        // with p = iteration / (total_steps - 1).
        #[allow(clippy::cast_precision_loss)]
        let p = (self.iteration as f64) / (self.opts.total_steps as f64 - 1.0).max(1.0);
        let p = p.clamp(0.0, 1.0);
        let ratio = self.opts.t_final / self.opts.t_initial;
        self.opts.t_initial * ratio.powf(p)
    }

    fn try_move(&mut self, t: f64) {
        // Pick an unlocked target.
        let n = self.input.footprints.len();
        let mut idx = self.rand_index(n);
        for _ in 0..n {
            if !self.input.footprints[idx].locked {
                break;
            }
            idx = (idx + 1) % n;
        }
        if self.input.footprints[idx].locked {
            return; // Everyone's locked.
        }

        let move_kind_roll = self.rand_unit();
        let old_pos_a = self.input.footprints[idx].position;
        let mut old_pos_b: Option<(usize, Point)> = None;

        if move_kind_roll < self.opts.swap_prob {
            // Swap-move: exchange positions with another unlocked.
            let mut other = self.rand_index(n);
            for _ in 0..n {
                if other != idx && !self.input.footprints[other].locked {
                    break;
                }
                other = (other + 1) % n;
            }
            if other == idx || self.input.footprints[other].locked {
                return;
            }
            let pos_b = self.input.footprints[other].position;
            self.input.footprints[idx].position = pos_b;
            self.input.footprints[other].position = old_pos_a;
            old_pos_b = Some((other, pos_b));
        } else {
            // Jitter-move: gaussian step scaled by current temperature.
            let scale = self.opts.jitter_scale_mm * (t / self.opts.t_initial).sqrt();
            let dx = self.gaussian() * scale;
            let dy = self.gaussian() * scale;
            let new_pos = Point::new(
                Length::from_mm(old_pos_a.x.to_mm() + dx),
                Length::from_mm(old_pos_a.y.to_mm() + dy),
            );
            self.input.footprints[idx].position = self.clamp_one(idx, new_pos);
        }

        let new_cost = self.compute_cost();
        let delta = new_cost - self.cost;
        let accept = if delta <= 0.0 {
            true
        } else {
            self.rand_unit() < (-delta / t.max(1e-6)).exp()
        };

        if accept {
            self.cost = new_cost;
        } else {
            // Revert.
            self.input.footprints[idx].position = old_pos_a;
            if let Some((other, pos_b)) = old_pos_b {
                self.input.footprints[other].position = pos_b;
            }
        }
    }

    fn compute_cost(&self) -> f64 {
        let mut hpwl = 0.0;
        for net in &self.input.nets {
            if net.len() < 2 {
                continue;
            }
            let mut min_x = f64::INFINITY;
            let mut max_x = f64::NEG_INFINITY;
            let mut min_y = f64::INFINITY;
            let mut max_y = f64::NEG_INFINITY;
            for r in net {
                if let Some(&idx) = self.ref_idx.get(r) {
                    let p = self.input.footprints[idx].position;
                    let x = p.x.to_mm();
                    let y = p.y.to_mm();
                    min_x = min_x.min(x);
                    max_x = max_x.max(x);
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
                }
            }
            if min_x.is_finite() {
                hpwl += (max_x - min_x) + (max_y - min_y);
            }
        }
        let mut overlap = 0.0;
        let n = self.input.footprints.len();
        for i in 0..n {
            for j in (i + 1)..n {
                let a = &self.input.footprints[i];
                let b = &self.input.footprints[j];
                let dx = (a.position.x.to_mm() - b.position.x.to_mm()).abs();
                let dy = (a.position.y.to_mm() - b.position.y.to_mm()).abs();
                let half_w = (a.bbox_w.to_mm() + b.bbox_w.to_mm()) / 2.0 + 1.5; // pad
                let half_h = (a.bbox_h.to_mm() + b.bbox_h.to_mm()) / 2.0 + 1.5;
                let ox = (half_w - dx).max(0.0);
                let oy = (half_h - dy).max(0.0);
                overlap += ox * oy;
            }
        }
        hpwl + self.opts.overlap_weight * overlap
    }

    /// Clamp + pair-wise separation so the visible frame is legal.
    fn legalise(&mut self) {
        for _ in 0..3 {
            self.clamp_to_bounds();
            if !self.separate_pairs() {
                break;
            }
        }
        self.clamp_to_bounds();
    }

    fn clamp_one(&self, idx: usize, p: Point) -> Point {
        let Some(bounds) = self.input.bounds else {
            return p;
        };
        let fp = &self.input.footprints[idx];
        let edge_clearance = Length::from_mm(1.0);
        let half_w = fp.bbox_w / 2;
        let half_h = fp.bbox_h / 2;
        let min_x = bounds.min.x + half_w + edge_clearance;
        let max_x = bounds.max.x - half_w - edge_clearance;
        let min_y = bounds.min.y + half_h + edge_clearance;
        let max_y = bounds.max.y - half_h - edge_clearance;
        let (lo_x, hi_x) = if min_x.0 <= max_x.0 {
            (min_x.0, max_x.0)
        } else {
            let mid = (bounds.min.x.0 + bounds.max.x.0) / 2;
            (mid, mid)
        };
        let (lo_y, hi_y) = if min_y.0 <= max_y.0 {
            (min_y.0, max_y.0)
        } else {
            let mid = (bounds.min.y.0 + bounds.max.y.0) / 2;
            (mid, mid)
        };
        Point::new(
            Length(p.x.0.clamp(lo_x, hi_x)),
            Length(p.y.0.clamp(lo_y, hi_y)),
        )
    }

    fn clamp_to_bounds(&mut self) {
        if self.input.bounds.is_none() {
            return;
        }
        for i in 0..self.input.footprints.len() {
            if self.input.footprints[i].locked {
                continue;
            }
            let p = self.input.footprints[i].position;
            self.input.footprints[i].position = self.clamp_one(i, p);
        }
    }

    fn separate_pairs(&mut self) -> bool {
        let n = self.input.footprints.len();
        let pad = 1.5_f64;
        let mut any_moved = false;
        for _pass in 0..6 {
            let mut moved = false;
            for i in 0..n {
                for j in (i + 1)..n {
                    let a_locked = self.input.footprints[i].locked;
                    let b_locked = self.input.footprints[j].locked;
                    if a_locked && b_locked {
                        continue;
                    }
                    let ax = self.input.footprints[i].position.x.to_mm();
                    let ay = self.input.footprints[i].position.y.to_mm();
                    let bx = self.input.footprints[j].position.x.to_mm();
                    let by = self.input.footprints[j].position.y.to_mm();
                    let half_w = (self.input.footprints[i].bbox_w.to_mm()
                        + self.input.footprints[j].bbox_w.to_mm()) / 2.0
                        + pad;
                    let half_h = (self.input.footprints[i].bbox_h.to_mm()
                        + self.input.footprints[j].bbox_h.to_mm()) / 2.0
                        + pad;
                    let mut dx = ax - bx;
                    let mut dy = ay - by;
                    let overlap_x = half_w - dx.abs();
                    let overlap_y = half_h - dy.abs();
                    if overlap_x <= 0.0 || overlap_y <= 0.0 {
                        continue;
                    }
                    moved = true;
                    any_moved = true;
                    if dx.abs() < 1e-6 && dy.abs() < 1e-6 {
                        dx = 1.0;
                        dy = 0.0;
                    }
                    let (push_x, push_y) = if overlap_x < overlap_y {
                        (overlap_x * if dx >= 0.0 { 1.0 } else { -1.0 }, 0.0)
                    } else {
                        (0.0, overlap_y * if dy >= 0.0 { 1.0 } else { -1.0 })
                    };
                    let (a_share, b_share) = match (a_locked, b_locked) {
                        (false, false) => (0.5, 0.5),
                        (true, false) => (0.0, 1.0),
                        (false, true) => (1.0, 0.0),
                        (true, true) => unreachable!(),
                    };
                    if !a_locked {
                        let ax2 = ax + push_x * a_share;
                        let ay2 = ay + push_y * a_share;
                        self.input.footprints[i].position =
                            Point::new(Length::from_mm(ax2), Length::from_mm(ay2));
                    }
                    if !b_locked {
                        let bx2 = bx - push_x * b_share;
                        let by2 = by - push_y * b_share;
                        self.input.footprints[j].position =
                            Point::new(Length::from_mm(bx2), Length::from_mm(by2));
                    }
                }
            }
            if !moved {
                break;
            }
        }
        any_moved
    }

    fn snapshot(&self) -> PlacementFrame {
        let positions = self
            .input
            .footprints
            .iter()
            .map(|fp| (fp.reference.clone(), fp.position))
            .collect();
        PlacementFrame {
            positions,
            iteration: self.iteration,
        }
    }

    // -- PRNG ----------------------------------------------------

    fn next_u64(&mut self) -> u64 {
        // PCG-style step on a 64-bit LCG. Good enough for SA.
        self.rng = self
            .rng
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.rng
    }

    fn rand_unit(&mut self) -> f64 {
        // Use the high 53 bits as a uniform [0, 1).
        let bits = self.next_u64() >> 11;
        #[allow(clippy::cast_precision_loss)]
        let f = bits as f64;
        f / ((1u64 << 53) as f64)
    }

    fn rand_index(&mut self, n: usize) -> usize {
        if n == 0 { return 0; }
        #[allow(clippy::cast_possible_truncation)]
        let idx = (self.next_u64() % n as u64) as usize;
        idx
    }

    fn gaussian(&mut self) -> f64 {
        // Box-Muller; we only consume the first sample.
        let u1 = self.rand_unit().max(1e-12);
        let u2 = self.rand_unit();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}
