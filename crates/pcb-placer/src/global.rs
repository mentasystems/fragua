//! ePlace-style electrostatic global placement.
//!
//! Casts placement as an electrostatic system (ePlace, Lu et al. 2015;
//! the same formulation DREAMPlace accelerates on GPU): every footprint
//! is a positive charge whose magnitude is its inflated body area, the
//! bin-wise charge density drives a Poisson equation ∇²ψ = -ρ solved
//! spectrally (DCT), and the resulting field E = -∇ψ pushes overlapping
//! parts apart. Wirelength is the weighted-average (WA) smooth model of
//! HPWL, so the total objective f = WL + λ·Φ has an exact gradient and
//! is minimised with Nesterov's accelerated gradient using a
//! Barzilai-Borwein step. λ starts where the two gradients balance and
//! grows geometrically until the density overflow drops under target —
//! parts cluster for wirelength but never pile up.
//!
//! The result intentionally still has small residual overlaps: global
//! placement finds the *structure*; the SA stage in `lib.rs` then
//! legalises against the hard solder-gap floor and polishes rotations.
//! Deterministic: no RNG anywhere in this phase.

use std::collections::HashMap;

use pcb_core::{Board, Id, Length, Point, Rect};

use crate::{fp_bounds_with_margin, MarginMap, PlaceOptions};

/// Outcome of the global phase, folded into `PlaceReport`.
#[derive(Debug, Clone, Default)]
pub struct GlobalReport {
    /// Gradient iterations actually run.
    pub iterations: usize,
    /// Final density overflow, fraction of total movable charge sitting
    /// above the target density (0 = perfectly spread, ~0.08 = done).
    pub overflow: f64,
    /// Raw (unweighted) HPWL after the global phase, mm.
    pub hpwl_mm: f64,
}

/// A movable footprint's geometry cached in mm, world frame. Offsets
/// are relative to `Footprint::position` and constant during the phase
/// (rotation is untouched here — the SA stage owns 90° flips).
struct Body {
    id: Id,
    /// Inflated bbox offsets from position: margin + solder_gap/2 per
    /// side. Density and outline containment use these. (Edge locking
    /// uses the RAW pad bbox instead, matching
    /// `Board::edge_mount_violation` — resolved once at `Body` build.)
    inf_min: [f64; 2],
    inf_max: [f64; 2],
    /// Electrostatic charge = inflated bbox area, mm².
    charge: f64,
    /// Axis whose coordinate is frozen so the part keeps touching its
    /// outline edge (edge-mounted connectors): `(axis, fixed_value)`.
    edge_lock: Option<(usize, f64)>,
    /// Degrees CCW the rotation probe has added on top of the
    /// footprint's original rotation; written back at the end.
    rot_delta: f32,
}

/// One pin of a net: either a fixed absolute position or an offset from
/// a movable body's position.
enum Pin {
    Fixed([f64; 2]),
    Mov { body: usize, off: [f64; 2] },
}

struct Net {
    pins: Vec<Pin>,
    /// Same clique weight the SA score uses: 4/(n_pads-1).
    weight: f64,
}

