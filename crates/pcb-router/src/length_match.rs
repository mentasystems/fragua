//! Length matching post-pass.
//!
//! For each net whose class declares a `target_length_mm` (or for both
//! halves of a diff-pair), measure the current total trace length,
//! compute the delta to the shared target, and insert a serpentine
//! zigzag into the longest straight segment to make up the difference.
//!
//! Serpentine amplitude is fixed at `3 × trace_width` (so the trace
//! comfortably clears its own self-clearance), and the frequency is
//! tuned so the added zigzag path length equals the delta.

use std::collections::HashMap;

use pcb_core::{Board, Length, Point, Schematic, Trace};

/// Per-net record of what length-match did. The caller can surface
/// these in a UI or just inspect them in tests.
#[derive(Debug, Clone)]
pub struct LengthAdjustment {
    pub net: String,
    pub original_mm: f64,
    pub target_mm: f64,
    pub final_mm: f64,
    pub status: AdjustmentStatus,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdjustmentStatus {
    /// Already within tolerance — nothing changed.
    Skipped,
    /// Successfully inserted a serpentine to reach the target.
    Adjusted,
    /// No straight segment long enough for a serpentine — left alone.
    Failed,
}

/// Run a length-matching pass on every net of `board`. Returns one
/// `LengthAdjustment` per net we considered (skipped, adjusted, or
/// failed).
pub fn length_match_pass(board: &mut Board, schematic: &Schematic) -> Vec<LengthAdjustment> {
    let mut out = Vec::new();

    // Group traces by net (capture indices so we can rewrite in place).
    let mut by_net: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, t) in board.traces.iter().enumerate() {
        by_net.entry(t.net.clone()).or_default().push(i);
    }

    let mut lengths: HashMap<String, f64> = HashMap::new();
    for (net, idxs) in &by_net {
        let mut sum = 0.0_f64;
        for &i in idxs {
            sum += trace_length_mm(&board.traces[i]);
        }
        lengths.insert(net.clone(), sum);
    }

    // Compute targets: explicit class.target_length_mm, or pair-derived
    // target (max of pair lengths). Diff-pair partners share a target.
    let mut targets: HashMap<String, f64> = HashMap::new();
    let mut tolerances: HashMap<String, f64> = HashMap::new();
    for net in lengths.keys() {
        let class = schematic.class_for(net);
        if let Some(t) = class.target_length_mm {
            targets.insert(net.clone(), t);
            tolerances.insert(net.clone(), class.length_tolerance_mm);
        }
        if let Some(partner) = &class.diff_pair_with {
            if partner != net && lengths.contains_key(partner) {
                let a = lengths.get(net).copied().unwrap_or(0.0);
                let b = lengths.get(partner).copied().unwrap_or(0.0);
                let shared = a.max(b);
                targets.insert(net.clone(), shared);
                tolerances.insert(net.clone(), class.length_tolerance_mm);
            }
        }
    }

