//! Mean and 95% confidence interval over seeds.
//!
//! Seeds are the unit of replication: each seed is a different corpus and query set, so the
//! interval says how much the number moves with the data, not with the clock. Student's t is used
//! because ten seeds is a small sample; the normal 1.96 would understate the interval by ~15% at
//! that size.

/// Mean, interval and sample count of one suite's per-seed values.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Summary {
    pub mean: f64,
    /// `[lo, hi]` of the 95% interval of the mean. Equal to `[mean, mean]` for a single value,
    /// where no spread can be measured; the `n` next to it says so.
    pub ci95: [f64; 2],
    pub n: usize,
}

/// Two-sided 97.5th percentile of Student's t for `df` degrees of freedom (1..=30); 1.96 beyond.
fn t_critical(df: usize) -> f64 {
    const TABLE: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160,
        2.145, 2.131, 2.120, 2.110, 2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056,
        2.052, 2.048, 2.045, 2.042,
    ];
    match df {
        0 => f64::NAN,
        1..=30 => TABLE[df - 1],
        _ => 1.96,
    }
}

/// Arithmetic mean; `None` of nothing.
pub fn mean(values: &[f64]) -> Option<f64> {
    if values.is_empty() {
        return None;
    }
    Some(values.iter().sum::<f64>() / values.len() as f64)
}

/// Mean and t-interval. `None` of nothing, never a fabricated zero.
pub fn summarize(values: &[f64]) -> Option<Summary> {
    let mean = mean(values)?;
    let n = values.len();
    if n < 2 {
        return Some(Summary {
            mean,
            ci95: [mean, mean],
            n,
        });
    }
    let variance = values
        .iter()
        .map(|value| (value - mean).powi(2))
        .sum::<f64>()
        / (n - 1) as f64;
    let half_width = t_critical(n - 1) * (variance / n as f64).sqrt();
    Some(Summary {
        mean,
        ci95: [mean - half_width, mean + half_width],
        n,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nothing_summarises_to_none() {
        assert!(summarize(&[]).is_none());
        assert!(mean(&[]).is_none());
    }

    #[test]
    fn one_value_has_no_interval() {
        let summary = summarize(&[0.8]).expect("summary");
        assert_eq!(summary.mean, 0.8);
        assert_eq!(summary.ci95, [0.8, 0.8]);
        assert_eq!(summary.n, 1);
    }

    #[test]
    fn identical_values_have_a_zero_width_interval() {
        let summary = summarize(&[1.0; 10]).expect("summary");
        assert_eq!(summary.mean, 1.0);
        assert_eq!(summary.ci95, [1.0, 1.0]);
    }

    #[test]
    fn ten_seeds_use_the_t_table_not_the_normal_constant() {
        // Values 1..=10: mean 5.5, sample sd ≈ 3.0277, se ≈ 0.9574, t(9) = 2.262.
        let values: Vec<f64> = (1..=10).map(f64::from).collect();
        let summary = summarize(&values).expect("summary");
        assert!((summary.mean - 5.5).abs() < 1e-12);
        let half = 2.262 * (9.166_666_666_666_666_f64 / 10.0).sqrt();
        assert!(
            (summary.ci95[0] - (5.5 - half)).abs() < 1e-9,
            "{:?}",
            summary.ci95
        );
        assert!(
            (summary.ci95[1] - (5.5 + half)).abs() < 1e-9,
            "{:?}",
            summary.ci95
        );
        assert_eq!(summary.n, 10);
    }

    #[test]
    fn large_samples_fall_back_to_the_normal_constant() {
        assert_eq!(t_critical(31), 1.96);
        assert_eq!(t_critical(1), 12.706);
        assert!(t_critical(0).is_nan());
    }
}