pub(crate) fn global_place(
    board: &mut Board,
    movable_ids: &[Id],
    opts: &PlaceOptions,
    margins: &MarginMap,
    outline: Rect,
) -> GlobalReport {
    let ox = outline.min.x.to_mm();
    let oy = outline.min.y.to_mm();
    let w_mm = outline.width().to_mm();
    let h_mm = outline.height().to_mm();
    if w_mm <= 1.0 || h_mm <= 1.0 || movable_ids.is_empty() {
        return GlobalReport {
            hpwl_mm: crate::total_hpwl(board),
            ..GlobalReport::default()
        };
    }

    // --- Cache movable bodies -------------------------------------------------
    let half_gap = opts.min_clearance_mm.max(opts.solder_gap_mm) / 2.0;
    let mut bodies: Vec<Body> = Vec::new();
    let mut body_of_id: HashMap<Id, usize> = HashMap::new();
    let mut pos: Vec<[f64; 2]> = Vec::new();
    for id in movable_ids {
        let Some(fp) = board.footprints.get(id) else {
            continue;
        };
        let Some(raw) = fp.bounds() else { continue };
        let inf = fp_bounds_with_margin(fp, margins).unwrap_or(raw);
        let px = fp.position.x.to_mm();
        let py = fp.position.y.to_mm();
        let mut inf_min = [
            inf.min.x.to_mm() - px - half_gap,
            inf.min.y.to_mm() - py - half_gap,
        ];
        let mut inf_max = [
            inf.max.x.to_mm() - px + half_gap,
            inf.max.y.to_mm() - py + half_gap,
        ];
        let raw_min = [raw.min.x.to_mm() - px, raw.min.y.to_mm() - py];
        let raw_max = [raw.max.x.to_mm() - px, raw.max.y.to_mm() - py];
        let charge = (inf_max[0] - inf_min[0]).max(0.01) * (inf_max[1] - inf_min[1]).max(0.01);
        let mut pre_q = 0u8;
        let edge_lock = if fp.edge_mounted {
            let (q, axis, value) = edge_plan(
                px, py, raw_min, raw_max, inf_min, inf_max, ox, oy, w_mm, h_mm,
            );
            pre_q = q;
            if q != 0 {
                let (nmin, nmax) = rot_bbox(inf_min, inf_max, q);
                inf_min = nmin;
                inf_max = nmax;
            }
            Some((axis, value))
        } else {
            None
        };
        body_of_id.insert(*id, bodies.len());
        bodies.push(Body {
            id: *id,
            inf_min,
            inf_max,
            charge,
            edge_lock,
            rot_delta: 90.0 * f32::from(pre_q),
        });
        pos.push([px, py]);
    }
    if bodies.is_empty() {
        return GlobalReport {
            hpwl_mm: crate::total_hpwl(board),
            ..GlobalReport::default()
        };
    }

    // --- Nets (pins split into fixed / movable) -------------------------------
    // `footprints_in_order` everywhere: HashMap-order iteration would
    // vary the float-summation order call to call and make the phase
    // non-deterministic (no RNG needed — summation order is enough).
    let mut net_pins: HashMap<&str, Vec<Pin>> = HashMap::new();
    let in_order: Vec<&pcb_core::Footprint> = board.footprints_in_order().collect();
    for fp in &in_order {
        let mov = body_of_id.get(&fp.id).copied();
        let px = fp.position.x.to_mm();
        let py = fp.position.y.to_mm();
        for pad in &fp.pads {
            let Some(net) = pad.net.as_deref() else {
                continue;
            };
            let c = fp.pad_world_center(pad);
            let pin = match mov {
                Some(b) => {
                    // Fold in any edge-plan pre-rotation chosen at Body
                    // build (rot_delta is always a multiple of 90 here).
                    let q = ((bodies[b].rot_delta / 90.0).round() as i32).rem_euclid(4) as u8;
                    Pin::Mov {
                        body: b,
                        off: rot_off([c.x.to_mm() - px, c.y.to_mm() - py], q),
                    }
                }
                None => Pin::Fixed([c.x.to_mm(), c.y.to_mm()]),
            };
            net_pins.entry(net).or_default().push(pin);
        }
    }
    // Sort by net name: HashMap iteration order would otherwise change
    // the float-summation order run to run and break the "same input →
    // same placement" guarantee the SA stage gets from its seed.
    let mut named: Vec<(&str, Vec<Pin>)> = net_pins.into_iter().collect();
    named.sort_by_key(|(name, _)| *name);
    let mut nets: Vec<Net> = named
        .into_iter()
        .map(|(_, pins)| pins)
        .filter(|pins| pins.len() >= 2 && pins.iter().any(|p| matches!(p, Pin::Mov { .. })))
        .map(|pins| {
            let weight = 4.0 / ((pins.len() - 1) as f64);
            Net { pins, weight }
        })
        .collect();
    // Which nets touch each body — the rotation probe re-evaluates only
    // these.
    let mut nets_of_body: Vec<Vec<usize>> = vec![Vec::new(); bodies.len()];
    for (ni, net) in nets.iter().enumerate() {
        for pin in &net.pins {
            if let Pin::Mov { body, .. } = pin {
                if nets_of_body[*body].last() != Some(&ni) {
                    nets_of_body[*body].push(ni);
                }
            }
        }
    }

    // --- Density grid + spectral Poisson solver -------------------------------
    let m = opts.density_bins.clamp(16, 256);
    let mut grid = FieldGrid::new(m, m, ox, oy, w_mm, h_mm);
    // Fixed (pinned) footprints are immovable charge: movables flow
    // around them instead of through them.
    let mut fixed_rho = vec![0.0f64; m * m];
    for fp in &in_order {
        if body_of_id.contains_key(&fp.id) {
            continue;
        }
        let Some(b) = fp_bounds_with_margin(fp, margins) else {
            continue;
        };
        grid.splat(
            &mut fixed_rho,
            b.min.x.to_mm() - half_gap,
            b.min.y.to_mm() - half_gap,
            b.max.x.to_mm() + half_gap,
            b.max.y.to_mm() + half_gap,
        );
    }
    let total_movable_charge: f64 = bodies.iter().map(|b| b.charge).sum();
    // Overflow contributed by pinned parts alone (a dense fixed region
    // can exceed the target density all by itself). Subtracting this
    // baseline keeps the movable overflow reachable, so λ stops growing
    // once the *movable* parts are spread — instead of exploding while
    // chasing an overflow the movables can't fix.
    let fixed_overflow = {
        let g = FieldGrid::new(m, m, ox, oy, w_mm, h_mm);
        g.overflow(&fixed_rho, opts.target_density, total_movable_charge)
    };

    // --- Optimiser state ------------------------------------------------------
    let n = bodies.len();
    let iters = opts.global_iterations.max(1);
    // WA smoothing width: start coarse (4 % of the long side) so distant
    // pins still see a gradient, anneal towards HPWL sharpness.
    let mut gamma = (0.04 * w_mm.max(h_mm)).max(1.0);
    let gamma_floor = 0.5;
    let overflow_target = opts.target_overflow.max(0.005);

    let edge_clearance = opts.edge_clearance_mm.max(0.0);
    let project =
        |p: &mut [f64; 2], b: &Body| project_position(p, b, ox, oy, w_mm, h_mm, edge_clearance);
    for (p, b) in pos.iter_mut().zip(&bodies) {
        project(p, b);
    }

    // λ runs in two regimes, mirroring ePlace's schedule but starting
    // from an arbitrary (possibly fully spread) layout instead of an
    // everything-at-the-centre one:
    //  - collapse: while overflow is under target the layout is legal,
    //    so wirelength dominates (λ = 5 % of the balance ratio) and the
    //    parts fall toward their nets;
    //  - spread: the first time overflow crosses the target, λ freezes
    //    at the ePlace balance ratio Σ|∇WL|/Σ|∇Φ| and then grows 5 %
    //    per iteration until the overflow is resolved again.
    let mut lambda = 0.0f64;
    let mut lambda_frozen = false;
    let mut x = pos.clone(); // major solution
    let mut y = pos.clone(); // Nesterov reference
    let mut y_prev: Vec<[f64; 2]> = y.clone();
    let mut g_prev: Vec<[f64; 2]> = vec![[0.0; 2]; n];
    let mut a_k = 1.0f64;
    let max_move = 2.0 * (grid.hx.max(grid.hy));
    let mut ran = 0usize;
    let mut overflow = f64::INFINITY;
    // Plateau detector: stop once the raw HPWL hasn't improved by 0.2 %
    // over the trailing 50 iterations while the layout is legal.
    let mut plateau_best = f64::INFINITY;
    let mut plateau_age = 0usize;

    for k in 0..iters {
        ran = k + 1;
        // Density + field at the reference point.
        let mut rho = fixed_rho.clone();
        for (p, b) in y.iter().zip(&bodies) {
            grid.splat(
                &mut rho,
                p[0] + b.inf_min[0],
                p[1] + b.inf_min[1],
                p[0] + b.inf_max[0],
                p[1] + b.inf_max[1],
            );
        }
        grid.solve(&rho);
        overflow = (grid.overflow(&rho, opts.target_density, total_movable_charge)
            - fixed_overflow)
            .max(0.0);

        // Gradient of f = Σ_e w_e·WA_e + λ·Φ at y.
        let mut g_wl = vec![[0.0f64; 2]; n];
        for net in &nets {
            accumulate_wa_gradient(net, &y, gamma, &mut g_wl);
        }
        let mut g_d = vec![[0.0f64; 2]; n];
        for (i, b) in bodies.iter().enumerate() {
            let (ex, ey) = grid.field_over(
                y[i][0] + b.inf_min[0],
                y[i][1] + b.inf_min[1],
                y[i][0] + b.inf_max[0],
                y[i][1] + b.inf_max[1],
            );
            // ∂Φ/∂x = -q·Ex: energy falls when the charge moves with the field.
            g_d[i] = [-b.charge * ex, -b.charge * ey];
        }
        let s_wl: f64 = g_wl.iter().map(|g| g[0].abs() + g[1].abs()).sum();
        let s_d: f64 = g_d.iter().map(|g| g[0].abs() + g[1].abs()).sum();
        let balance = if s_d > 1e-12 {
            (s_wl / s_d).max(1e-9)
        } else {
            0.0
        };
        if !lambda_frozen {
            if overflow > overflow_target && balance > 0.0 {
                lambda = balance;
                lambda_frozen = true;
            } else {
                // Collapse regime: wirelength dominates, density only
                // whispers so parts can flow through each other.
                lambda = 0.05 * balance;
            }
        } else if overflow > overflow_target {
            lambda *= 1.06;
        } else {
            // Feedback rather than a ratchet: once the layout is legal
            // again, relax λ so wirelength can keep compressing. A
            // monotone λ (classic ePlace) is right when you *start*
            // overlapped and stop at first legality, but this loop
            // lives past legality — a frozen-high λ would let a few
            // near-empty bins outvote real wirelength gains forever.
            lambda *= 0.96;
        }
        if lambda_frozen && balance > 0.0 {
            lambda = lambda.clamp(0.01 * balance, 100.0 * balance);
        }
        let mut g = vec![[0.0f64; 2]; n];
        for i in 0..n {
            g[i] = [
                g_wl[i][0] + lambda * g_d[i][0],
                g_wl[i][1] + lambda * g_d[i][1],
            ];
            if let Some((axis, _)) = bodies[i].edge_lock {
                g[i][axis] = 0.0;
            }
        }

        // Barzilai-Borwein step (BB2), clamped so no part teleports.
        let mut sy = 0.0f64;
        let mut ss = 0.0f64;
        for i in 0..n {
            for d in 0..2 {
                let dy = y[i][d] - y_prev[i][d];
                let dg = g[i][d] - g_prev[i][d];
                sy += dy * dg;
                ss += dg * dg;
            }
        }
        let g_inf = g
            .iter()
            .map(|v| v[0].abs().max(v[1].abs()))
            .fold(0.0f64, f64::max);
        let alpha = if k > 0 && ss > 1e-12 && sy > 0.0 {
            sy / ss
        } else if g_inf > 1e-12 {
            // First step: move the steepest part by one bin.
            grid.hx.min(grid.hy) / g_inf
        } else {
            0.0
        };

        y_prev.copy_from_slice(&y);
        g_prev.copy_from_slice(&g);

        // Nesterov update. The trust region is per part, not a global
        // step clamp: clamping α by the steepest gradient would freeze
        // weak-gradient parts (a lone passive 60 mm from its net would
        // crawl and never arrive); instead every part takes the BB step
        // and its own displacement is capped at `max_move`.
        let mut x_new = vec![[0.0f64; 2]; n];
        for i in 0..n {
            let dx = (alpha * g[i][0]).clamp(-max_move, max_move);
            let dy = (alpha * g[i][1]).clamp(-max_move, max_move);
            x_new[i] = [y[i][0] - dx, y[i][1] - dy];
            project(&mut x_new[i], &bodies[i]);
        }
        let a_next = 0.5 * (1.0 + (4.0 * a_k * a_k + 1.0).sqrt());
        let coef = (a_k - 1.0) / a_next;
        for i in 0..n {
            y[i] = [
                x_new[i][0] + coef * (x_new[i][0] - x[i][0]),
                x_new[i][1] + coef * (x_new[i][1] - x[i][1]),
            ];
            project(&mut y[i], &bodies[i]);
        }
        x = x_new;
        a_k = a_next;

        // Rotation probe — the PCB twist ePlace doesn't have (VLSI
        // cells don't rotate, modules and passives do). Every 25
        // iterations, try the three other 90° orientations of each
        // free body against the WA objective of just its nets and keep
        // the best. While parts still float this fixes orientation far
        // more cheaply than the SA stage can later, when the layout is
        // already packed and most rotations collide.
        if k % 25 == 24 {
            for i in 0..n {
                if bodies[i].edge_lock.is_some() {
                    continue;
                }
                let base: f64 = nets_of_body[i]
                    .iter()
                    .map(|&ni| wa_of_net(&nets[ni], &x, gamma, None))
                    .sum();
                let mut best_q = 0u8;
                let mut best_val = base;
                for q in 1u8..4 {
                    let val: f64 = nets_of_body[i]
                        .iter()
                        .map(|&ni| wa_of_net(&nets[ni], &x, gamma, Some((i, q))))
                        .sum();
                    if val < best_val - 1e-9 {
                        best_val = val;
                        best_q = q;
                    }
                }
                if best_q != 0 {
                    for &ni in &nets_of_body[i] {
                        for pin in &mut nets[ni].pins {
                            if let Pin::Mov { body, off } = pin {
                                if *body == i {
                                    *off = rot_off(*off, best_q);
                                }
                            }
                        }
                    }
                    let (nmin, nmax) = rot_bbox(bodies[i].inf_min, bodies[i].inf_max, best_q);
                    bodies[i].inf_min = nmin;
                    bodies[i].inf_max = nmax;
                    bodies[i].rot_delta = (bodies[i].rot_delta + 90.0 * f32::from(best_q)) % 360.0;
                    // Extents changed: re-project and drop this body's
                    // stale momentum by re-basing the reference on it.
                    project(&mut x[i], &bodies[i]);
                    y[i] = x[i];
                }
            }
        }

        gamma = (gamma * 0.985).max(gamma_floor);

        // Convergence: legal density + no meaningful HPWL progress.
        if overflow <= overflow_target {
            // Cheap raw-HPWL probe on the major solution.
            let hpwl = {
                for (p, b) in x.iter().zip(&bodies) {
                    if let Some(fp) = board.footprints.get_mut(&b.id) {
                        fp.position = Point::new(Length::from_mm(p[0]), Length::from_mm(p[1]));
                    }
                }
                crate::total_hpwl(board)
            };
            if hpwl < plateau_best * 0.998 {
                plateau_best = hpwl;
                plateau_age = 0;
            } else {
                plateau_age += 1;
            }
            if plateau_age >= 50 && k >= 100 {
                break;
            }
        } else {
            plateau_age = 0;
        }
    }

    // Legalisation spread: wirelength off, pure density descent until
    // the layout is (near) legal. The Nesterov loop above may stop mid
    // λ-oscillation with real overlap left, and the SA stage cannot dig
    // parts out of deep overlap — separating raises HPWL, which
    // Metropolis rejects at refinement temperatures. A few dozen pure
    // density steps guarantee the handoff is feasible; the SA re-wins
    // any wirelength these steps give up.
    for _ in 0..200 {
        let mut rho = fixed_rho.clone();
        for (p, b) in x.iter().zip(&bodies) {
            grid.splat(
                &mut rho,
                p[0] + b.inf_min[0],
                p[1] + b.inf_min[1],
                p[0] + b.inf_max[0],
                p[1] + b.inf_max[1],
            );
        }
        grid.solve(&rho);
        overflow = (grid.overflow(&rho, opts.target_density, total_movable_charge)
            - fixed_overflow)
            .max(0.0);
        if overflow <= 0.25 * overflow_target {
            break;
        }
        let mut g_inf = 0.0f64;
        let mut g = vec![[0.0f64; 2]; n];
        for (i, b) in bodies.iter().enumerate() {
            let (ex, ey) = grid.field_over(
                x[i][0] + b.inf_min[0],
                x[i][1] + b.inf_min[1],
                x[i][0] + b.inf_max[0],
                x[i][1] + b.inf_max[1],
            );
            g[i] = [-b.charge * ex, -b.charge * ey];
            if let Some((axis, _)) = b.edge_lock {
                g[i][axis] = 0.0;
            }
            g_inf = g_inf.max(g[i][0].abs()).max(g[i][1].abs());
        }
        if g_inf < 1e-12 {
            break;
        }
        // Steepest part moves exactly one bin per step: monotone,
        // overshoot-free spreading.
        let alpha = grid.hx.min(grid.hy) / g_inf;
        for i in 0..n {
            x[i] = [x[i][0] - alpha * g[i][0], x[i][1] - alpha * g[i][1]];
            project(&mut x[i], &bodies[i]);
        }
    }

    // Write the solution back (nm fixed-point), rotations included.
    for (p, b) in x.iter().zip(&bodies) {
        if let Some(fp) = board.footprints.get_mut(&b.id) {
            fp.position = Point::new(Length::from_mm(p[0]), Length::from_mm(p[1]));
            if b.rot_delta != 0.0 {
                fp.rotation = (fp.rotation + b.rot_delta).rem_euclid(360.0);
            }
        }
    }

    GlobalReport {
        iterations: ran,
        overflow,
        hpwl_mm: crate::total_hpwl(board),
    }
}

