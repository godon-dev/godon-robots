//! Response curve interpolation and convergence detection.
//!
//! Port of `engine/characterization.py` — the mathematical core of the
//! characterization loop: probe → measure → interpolate → check
//! convergence → halt or probe next.
//!
//! - ResponseCurve accumulates (level, response) points per edge
//! - piecewise linear interpolation through measured points
//! - convergence delta: how much the interpolated surface moved when
//!   the latest point was added
//!
//! The convergence delta serves triple duty: sampler objective,
//! halting criterion, drift detector. No model form assumed — the
//! measured data IS the function.

use std::collections::HashSet;

/// A level key for the visited set: python stored `round(x, 6)` floats
/// in a set; f64 is not Hash/Eq, so the key is the round-6 bits.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct LevelKey(pub i64);

impl LevelKey {
    pub fn of(v: f64) -> Self {
        LevelKey((v * 1e6).round() as i64)
    }
}

/// Accumulates measured response points for a single edge: a
/// (sender_param, receiver_objective) pair mapping sender push level →
/// receiver objective shift. Points are stored sorted by level;
/// piecewise linear interpolation between adjacent measured points; no
/// extrapolation.
pub struct ResponseCurve {
    pub convergence_threshold: f64,
    pub min_points: usize,
    grid_size: usize,

    /// measured points (level, response_shift), insertion-ordered
    points: Vec<(f64, f64)>,
    /// previous interpolation snapshot (for convergence comparison)
    prev_interp: Option<Vec<(f64, f64)>>,
    /// convergence delta after each point addition
    delta_history: Vec<f64>,
    /// number of substeps inserted (recursive refinement counter)
    substep_count: i64,
}

impl ResponseCurve {
    pub fn new(convergence_threshold: f64, min_points: usize) -> Self {
        ResponseCurve {
            convergence_threshold,
            min_points,
            grid_size: 200,
            points: Vec::new(),
            prev_interp: None,
            delta_history: Vec::new(),
            substep_count: 0,
        }
    }

    /// Sorted copy of measured points.
    pub fn points(&self) -> Vec<(f64, f64)> {
        let mut pts = self.points.clone();
        pts.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
        pts
    }

    pub fn num_points(&self) -> usize {
        self.points.len()
    }

    /// True when the last added point barely moved the surface.
    pub fn is_converged(&self) -> bool {
        if self.delta_history.is_empty() {
            return false;
        }
        if self.num_points() < self.min_points {
            return false;
        }
        self.delta_history[self.delta_history.len() - 1] < self.convergence_threshold
    }

    /// Convergence delta from the most recent point addition.
    pub fn last_delta(&self) -> f64 {
        *self.delta_history.last().unwrap_or(&f64::INFINITY)
    }

    /// Add a measured point and return the convergence delta. A point
    /// at the same level REPLACES its predecessor — the delta from
    /// replacement is the drift signal. The delta is the L1 difference
    /// between the new and previous interpolated surfaces, evaluated
    /// on a fine grid spanning the measured range.
    pub fn add_point(&mut self, level: f64, response: f64) -> f64 {
        // Check for re-measurement
        let existing = self
            .points
            .iter()
            .position(|(l, _)| (l - level).abs() < 1e-9);
        match existing {
            Some(idx) => {
                self.points[idx] = (level, response);
            }
            None => self.points.push((level, response)),
        }

        let mut sorted_points = self.points();
        let curr_interp = evaluate_grid(&mut sorted_points, self.grid_size);

        let delta = match &self.prev_interp {
            None => f64::INFINITY, // first meaningful point — no prior
            Some(prev) => grid_distance(&curr_interp, prev),
        };

        self.prev_interp = Some(curr_interp);
        self.delta_history.push(delta);
        delta
    }

