//! Pure quota forecasts and conservative window scoring.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Exact identity of a quota window.
pub(crate) struct Key {
    pub runtime: String,
    pub lane: String,
    pub window: String,
    pub target: Option<String>,
    pub source: String,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// One quota observation; times are epoch seconds and remaining is a percentage.
pub(crate) struct Sample {
    pub key: Key,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<f64>,
    pub observed_at: Option<f64>,
    pub valid_until: Option<f64>,
}
#[derive(Debug, Clone, Serialize, Deserialize)]
/// Forecast for one quota window; rates are percentage points per hour.
pub(crate) struct Forecast {
    pub key: Key,
    pub known: bool,
    pub remaining_percent: Option<f64>,
    pub reset_at: Option<f64>,
    pub observed_at: Option<f64>,
    pub warmup: bool,
    pub burn_percent_per_hour: Option<f64>,
    pub sustainable_percent_per_hour: Option<f64>,
    pub risk: String,
    pub burn_span_seconds: Option<f64>,
}
/// Returns whether observations belong to the same reset cycle.
pub(crate) fn same_cycle(sample: &Sample, latest: &Sample) -> bool {
    match (sample.reset_at, latest.reset_at) {
        (None, None) => true,
        (Some(a), Some(b)) => {
            a == b || ((a - b).abs() <= 1.0 && latest.observed_at.is_some_and(|t| a.min(b) > t))
        }
        _ => false,
    }
}
/// Mirrors Python's `forecast._is_fresh`: a missing `valid_until` or `observed_at`
/// is an absent bound, not a failed one -- history rows built straight from
/// already-validated storage may omit them without becoming unknown evidence.
fn forecast_fresh(s: &Sample, at: f64) -> bool {
    s.valid_until.is_none_or(|v| v >= at)
        && s.observed_at.is_none_or(|v| v <= at)
        && s.reset_at.is_none_or(|v| v > at)
}
/// Builds a forecast from a newest-first history for one exact capacity key.
///
/// The newest sample is the sole freshness gate; older samples from its reset
/// cycle remain usable as burn evidence even when their own validity has
/// expired. Missing observation and validity bounds are treated as absent
/// bounds, matching the Python forecast contract.
pub(crate) fn forecast(key: &Key, samples: &[Sample], at: f64) -> Forecast {
    let unknown = Forecast {
        key: key.clone(),
        known: false,
        remaining_percent: None,
        reset_at: None,
        observed_at: None,
        warmup: true,
        burn_percent_per_hour: None,
        sustainable_percent_per_hour: None,
        risk: "unknown".into(),
        burn_span_seconds: None,
    };
    let Some(latest) = samples
        .first()
        .filter(|s| forecast_fresh(s, at) && s.remaining_percent.is_some())
    else {
        return unknown;
    };
    let remaining = latest.remaining_percent.unwrap_or(0.0);
    let mut span = None;
    let mut burn = None;
    let matching: Vec<_> = samples.iter().filter(|s| same_cycle(s, latest)).collect();
    if matching.len() >= 2
        && let Some(oldest) = matching.last()
        && let (Some(a), Some(b), Some(old)) = (
            latest.observed_at,
            oldest.observed_at,
            oldest.remaining_percent,
        )
    {
        span = Some(a - b);
        if a > b {
            burn = Some((old - remaining).max(0.0) / ((a - b) / 3600.0));
        }
    }
    let sustainable = latest
        .reset_at
        .filter(|r| *r > at)
        .map(|r| remaining / ((r - at) / 3600.0));
    let risk = if remaining <= 10.0 {
        "high"
    } else if remaining <= 30.0 {
        "medium"
    } else if span.is_some_and(|s| s >= 3600.0) {
        match (burn, sustainable) {
            (Some(b), Some(s)) if b > s * 1.5 => "high",
            (Some(b), Some(s)) if b > s => "medium",
            _ => "low",
        }
    } else {
        "low"
    };
    Forecast {
        key: key.clone(),
        known: true,
        remaining_percent: Some(remaining),
        reset_at: latest.reset_at,
        observed_at: latest.observed_at,
        warmup: burn.is_none(),
        burn_percent_per_hour: burn,
        sustainable_percent_per_hour: sustainable,
        risk: risk.into(),
        burn_span_seconds: span,
    }
}
/// Scoring evidence for one exact governing quota window.
///
/// Mirrors Python's `CapacityWindowExplanation`. `marker` is `"projected"` for
/// reliable burn evidence or one of `"warmup"`, `"thin_evidence"`, and
/// `"no_reset"` for the centered remaining-percent fallback.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct WindowExplanation {
    pub key: Key,
    pub remaining_percent: f64,
    pub burn_percent_per_hour: Option<f64>,
    pub burn_span_seconds: Option<f64>,
    pub reset_at: Option<f64>,
    pub projected_percent: Option<f64>,
    pub slack: f64,
    pub marker: String,
    pub risk: String,
}