/// Clamp a candidate position so the inflated bbox stays inside the
/// outline; a locked axis snaps back to its edge value. Free (non
/// edge-mounted) parts keep an extra `EDGE_CLEARANCE_MM` so their pads
/// don't end up kissing the cut line the DRC's edge check flags.
fn project_position(p: &mut [f64; 2], b: &Body, ox: f64, oy: f64, w: f64, h: f64, edge: f64) {
    let e = if b.edge_lock.is_some() { 0.0 } else { edge };
    let lo_x = ox + e - b.inf_min[0];
    let hi_x = ox + w - e - b.inf_max[0];
    let lo_y = oy + e - b.inf_min[1];
    let hi_y = oy + h - e - b.inf_max[1];
    // A part wider than the board still gets a sane centred clamp.
    p[0] = if lo_x <= hi_x {
        p[0].clamp(lo_x, hi_x)
    } else {
        f64::midpoint(lo_x, hi_x)
    };
    p[1] = if lo_y <= hi_y {
        p[1].clamp(lo_y, hi_y)
    } else {
        f64::midpoint(lo_y, hi_y)
    };
    if let Some((axis, v)) = b.edge_lock {
        p[axis] = v;
    }
}

/// Plan an edge-mounted part's edge, orientation and back-off.
///
/// The edge is the one nearest to the part's RAW pad bbox. The
/// orientation (0/90/180/270) is the one whose *body* margin on the
/// outboard side is smallest — an ESP32 module has 1.84 mm of plastic
/// beyond its pads on two sides and 0.63 mm on the others; only the
/// 0.63 mm sides may face the cut line or the body hangs off the board.
/// The pads are then backed off the edge by up to the 0.5 mm touch
/// tolerance so the body overhang shrinks under the 0.5 mm the DRC
/// allows. Returns `(quarter, axis, lock_value)` where `lock_value` is
/// the position coordinate on `axis` that realises the plan.
fn edge_plan(
    px: f64,
    py: f64,
    raw_min: [f64; 2],
    raw_max: [f64; 2],
    inf_min: [f64; 2],
    inf_max: [f64; 2],
    ox: f64,
    oy: f64,
    w: f64,
    h: f64,
) -> (u8, usize, f64) {
    // Matches pcb-core's EDGE_TOUCH_TOLERANCE_MM and the 0.5 mm body
    // overhang the DRC's body_off_board check tolerates.
    const TOUCH_TOL: f64 = 0.5;
    const OVERHANG_TOL: f64 = 0.5;

    let d_left = (px + raw_min[0] - ox).abs();
    let d_right = (ox + w - (px + raw_max[0])).abs();
    let d_bottom = (py + raw_min[1] - oy).abs();
    let d_top = (oy + h - (py + raw_max[1])).abs();
    // Edge id: 0 = left, 1 = right, 2 = bottom, 3 = top.
    let dists = [d_left, d_right, d_bottom, d_top];
    let edge = (0..4)
        .min_by(|&a, &b| dists[a].total_cmp(&dists[b]))
        .unwrap_or(0);

    let mut chosen = (0u8, 0.0f64, f64::INFINITY); // (q, backoff, residual)
    for q in 0u8..4 {
        let (rmin, rmax) = rot_bbox(raw_min, raw_max, q);
        let (imin, imax) = rot_bbox(inf_min, inf_max, q);
        // Body margin on the outboard side when the pads touch the edge.
        let m_out = match edge {
            0 => rmin[0] - imin[0],
            1 => imax[0] - rmax[0],
            2 => rmin[1] - imin[1],
            _ => imax[1] - rmax[1],
        };
        let backoff = (m_out - OVERHANG_TOL).clamp(0.0, TOUCH_TOL);
        let residual = (m_out - OVERHANG_TOL - backoff).max(0.0);
        if residual < chosen.2 - 1e-9 {
            chosen = (q, backoff, residual);
        }
    }
    let (q, backoff, _) = chosen;
    let (rmin, rmax) = rot_bbox(raw_min, raw_max, q);
    match edge {
        0 => (q, 0, ox - rmin[0] + backoff),
        1 => (q, 0, ox + w - rmax[0] - backoff),
        2 => (q, 1, oy - rmin[1] + backoff),
        _ => (q, 1, oy + h - rmax[1] - backoff),
    }
}