    /// Suggest the next level to probe based on where the curve moves
    /// most. Coverage-first, slope-guided: unvisited levels adjacent
    /// to the steepest segment; when all discrete levels are visited,
    /// a substep inside the steepest segment; None once converged.
    pub fn suggest_next_level(
        &mut self,
        lower: f64,
        upper: f64,
        step: f64,
        visited: &HashSet<LevelKey>,
    ) -> Option<f64> {
        if self.is_converged() {
            return None;
        }

        // Enumerate all discrete levels
        let mut levels: Vec<f64> = Vec::new();
        let mut v = lower;
        while v <= upper + 1e-9 {
            levels.push(round6(v));
            v += step;
        }

        let unvisited: Vec<f64> = levels
            .into_iter()
            .filter(|l| !visited.contains(&LevelKey::of(*l)))
            .collect();

        if !unvisited.is_empty() {
            if self.num_points() < 2 {
                // Not enough points to estimate slope — midpoint of
                // the unvisited set
                return Some(unvisited[unvisited.len() / 2]);
            }

            // The unvisited level adjacent to the steepest segment is
            // most informative.
            let sorted_pts = self.points();
            let mut best_level = unvisited[0];
            let mut best_priority = -1.0f64;

            for l in &unvisited {
                for i in 0..sorted_pts.len().saturating_sub(1) {
                    let (x0, y0) = sorted_pts[i];
                    let (x1, y1) = sorted_pts[i + 1];
                    if x0 <= *l && *l <= x1 && x1 > x0 {
                        let slope = (y1 - y0).abs() / (x1 - x0);
                        if slope > best_priority {
                            best_priority = slope;
                            best_level = *l;
                        }
                        break;
                    }
                }
            }
            return Some(best_level);
        }

        // All discrete levels visited — suggest a substep in the
        // segment with the steepest slope.
        let sorted_pts = self.points();
        if sorted_pts.len() < 2 {
            return None;
        }

        let mut max_slope = 0.0f64;
        let mut substep_at: Option<f64> = None;
        for i in 0..sorted_pts.len() - 1 {
            let (x0, y0) = sorted_pts[i];
            let (x1, y1) = sorted_pts[i + 1];
            if x1 > x0 {
                let slope = (y1 - y0).abs() / (x1 - x0);
                if slope > max_slope {
                    max_slope = slope;
                    let mid = (x0 + x1) / 2.0;
                    if !visited.contains(&LevelKey::of(mid)) {
                        substep_at = Some(mid);
                    }
                }
            }
        }
        if substep_at.is_some() {
            self.substep_count += 1;
        }
        substep_at
    }

    /// Export measured points for storage/transfer.
    pub fn export_points(&self) -> Vec<(f64, f64)> {
        self.points()
    }

    /// Summary statistics for logging/debugging.
    pub fn summary(&self) -> CurveSummary {
        let pts = self.points();
        if pts.len() < 2 {
            return CurveSummary {
                num_points: pts.len(),
                num_substeps: 0,
                converged: false,
                last_delta: f64::INFINITY,
                mean_slope: 0.0,
                max_slope: 0.0,
                level_range: None,
                response_range: None,
            };
        }
        let mut slopes = Vec::new();
        for i in 0..pts.len() - 1 {
            let (x0, y0) = pts[i];
            let (x1, y1) = pts[i + 1];
            if x1 > x0 {
                slopes.push((y1 - y0).abs() / (x1 - x0));
            }
        }
        let mean_slope = if slopes.is_empty() {
            0.0
        } else {
            slopes.iter().sum::<f64>() / slopes.len() as f64
        };
        CurveSummary {
            num_points: pts.len(),
            num_substeps: self.substep_count,
            converged: self.is_converged(),
            last_delta: self.last_delta(),
            mean_slope,
            max_slope: slopes.iter().cloned().fold(0.0, f64::max),
            level_range: Some((pts[0].0, pts[pts.len() - 1].0)),
            response_range: Some((
                pts.iter().map(|p| p.1).fold(f64::INFINITY, f64::min),
                pts.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
            )),
        }
    }
}

#[derive(Debug, Clone)]
pub struct CurveSummary {
    pub num_points: usize,
    pub num_substeps: i64,
    pub converged: bool,
    pub last_delta: f64,
    pub mean_slope: f64,
    pub max_slope: f64,
    pub level_range: Option<(f64, f64)>,
    pub response_range: Option<(f64, f64)>,
}

fn round6(v: f64) -> f64 {
    (v * 1e6).round() / 1e6
}

/// Evaluate piecewise linear interpolation on a fine grid spanning
/// min→max measured level.
fn evaluate_grid(sorted_points: &mut [(f64, f64)], grid_size: usize) -> Vec<(f64, f64)> {
    sorted_points.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    if sorted_points.len() < 2 {
        return sorted_points.to_vec();
    }
    let lo = sorted_points[0].0;
    let hi = sorted_points[sorted_points.len() - 1].0;
    if hi <= lo {
        return sorted_points.to_vec();
    }
    let step = (hi - lo) / grid_size as f64;
    let mut result = Vec::with_capacity(grid_size + 1);
    for i in 0..=grid_size {
        let x = lo + i as f64 * step;
        let y = interp_at(x, sorted_points);
        result.push((x, y));
    }
    result
}

/// Piecewise linear interpolation at x (python `_interp_at`).
pub fn interp_at(x: f64, sorted_points: &[(f64, f64)]) -> f64 {
    if sorted_points.is_empty() {
        return f64::NAN;
    }
    if x <= sorted_points[0].0 {
        return sorted_points[0].1;
    }
    if x >= sorted_points[sorted_points.len() - 1].0 {
        return sorted_points[sorted_points.len() - 1].1;
    }
    for i in 0..sorted_points.len() - 1 {
        let (x0, y0) = sorted_points[i];
        let (x1, y1) = sorted_points[i + 1];
        if x0 <= x && x <= x1 {
            if x1 == x0 {
                return y0;
            }
            let t = (x - x0) / (x1 - x0);
            return y0 + t * (y1 - y0);
        }
    }
    sorted_points[sorted_points.len() - 1].1
}