    // For each net needing adjustment, find the longest straight
    // segment, replace it with a serpentine that adds (target - len).
    // Iterate over a stable net order (sorted) so test output is
    // deterministic.
    let mut net_names: Vec<String> = targets.keys().cloned().collect();
    net_names.sort();
    for net in net_names {
        let target = targets[&net];
        let tol = tolerances.get(&net).copied().unwrap_or(0.5);
        let current = lengths.get(&net).copied().unwrap_or(0.0);
        let delta = target - current;
        if delta.abs() <= tol {
            out.push(LengthAdjustment {
                net,
                original_mm: current,
                target_mm: target,
                final_mm: current,
                status: AdjustmentStatus::Skipped,
            });
            continue;
        }
        if delta <= 0.0 {
            // Already longer than target — we don't shorten.
            out.push(LengthAdjustment {
                net,
                original_mm: current,
                target_mm: target,
                final_mm: current,
                status: AdjustmentStatus::Skipped,
            });
            continue;
        }
        // Find the longest segment on this net.
        let Some(longest_idx) = by_net.get(&net).and_then(|idxs| {
            idxs.iter().copied().max_by(|a, b| {
                trace_length_mm(&board.traces[*a])
                    .partial_cmp(&trace_length_mm(&board.traces[*b]))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
        }) else {
            out.push(LengthAdjustment {
                net,
                original_mm: current,
                target_mm: target,
                final_mm: current,
                status: AdjustmentStatus::Failed,
            });
            continue;
        };
        let seg = board.traces[longest_idx].clone();
        let seg_len = trace_length_mm(&seg);
        let width_mm = seg.width.to_mm();
        let amp_mm = (width_mm * 3.0).max(0.3);
        // Reserve a margin at each end so the serpentine doesn't run
        // into the segment endpoints.
        let margin = amp_mm.max(1.0);
        let usable = seg_len - 2.0 * margin;
        if usable < amp_mm * 2.0 {
            out.push(LengthAdjustment {
                net,
                original_mm: current,
                target_mm: target,
                final_mm: current,
                status: AdjustmentStatus::Failed,
            });
            continue;
        }
        // The serpentine I emit is a triangle wave: each successive
        // waypoint flips the perpendicular sign by `2 × amp`. So one
        // "cycle" (two half segments) spans `cycle_len` along the
        // segment and swings 2*amp perpendicular twice — its path
        // length is 2 × sqrt((cycle_len/2)² + (2*amp)²). With
        // `cycle_len = amp_mm` each cycle adds ~2.34 × amp_mm of
        // extra path: high enough that short base segments still
        // absorb a meaningful delta.
        let cycle_len = amp_mm;
        let per_cycle_path = 2.0
            * (((cycle_len / 2.0) * (cycle_len / 2.0)) + (2.0 * amp_mm) * (2.0 * amp_mm)).sqrt();
        let per_cycle_extra = per_cycle_path - cycle_len;
        if per_cycle_extra <= 1e-6 {
            out.push(LengthAdjustment {
                net,
                original_mm: current,
                target_mm: target,
                final_mm: current,
                status: AdjustmentStatus::Failed,
            });
            continue;
        }
        // Pick the smallest n_cycles whose maximum extra (at the
        // chosen amp) is >= delta. Then we'll trim amp_eff down so
        // the actual extra matches delta exactly.
        let extra_for = |a: f64, n: f64| -> f64 {
            let start_ramp = ((cycle_len / 2.0).powi(2) + a * a).sqrt();
            let full_swing = ((cycle_len / 2.0).powi(2) + (2.0 * a).powi(2)).sqrt();
            // start-ramp + (2n-1) full-swings + amp-drop - n*cycle_len
            start_ramp + (2.0 * n - 1.0) * full_swing + a - n * cycle_len
        };
        // Find n_cycles such that extra_for(amp_mm, n) >= delta.
        let mut n_cycles: usize = 1;
        loop {
            if extra_for(amp_mm, n_cycles as f64) >= delta {
                break;
            }
            n_cycles += 1;
            if n_cycles > 10_000 {
                break;
            }
        }
        let serp_along = (n_cycles as f64) * cycle_len;
        if serp_along > usable {
            out.push(LengthAdjustment {
                net,
                original_mm: current,
                target_mm: target,
                final_mm: current,
                status: AdjustmentStatus::Failed,
            });
            continue;
        }
        // Bisection on amp_eff so the actual added path equals delta
        // within tolerance.
        let mut lo = 0.0_f64;
        let mut hi = amp_mm;
        let nf = n_cycles as f64;
        // Make sure hi overshoots — extend if needed.
        let mut tries = 0;
        while extra_for(hi, nf) < delta && tries < 20 {
            hi *= 2.0;
            tries += 1;
        }
        let mut amp_eff = amp_mm;
        for _ in 0..40 {
            let mid = 0.5 * (lo + hi);
            let e = extra_for(mid, nf);
            if (e - delta).abs() < 1e-6 {
                amp_eff = mid;
                break;
            }
            if e < delta {
                lo = mid;
            } else {
                hi = mid;
            }
            amp_eff = mid;
        }
        let amp_mm = amp_eff;

        // Build the serpentine segments in segment-local coords.
        // Segment direction unit vector, perpendicular normal.
        let sx = seg.start.x.to_mm();
        let sy = seg.start.y.to_mm();
        let ex = seg.end.x.to_mm();
        let ey = seg.end.y.to_mm();
        let dx = ex - sx;
        let dy = ey - sy;
        let dlen = (dx * dx + dy * dy).sqrt();
        let ux = dx / dlen;
        let uy = dy / dlen;
        let nx = -uy;
        let ny = ux;
        // Start of serpentine zone, along the segment.
        let zone_start_t = (seg_len - serp_along) / 2.0;
        // Construct waypoints: start → zone_start → up → down → up …
        // → zone_end → end. The serpentine alternates side every half
        // cycle.
        let mut waypoints: Vec<(f64, f64)> = Vec::new();
        waypoints.push((sx, sy));
        let zs_x = sx + ux * zone_start_t;
        let zs_y = sy + uy * zone_start_t;
        waypoints.push((zs_x, zs_y));
        // For each cycle: forward half (offset +amp), forward half
        // (offset -amp). Each half spans cycle_len/2 along the segment.
        let mut t_along = zone_start_t;
        let mut half_idx: usize = 0;
        for _ in 0..n_cycles {
            for _ in 0..2 {
                t_along += cycle_len / 2.0;
                let sign = if half_idx.is_multiple_of(2) {
                    1.0
                } else {
                    -1.0
                };
                let wx = sx + ux * t_along + sign * amp_mm * nx;
                let wy = sy + uy * t_along + sign * amp_mm * ny;
                waypoints.push((wx, wy));
                half_idx += 1;
            }
        }
        // After last cycle, return to centerline and continue to end.
        let ze_x = sx + ux * (zone_start_t + serp_along);
        let ze_y = sy + uy * (zone_start_t + serp_along);
        waypoints.push((ze_x, ze_y));
        waypoints.push((ex, ey));

        // Compute new total length contribution from the new segments.
        let mut new_len_mm = 0.0_f64;
        let mut new_traces: Vec<Trace> = Vec::with_capacity(waypoints.len() - 1);
        for w in waypoints.windows(2) {
            let (ax, ay) = w[0];
            let (bx, by) = w[1];
            let dx2 = bx - ax;
            let dy2 = by - ay;
            let l = (dx2 * dx2 + dy2 * dy2).sqrt();
            new_len_mm += l;
            new_traces.push(Trace {
                id: pcb_core::Id::new(),
                layer: seg.layer,
                start: Point::new(Length::from_mm(ax), Length::from_mm(ay)),
                end: Point::new(Length::from_mm(bx), Length::from_mm(by)),
                width: seg.width,
                net: seg.net.clone(),
            });
        }
        // Replace the original segment with the new traces.
        board.traces.remove(longest_idx);
        // Recompute indices in by_net since we just removed an element.
        for idxs in by_net.values_mut() {
            for i in idxs.iter_mut() {
                if *i > longest_idx {
                    *i -= 1;
                }
            }
        }
        // Drop the removed index from the per-net list.
        if let Some(v) = by_net.get_mut(&seg.net) {
            v.retain(|&i| i != longest_idx);
        }
        // Append new traces at the end of board.traces, and record
        // their new indices.
        let insert_at = board.traces.len();
        for (k, t) in new_traces.into_iter().enumerate() {
            board.traces.push(t);
            if let Some(v) = by_net.get_mut(&seg.net) {
                v.push(insert_at + k);
            }
        }

        let final_total = current - seg_len + new_len_mm;
        lengths.insert(seg.net.clone(), final_total);
        out.push(LengthAdjustment {
            net,
            original_mm: current,
            target_mm: target,
            final_mm: final_total,
            status: AdjustmentStatus::Adjusted,
        });
    }

    out
}

fn trace_length_mm(t: &Trace) -> f64 {
    let dx = t.end.x.to_mm() - t.start.x.to_mm();
    let dy = t.end.y.to_mm() - t.start.y.to_mm();
    (dx * dx + dy * dy).sqrt()
}