/// Rotate a footprint-local world offset by `quarter` × 90° CCW.
fn rot_off(off: [f64; 2], quarter: u8) -> [f64; 2] {
    match quarter % 4 {
        1 => [-off[1], off[0]],
        2 => [-off[0], -off[1]],
        3 => [off[1], -off[0]],
        _ => off,
    }
}

/// Rotate a bbox given as min/max offsets by `quarter` × 90° CCW.
fn rot_bbox(min: [f64; 2], max: [f64; 2], quarter: u8) -> ([f64; 2], [f64; 2]) {
    let corners = [
        [min[0], min[1]],
        [min[0], max[1]],
        [max[0], min[1]],
        [max[0], max[1]],
    ];
    let mut nmin = [f64::INFINITY; 2];
    let mut nmax = [f64::NEG_INFINITY; 2];
    for c in corners {
        let r = rot_off(c, quarter);
        for d in 0..2 {
            nmin[d] = nmin[d].min(r[d]);
            nmax[d] = nmax[d].max(r[d]);
        }
    }
    (nmin, nmax)
}

/// Weighted-average wirelength VALUE of one net (both axes), with an
/// optional hypothetical rotation `(body, quarter)` applied to that
/// body's pins. Used by the rotation probe; the optimiser itself only
/// needs the gradient.
fn wa_of_net(net: &Net, pos: &[[f64; 2]], gamma: f64, rotated: Option<(usize, u8)>) -> f64 {
    let coords: Vec<[f64; 2]> = net
        .pins
        .iter()
        .map(|p| match p {
            Pin::Fixed(c) => *c,
            Pin::Mov { body, off } => {
                let off = match rotated {
                    Some((b, q)) if b == *body => rot_off(*off, q),
                    _ => *off,
                };
                [pos[*body][0] + off[0], pos[*body][1] + off[1]]
            }
        })
        .collect();
    let mut total = 0.0;
    for axis in 0..2 {
        let vals: Vec<f64> = coords.iter().map(|c| c[axis]).collect();
        let vmax = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let vmin = vals.iter().copied().fold(f64::INFINITY, f64::min);
        let s_hi: f64 = vals.iter().map(|v| ((v - vmax) / gamma).exp()).sum();
        let x_hi: f64 = vals
            .iter()
            .map(|v| v * ((v - vmax) / gamma).exp())
            .sum::<f64>()
            / s_hi;
        let s_lo: f64 = vals.iter().map(|v| (-(v - vmin) / gamma).exp()).sum();
        let x_lo: f64 = vals
            .iter()
            .map(|v| v * (-(v - vmin) / gamma).exp())
            .sum::<f64>()
            / s_lo;
        total += x_hi - x_lo;
    }
    net.weight * total
}

