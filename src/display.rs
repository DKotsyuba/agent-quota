//! Shared human-readable quota facts for MCP and host hooks.

use crate::{capacity::RankingCredits, quota::CreditBalance};

/// Formats a Unix timestamp in UTC, with a relative reset time when requested.
pub(crate) fn utc(at: Option<f64>, relative_to: Option<f64>) -> String {
    let Some(at) = at.filter(|at| (0.0..253_402_300_800.0).contains(at)) else {
        return "unknown".into();
    };
    let Some(time) = utc_parts(at) else {
        return "unknown".into();
    };
    let stamp = format!(
        "{:02}.{:02} {:02}:{:02} UTC",
        time.tm_mday,
        time.tm_mon + 1,
        time.tm_hour,
        time.tm_min
    );
    match relative_to {
        Some(now) if at > now => format!("{stamp} (in {})", duration(at - now)),
        Some(now) => format!("{stamp} ({} ago)", duration(now - at)),
        None => stamp,
    }
}

/// Renders a short reset time for the hook header's UTC timezone.
pub(crate) fn short_reset(at: f64, now: f64) -> String {
    match (utc_parts(at), utc_parts(now)) {
        (Some(at), Some(now)) if at.tm_year == now.tm_year && at.tm_yday == now.tm_yday => {
            format!("{:02}:{:02}", at.tm_hour, at.tm_min)
        }
        (Some(at), _) => format!(
            "{:02}.{:02} {:02}:{:02}",
            at.tm_mday,
            at.tm_mon + 1,
            at.tm_hour,
            at.tm_min
        ),
        _ => "unknown".into(),
    }
}

/// Converts a supported Unix second to a Gregorian date without process timezone state.
fn utc_parts(at: f64) -> Option<libc::tm> {
    if !(0.0..253_402_300_800.0).contains(&at) {
        return None;
    }
    let seconds = at as libc::time_t;
    let mut time = std::mem::MaybeUninit::uninit();
    // SAFETY: Both pointers are valid. On success gmtime_r initializes the supplied tm.
    unsafe { (!libc::gmtime_r(&seconds, time.as_mut_ptr()).is_null()).then(|| time.assume_init()) }
}

/// Shows at most two whole units, using minutes for durations under an hour.
fn duration(seconds: f64) -> String {
    let minutes = (seconds / 60.0).floor() as u64;
    match minutes {
        0 => "<1m".into(),
        1..60 => format!("{minutes}m"),
        60..1440 => format!("{}h", minutes / 60),
        _ => format!("{}d {}h", minutes / 1440, (minutes % 1440) / 60),
    }
}

/// Gives the age of stale evidence, keeping an unknown age distinct.
pub(crate) fn stale(observed: Option<f64>, now: f64) -> String {
    observed.filter(|at| *at <= now).map_or_else(
        || "stale".into(),
        |at| format!("stale: {} old", duration(now - at)),
    )
}

/// Names normalized provider windows and leaves other labels to the caller to escape.
pub(crate) fn window_label(window: &str) -> &str {
    match window {
        "five_hour" => "5h",
        "seven_day" => "7d",
        other => other,
    }
}

/// Rounds percentages for display, with a separate burn-rate precision.
pub(crate) fn percent(value: Option<f64>) -> String {
    value.map_or_else(|| "unknown".into(), |value| format!("{value:.0}%"))
}

pub(crate) fn burn(value: f64) -> String {
    format!("{value:.1}%/h")
}

/// Explains a forecast or provider exhaustion in terms of its limiting window.
pub(crate) fn reason(
    status: &str,
    window: Option<&str>,
    remaining: Option<f64>,
    rate: Option<f64>,
) -> Option<String> {
    let window = window.unwrap_or("plan");
    match status {
        "exhausted" => Some(format!("{window} window has no usable quota")),
        "limited" => Some(match (remaining, rate) {
            (Some(left), _) if left <= 10.0 => format!("{window} window {left:.0}% left"),
            (Some(left), Some(rate)) => format!(
                "{window} window {left:.0}% left, burn {} → runs out before reset",
                burn(rate)
            ),
            _ => format!("{window} window at high quota risk"),
        }),
        _ => None,
    }
}

/// Preserves the paid balance's provider-supplied decimal and availability.
pub(crate) fn paid(credit: &CreditBalance) -> String {
    let amount = credit
        .balance
        .as_deref()
        .filter(|amount| CreditBalance::valid_amount(amount));
    let detail = if credit.unlimited {
        "unlimited".to_owned()
    } else {
        amount.map_or_else(
            || {
                if credit.available() {
                    "available".into()
                } else {
                    "unavailable".into()
                }
            },
            |amount| format!("${amount}"),
        )
    };
    if !credit.available() && amount.is_some() {
        format!("{detail} (unavailable)")
    } else {
        detail
    }
}

/// Describes the credits behind one ranked model.
pub(crate) fn credit_badges(credits: &RankingCredits) -> Vec<String> {
    let mut badges = Vec::new();
    match credits.reset_uses {
        0 => {}
        1 => badges.push("+reset credit".into()),
        count => badges.push(format!("+reset credits ×{count}")),
    }
    if let Some(credit) = &credits.paid {
        badges.push(format!("+credits {}", paid(credit)));
    }
    badges
}