/// Convert one forecast to conservative scoring evidence.
///
/// Unknown, malformed, future-observed, or already-reset forecasts return
/// `None` so the caller defers their route. Reliable projection requires a
/// nonnegative burn rate, at least one hour of evidence, and a future reset;
/// all other known evidence uses the centered remaining-percent fallback.
/// Mirrors Python's `_window`.
pub(crate) fn window_explanation(forecast: &Forecast, now: f64) -> Option<WindowExplanation> {
    if !forecast.known {
        return None;
    }
    let remaining = forecast.remaining_percent?;
    if !remaining.is_finite() || !(0.0..=100.0).contains(&remaining) {
        return None;
    }
    let observed = forecast.observed_at?;
    if !observed.is_finite() || observed > now {
        return None;
    }
    let nonneg = |value: Option<f64>| -> Option<Option<f64>> {
        match value {
            None => Some(None),
            Some(v) if v.is_finite() && v >= 0.0 => Some(Some(v)),
            Some(_) => None,
        }
    };
    let burn = nonneg(forecast.burn_percent_per_hour)?;
    let span = nonneg(forecast.burn_span_seconds)?;
    let reset = nonneg(forecast.reset_at)?;
    if reset.is_some_and(|r| r <= now) {
        return None;
    }
    let (projected, slack, marker) = match (burn, span, reset) {
        (Some(b), Some(s), Some(r)) if s >= 3600.0 => {
            let projected = remaining - b * ((r - now) / 3600.0);
            (
                Some(projected),
                (projected / 100.0).clamp(-1.0, 1.0),
                "projected",
            )
        }
        _ => {
            let slack = (2.0 * remaining / 100.0 - 1.0).clamp(-1.0, 1.0);
            let marker = if reset.is_none() {
                "no_reset"
            } else if burn.is_none() || forecast.warmup {
                "warmup"
            } else {
                "thin_evidence"
            };
            (None, slack, marker)
        }
    };
    Some(WindowExplanation {
        key: forecast.key.clone(),
        remaining_percent: remaining,
        burn_percent_per_hour: burn,
        burn_span_seconds: span,
        reset_at: reset,
        projected_percent: projected,
        slack,
        marker: marker.into(),
        risk: forecast.risk.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Builds the exact key used by the source reset-identity fixtures.
    fn key() -> Key {
        Key {
            runtime: "codex".into(),
            lane: "requests".into(),
            window: "5h".into(),
            target: Some("gpt-5.6-sol".into()),
            source: "app_server".into(),
        }
    }

    /// Builds a sample with optional reset, observation, and expiry epochs.
    fn sample(
        remaining: f64,
        reset: Option<f64>,
        observed: Option<f64>,
        valid: Option<f64>,
    ) -> Sample {
        Sample {
            key: key(),
            remaining_percent: Some(remaining),
            reset_at: reset,
            observed_at: observed,
            valid_until: valid,
        }
    }

    /// Empty, stale, future and reset-boundary evidence remains unknown.
    #[test]
    fn unknown_and_freshness_boundaries() {
        let now = 1_000_000.0;
        let empty = forecast(&key(), &[], now);
        assert!(!empty.known);
        assert_eq!(empty.risk, "unknown");
        assert!(empty.warmup);
        for latest in [
            sample(50.0, None, Some(now), Some(now - 1.0)),
            sample(80.0, Some(now + 900.0), Some(now + 60.0), None),
            sample(80.0, Some(now), Some(now - 60.0), None),
        ] {
            assert!(
                !forecast(
                    &key(),
                    &[latest, sample(60.0, None, Some(now - 3600.0), None)],
                    now
                )
                .known
            );
        }
        assert!(forecast(&key(), &[sample(50.0, None, None, Some(now))], now).known);
    }

    /// Reset jitter groups only open cycles; stale old evidence still counts.
    #[test]
    fn reset_cycles_and_old_expiry() {
        let now = 4600.0;
        let mut old = sample(90.0, Some(11799.5), Some(1000.0), Some(1001.0));
        let latest = sample(70.0, Some(11800.0), Some(now), None);
        assert!(same_cycle(&old, &latest));
        let f = forecast(&key(), &[latest.clone(), old.clone()], now);
        assert_eq!(f.burn_percent_per_hour, Some(20.0));
        assert_eq!(f.burn_span_seconds, Some(3600.0));
        assert_eq!(f.sustainable_percent_per_hour, Some(35.0));
        old.reset_at = Some(1000.0);
        assert!(forecast(&key(), &[latest, old], now).warmup);
        assert!(!same_cycle(
            &sample(90.0, Some(1999.5), Some(900.0), None),
            &sample(80.0, Some(2000.5), Some(2000.0), None)
        ));
    }

    /// Risk thresholds and the one-hour burn evidence gate match source fixtures.
    #[test]
    fn risk_boundaries_and_history() {
        let now = 1_000_000.0;
        for (remaining, expected) in [
            (10.0, "high"),
            (10.1, "medium"),
            (30.0, "medium"),
            (30.1, "low"),
        ] {
            assert_eq!(
                forecast(&key(), &[sample(remaining, None, Some(now), None)], now).risk,
                expected
            );
        }
        let reset = Some(now + 7200.0);
        for (old, expected) in [
            (80.0, "low"),
            (90.0, "low"),
            (95.0, "medium"),
            (105.0, "medium"),
            (160.0, "high"),
        ] {
            let f = forecast(
                &key(),
                &[
                    sample(60.0, reset, Some(now), None),
                    sample(old, reset, Some(now - 3600.0), None),
                ],
                now,
            );
            assert_eq!(f.risk, expected);
        }
        let thin = forecast(
            &key(),
            &[
                sample(99.0, Some(now + 48.0 * 3600.0), Some(now), None),
                sample(100.0, Some(now + 48.0 * 3600.0), Some(now - 600.0), None),
            ],
            now,
        );
        assert_eq!(thin.risk, "low");
        assert_eq!(
            window_explanation(&thin, now).map(|w| w.marker),
            Some("thin_evidence".into())
        );
    }

    /// Reversed timestamps preserve negative spans; invalid scoring data is rejected.
    #[test]
    fn negative_span_and_window_validation() {
        let now = 4600.0;
        let f = forecast(
            &key(),
            &[
                sample(70.0, Some(11800.0), Some(1000.0), None),
                sample(90.0, Some(11800.0), Some(now), None),
            ],
            now,
        );
        assert_eq!(f.burn_span_seconds, Some(-3600.0));
        assert!(f.warmup);
        assert!(window_explanation(&f, now).is_none());
        let f = forecast(
            &key(),
            &[
                sample(70.0, Some(11800.0), Some(now), None),
                sample(90.0, Some(11800.0), Some(1000.0), None),
            ],
            now,
        );
        let w = window_explanation(&f, now);
        assert_eq!(w.as_ref().map(|w| w.marker.as_str()), Some("projected"));
        assert_eq!(w.and_then(|w| w.projected_percent), Some(30.0));
    }
}