/// Accumulate the gradient of the weighted-average wirelength of one
/// net into `g`. WA per axis: `x⁺ - x⁻` with
/// `x⁺ = Σ x·e^{x/γ} / Σ e^{x/γ}` (and `-γ` for the min side) — a
/// smooth, everywhere-differentiable HPWL that sharpens as γ shrinks.
fn accumulate_wa_gradient(net: &Net, pos: &[[f64; 2]], gamma: f64, g: &mut [[f64; 2]]) {
    let coords: Vec<[f64; 2]> = net
        .pins
        .iter()
        .map(|p| match p {
            Pin::Fixed(c) => *c,
            Pin::Mov { body, off } => [pos[*body][0] + off[0], pos[*body][1] + off[1]],
        })
        .collect();
    for axis in 0..2 {
        let vals: Vec<f64> = coords.iter().map(|c| c[axis]).collect();
        let vmax = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let vmin = vals.iter().copied().fold(f64::INFINITY, f64::min);
        // Max side (shift by vmax for stability).
        let e_hi: Vec<f64> = vals.iter().map(|v| ((v - vmax) / gamma).exp()).collect();
        let s_hi: f64 = e_hi.iter().sum();
        let xw_hi: f64 = vals.iter().zip(&e_hi).map(|(v, e)| v * e).sum::<f64>() / s_hi;
        // Min side.
        let e_lo: Vec<f64> = vals.iter().map(|v| (-(v - vmin) / gamma).exp()).collect();
        let s_lo: f64 = e_lo.iter().sum();
        let xw_lo: f64 = vals.iter().zip(&e_lo).map(|(v, e)| v * e).sum::<f64>() / s_lo;

        for (j, pin) in net.pins.iter().enumerate() {
            let Pin::Mov { body, .. } = pin else { continue };
            let d_hi = e_hi[j] / s_hi * (1.0 + (vals[j] - xw_hi) / gamma);
            let d_lo = e_lo[j] / s_lo * (1.0 - (vals[j] - xw_lo) / gamma);
            g[*body][axis] += net.weight * (d_hi - d_lo);
        }
    }
}