/// L1 distance between two interpolation grids: mean absolute
/// deviation over grid_a's evaluation points, interpolating grid_b
/// onto them (python `_grid_distance`).
fn grid_distance(grid_a: &[(f64, f64)], grid_b: &[(f64, f64)]) -> f64 {
    if grid_a.is_empty() || grid_b.is_empty() {
        return f64::INFINITY;
    }
    let mut sorted_b = grid_b.to_vec();
    sorted_b.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    let mut total_dist = 0.0;
    let mut count = 0usize;
    for (x, y_a) in grid_a {
        let y_b = interp_at(*x, &sorted_b);
        total_dist += (y_a - y_b).abs();
        count += 1;
    }
    if count > 0 {
        total_dist / count as f64
    } else {
        f64::INFINITY
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // test_linear_convergence: f(x) = 0.7 * x/100 converges fast
    #[test]
    fn linear_convergence() {
        let mut curve = ResponseCurve::new(0.005, 4);
        let (lower, upper, step) = (0.0, 100.0, 20.0);
        let mut visited: HashSet<LevelKey> = HashSet::new();

        for _ in 0..20 {
            let Some(level) = curve.suggest_next_level(lower, upper, step, &visited) else {
                break;
            };
            visited.insert(LevelKey::of(level));
            let response = 0.7 * (level / 100.0);
            curve.add_point(level, response);
        }

        let summary = curve.summary();
        assert!(summary.converged, "Linear should converge");
        assert!(summary.num_points <= 8, "Linear should converge fast");
    }

    // test_threshold_recovery: f(x) = 0 if x < 50 else 0.7
    #[test]
    fn threshold_recovery() {
        let mut curve = ResponseCurve::new(0.01, 5);
        let (lower, upper, step) = (0.0, 100.0, 20.0);
        let mut visited: HashSet<LevelKey> = HashSet::new();

        for _ in 0..25 {
            let Some(level) = curve.suggest_next_level(lower, upper, step, &visited) else {
                break;
            };
            visited.insert(LevelKey::of(level));
            let response = if level < 50.0 { 0.0 } else { 0.7 };
            curve.add_point(level, response);
        }

        let pts = curve.points();
        let below: Vec<_> = pts.iter().filter(|p| p.0 < 50.0).collect();
        let above: Vec<_> = pts.iter().filter(|p| p.0 >= 50.0).collect();
        assert!(!below.is_empty(), "Should have points below threshold");
        assert!(!above.is_empty(), "Should have points above threshold");
        assert!(
            below.iter().all(|p| p.1.abs() < 0.05),
            "Below threshold should be ~0: {below:?}"
        );
        assert!(
            above.iter().all(|p| (p.1 - 0.7).abs() < 0.05),
            "Above threshold should be ~0.7: {above:?}"
        );
    }

    // test_saturation_recovery: a bend, still converges
    #[test]
    fn saturation_recovery() {
        let mut curve = ResponseCurve::new(0.005, 4);
        let (lower, upper, step) = (0.0, 100.0, 20.0);
        let mut visited: HashSet<LevelKey> = HashSet::new();

        for _ in 0..25 {
            let Some(level) = curve.suggest_next_level(lower, upper, step, &visited) else {
                break;
            };
            visited.insert(LevelKey::of(level));
            let x = level / 100.0;
            let response = 0.7 * x / (1.0 + (x - 0.5).abs() * 4.0);
            curve.add_point(level, response);
        }

        assert!(curve.summary().converged, "Saturation should converge");
    }

    // test_drift_detection: re-measuring a drifted level moves the surface
    #[test]
    fn drift_detection() {
        let mut curve = ResponseCurve::new(0.01, 4);
        for level in [0.0, 50.0, 100.0, 25.0, 75.0] {
            curve.add_point(level, 0.7 * (level / 100.0));
        }

        // the system drifted: re-measure level 50 with a new response
        let drift_amount = 0.15;
        let response_drifted = 0.7 * (50.0 / 100.0) + drift_amount;
        let delta_after = curve.add_point(50.0, response_drifted);

        assert!(
            delta_after > curve.convergence_threshold,
            "Drift should cause delta above threshold: {delta_after:.4}"
        );
    }

    // test_convergence_delta_stationary: last delta ~0 for stationary
    #[test]
    fn convergence_delta_stationary() {
        let mut curve = ResponseCurve::new(0.001, 6);
        let (lower, upper, step) = (0.0, 100.0, 25.0);
        let mut visited: HashSet<LevelKey> = HashSet::new();

        for _ in 0..10 {
            let Some(level) = curve.suggest_next_level(lower, upper, step, &visited) else {
                break;
            };
            visited.insert(LevelKey::of(level));
            curve.add_point(level, 0.5 * (level / 100.0));
        }

        assert!(curve.is_converged(), "Stationary linear should converge");
        assert!(
            curve.last_delta() < 0.001,
            "Last delta should be ~0 for stationary: {}",
            curve.last_delta()
        );
    }
}