/// Bin grid + spectral Poisson solver.
///
/// `solve` computes the electric field of the charge density `rho` by
/// expanding in a cosine series (mirror/Neumann boundary — charge is
/// repelled by the board edge, matching the outline clamp):
///   ψ(x,y)  = Σ a_uv cos(w_u x)cos(w_v y),  a_uv = ρ̂_uv/(w_u²+w_v²)
///   Ex(x,y) = Σ a_uv·w_u sin(w_u x)cos(w_v y)   (Ey symmetric)
/// with w_u = πu/W. Transforms are dense matrix products against
/// precomputed cos/sin tables — O(m³), microseconds at m ≤ 128, no FFT
/// dependency needed at PCB scale.
struct FieldGrid {
    mx: usize,
    my: usize,
    ox: f64,
    oy: f64,
    hx: f64,
    hy: f64,
    /// cos_x[u*mx + i] = cos(π·u·(2i+1)/(2mx)); sin likewise.
    cos_x: Vec<f64>,
    sin_x: Vec<f64>,
    cos_y: Vec<f64>,
    sin_y: Vec<f64>,
    /// Physical wavenumbers πu/W, πv/H.
    wu: Vec<f64>,
    wv: Vec<f64>,
    ex: Vec<f64>,
    ey: Vec<f64>,
}

impl FieldGrid {
    fn new(mx: usize, my: usize, ox: f64, oy: f64, w: f64, h: f64) -> Self {
        let table = |m: usize, f: fn(f64) -> f64| -> Vec<f64> {
            let mut t = vec![0.0; m * m];
            for u in 0..m {
                for i in 0..m {
                    t[u * m + i] =
                        f(std::f64::consts::PI * u as f64 * (2 * i + 1) as f64 / (2 * m) as f64);
                }
            }
            t
        };
        Self {
            mx,
            my,
            ox,
            oy,
            hx: w / mx as f64,
            hy: h / my as f64,
            cos_x: table(mx, f64::cos),
            sin_x: table(mx, f64::sin),
            cos_y: table(my, f64::cos),
            sin_y: table(my, f64::sin),
            wu: (0..mx)
                .map(|u| std::f64::consts::PI * u as f64 / w)
                .collect(),
            wv: (0..my)
                .map(|v| std::f64::consts::PI * v as f64 / h)
                .collect(),
            ex: vec![0.0; mx * my],
            ey: vec![0.0; mx * my],
        }
    }

    /// Add a rectangle's charge to `rho` as per-bin overlap area. A
    /// rectangle smaller than a bin is inflated to bin size with its
    /// charge preserved so tiny 0402/0603 parts still repel instead of
    /// vanishing between samples.
    fn splat(&self, rho: &mut [f64], mut x0: f64, mut y0: f64, mut x1: f64, mut y1: f64) {
        let area = ((x1 - x0) * (y1 - y0)).max(1e-6);
        if x1 - x0 < self.hx {
            let c = f64::midpoint(x0, x1);
            x0 = c - self.hx / 2.0;
            x1 = c + self.hx / 2.0;
        }
        if y1 - y0 < self.hy {
            let c = f64::midpoint(y0, y1);
            y0 = c - self.hy / 2.0;
            y1 = c + self.hy / 2.0;
        }
        // Preserve total charge after inflation.
        let scale = area / ((x1 - x0) * (y1 - y0));
        let (i0, i1) = self.bin_range_x(x0, x1);
        let (j0, j1) = self.bin_range_y(y0, y1);
        for i in i0..=i1 {
            let bx0 = self.ox + i as f64 * self.hx;
            let ov_x = (x1.min(bx0 + self.hx) - x0.max(bx0)).max(0.0);
            for j in j0..=j1 {
                let by0 = self.oy + j as f64 * self.hy;
                let ov_y = (y1.min(by0 + self.hy) - y0.max(by0)).max(0.0);
                // Store as dimensionless utilisation: charge / bin area.
                rho[i * self.my + j] += scale * ov_x * ov_y / (self.hx * self.hy);
            }
        }
    }

    fn bin_range_x(&self, x0: f64, x1: f64) -> (usize, usize) {
        let i0 = (((x0 - self.ox) / self.hx).floor() as i64).clamp(0, self.mx as i64 - 1);
        let i1 = (((x1 - self.ox) / self.hx).ceil() as i64 - 1).clamp(i0, self.mx as i64 - 1);
        (i0 as usize, i1 as usize)
    }

    fn bin_range_y(&self, y0: f64, y1: f64) -> (usize, usize) {
        let j0 = (((y0 - self.oy) / self.hy).floor() as i64).clamp(0, self.my as i64 - 1);
        let j1 = (((y1 - self.oy) / self.hy).ceil() as i64 - 1).clamp(j0, self.my as i64 - 1);
        (j0 as usize, j1 as usize)
    }

    /// Solve the Poisson system for `rho` and fill `self.ex`/`self.ey`.
    fn solve(&mut self, rho: &[f64]) {
        let (mx, my) = (self.mx, self.my);
        // Forward DCT-II both axes:  ρ̂_uv = (α_u α_v / (mx·my)) Σ ρ_ij cos·cos
        // Pass 1: t[u][j] = Σ_i cos_x[u,i]·ρ[i][j]
        let mut t = vec![0.0f64; mx * my];
        for u in 0..mx {
            for i in 0..mx {
                let c = self.cos_x[u * mx + i];
                if c == 0.0 {
                    continue;
                }
                let row = &rho[i * my..(i + 1) * my];
                let out = &mut t[u * my..(u + 1) * my];
                for (o, r) in out.iter_mut().zip(row) {
                    *o += c * r;
                }
            }
        }
        // Pass 2 + Poisson division: a[u][v] = ρ̂_uv / (w_u²+w_v²)
        let norm = 1.0 / (mx * my) as f64;
        let mut a = vec![0.0f64; mx * my];
        for u in 0..mx {
            let alpha_u = if u == 0 { 1.0 } else { 2.0 };
            for v in 0..my {
                if u == 0 && v == 0 {
                    continue; // DC term: neutralised background charge
                }
                let alpha_v = if v == 0 { 1.0 } else { 2.0 };
                let mut s = 0.0;
                for j in 0..my {
                    s += t[u * my + j] * self.cos_y[v * my + j];
                }
                let w2 = self.wu[u] * self.wu[u] + self.wv[v] * self.wv[v];
                a[u * my + v] = alpha_u * alpha_v * norm * s / w2;
            }
        }
        // Ex = Σ a_uv·w_u·sin_x·cos_y : pass back to space, x first.
        let mut tx = vec![0.0f64; mx * my];
        let mut ty = vec![0.0f64; mx * my];
        for i in 0..mx {
            for u in 0..mx {
                let su = self.sin_x[u * mx + i] * self.wu[u];
                let cu = self.cos_x[u * mx + i];
                if su == 0.0 && cu == 0.0 {
                    continue;
                }
                let arow = &a[u * my..(u + 1) * my];
                let xrow = &mut tx[i * my..(i + 1) * my];
                let yrow = &mut ty[i * my..(i + 1) * my];
                for v in 0..my {
                    xrow[v] += su * arow[v];
                    yrow[v] += cu * arow[v] * self.wv[v];
                }
            }
        }
        for i in 0..mx {
            for j in 0..my {
                let mut ex = 0.0;
                let mut ey = 0.0;
                for v in 0..my {
                    ex += tx[i * my + v] * self.cos_y[v * my + j];
                    ey += ty[i * my + v] * self.sin_y[v * my + j];
                }
                self.ex[i * self.my + j] = ex;
                self.ey[i * self.my + j] = ey;
            }
        }
    }

    /// Charge-weighted mean field over a rectangle (the force on that
    /// rectangle's charge, per unit charge).
    fn field_over(&self, x0: f64, y0: f64, x1: f64, y1: f64) -> (f64, f64) {
        let (i0, i1) = self.bin_range_x(x0, x1);
        let (j0, j1) = self.bin_range_y(y0, y1);
        let mut fx = 0.0;
        let mut fy = 0.0;
        let mut w_sum = 0.0;
        for i in i0..=i1 {
            let bx0 = self.ox + i as f64 * self.hx;
            let ov_x = (x1.min(bx0 + self.hx) - x0.max(bx0)).max(0.0);
            for j in j0..=j1 {
                let by0 = self.oy + j as f64 * self.hy;
                let ov_y = (y1.min(by0 + self.hy) - y0.max(by0)).max(0.0);
                let w = ov_x * ov_y;
                fx += w * self.ex[i * self.my + j];
                fy += w * self.ey[i * self.my + j];
                w_sum += w;
            }
        }
        if w_sum > 1e-12 {
            (fx / w_sum, fy / w_sum)
        } else {
            (0.0, 0.0)
        }
    }

    /// Fraction of movable charge above the target utilisation.
    fn overflow(&self, rho: &[f64], target_density: f64, total_charge: f64) -> f64 {
        if total_charge <= 1e-9 {
            return 0.0;
        }
        let bin_area = self.hx * self.hy;
        let over: f64 = rho
            .iter()
            .map(|&r| (r - target_density).max(0.0) * bin_area)
            .sum();
        over / total_charge
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A charge block centred exactly on the domain must produce a field
    /// that points away from it and is mirror-antisymmetric. (The block
    /// spans bins 15..16 on both axes so its centre coincides with the
    /// domain centre — otherwise the Neumann mirror images at the two
    /// walls sit at different distances and break exact symmetry.)
    #[test]
    fn poisson_field_points_away_from_charge() {
        let m = 32;
        let mut grid = FieldGrid::new(m, m, 0.0, 0.0, 32.0, 32.0);
        let mut rho = vec![0.0; m * m];
        for i in 15..=16 {
            for j in 15..=16 {
                rho[i * m + j] = 1.0;
            }
        }
        grid.solve(&rho);
        // Left of the charge: field must push -x. Right: +x.
        let ex_left = grid.ex[11 * m + 15];
        let ex_right = grid.ex[20 * m + 15];
        assert!(
            ex_left < 0.0,
            "field left of charge should point -x, got {ex_left}"
        );
        assert!(
            ex_right > 0.0,
            "field right of charge should point +x, got {ex_right}"
        );
        // (11,15) mirrors to (20,15) in x → ex exactly antisymmetric.
        assert!(
            (ex_left + ex_right).abs() < 1e-8,
            "field should be antisymmetric: {ex_left} vs {ex_right}"
        );
        // (11,15) mirrors to (11,16) in y → ey exactly antisymmetric.
        let ey_a = grid.ey[11 * m + 15];
        let ey_b = grid.ey[11 * m + 16];
        assert!(
            (ey_a + ey_b).abs() < 1e-8,
            "ey should be antisymmetric across the charge: {ey_a} vs {ey_b}"
        );
    }

    /// Uniform charge everywhere → no net field (everything cancels).
    #[test]
    fn poisson_uniform_charge_is_field_free() {
        let m = 16;
        let mut grid = FieldGrid::new(m, m, 0.0, 0.0, 16.0, 16.0);
        let rho = vec![0.7; m * m];
        grid.solve(&rho);
        for v in grid.ex.iter().chain(grid.ey.iter()) {
            assert!(
                v.abs() < 1e-9,
                "uniform density must produce zero field, got {v}"
            );
        }
    }

    /// WA gradient must match a finite-difference of the WA objective.
    #[test]
    fn wa_gradient_matches_finite_difference() {
        let net = Net {
            pins: vec![
                Pin::Fixed([3.0, 7.0]),
                Pin::Mov {
                    body: 0,
                    off: [1.0, -0.5],
                },
                Pin::Mov {
                    body: 1,
                    off: [-2.0, 0.0],
                },
            ],
            weight: 2.0,
        };
        let gamma = 1.5;
        let wa = |pos: &[[f64; 2]]| -> f64 {
            let coords: Vec<[f64; 2]> = net
                .pins
                .iter()
                .map(|p| match p {
                    Pin::Fixed(c) => *c,
                    Pin::Mov { body, off } => [pos[*body][0] + off[0], pos[*body][1] + off[1]],
                })
                .collect();
            let mut total = 0.0;
            for axis in 0..2 {
                let vals: Vec<f64> = coords.iter().map(|c| c[axis]).collect();
                let vmax = vals.iter().copied().fold(f64::NEG_INFINITY, f64::max);
                let vmin = vals.iter().copied().fold(f64::INFINITY, f64::min);
                let s_hi: f64 = vals.iter().map(|v| ((v - vmax) / gamma).exp()).sum();
                let x_hi: f64 = vals
                    .iter()
                    .map(|v| v * ((v - vmax) / gamma).exp())
                    .sum::<f64>()
                    / s_hi;
                let s_lo: f64 = vals.iter().map(|v| (-(v - vmin) / gamma).exp()).sum();
                let x_lo: f64 = vals
                    .iter()
                    .map(|v| v * (-(v - vmin) / gamma).exp())
                    .sum::<f64>()
                    / s_lo;
                total += x_hi - x_lo;
            }
            net.weight * total
        };

        let pos = vec![[4.0, 5.0], [8.0, 6.5]];
        let mut g = vec![[0.0; 2]; 2];
        accumulate_wa_gradient(&net, &pos, gamma, &mut g);

        let eps = 1e-6;
        for b in 0..2 {
            for axis in 0..2 {
                let mut p_hi = pos.clone();
                p_hi[b][axis] += eps;
                let mut p_lo = pos.clone();
                p_lo[b][axis] -= eps;
                let fd = (wa(&p_hi) - wa(&p_lo)) / (2.0 * eps);
                let got = g[b][axis];
                assert!(
                    (fd - got).abs() < 1e-5,
                    "gradient mismatch body {b} axis {axis}: fd {fd} vs analytic {got}"
                );
            }
        }
    }

    /// Two identical rectangles stacked on the same spot must be pushed
    /// in opposite directions by the density field.
    #[test]
    fn overlapping_rectangles_repel() {
        let m = 32;
        let mut grid = FieldGrid::new(m, m, 0.0, 0.0, 64.0, 64.0);
        let mut rho = vec![0.0; m * m];
        // One 8×8 mm block left of centre, one right, overlapping across
        // the middle: the left block's centre must feel a -x field.
        grid.splat(&mut rho, 24.0, 28.0, 36.0, 36.0);
        grid.splat(&mut rho, 28.0, 28.0, 40.0, 36.0);
        grid.solve(&rho);
        let (ex_l, _) = grid.field_over(24.0, 28.0, 36.0, 36.0);
        let (ex_r, _) = grid.field_over(28.0, 28.0, 40.0, 36.0);
        assert!(ex_l < 0.0, "left block should be pushed -x, got {ex_l}");
        assert!(ex_r > 0.0, "right block should be pushed +x, got {ex_r}");
    }
}
