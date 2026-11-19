//! Opt-in Claude OAuth renewal. Only the configured Claude CLI can modify its credentials.

mod ledger;
mod profile;

use crate::{
    collector::{self, HelperError},
    config::SourceConfig,
    credentials::{
        CREDENTIAL_EXPIRED, CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED, CREDENTIAL_REFRESH_FAILED,
        CREDENTIAL_REFRESH_LIMITED, CREDENTIAL_REFRESH_UNSUPPORTED_HOST, CREDENTIAL_UNAVAILABLE,
        ClaudeLogin,
    },
};
use ledger::{Ledger, Store};
use profile::Host;
use std::{
    path::Path,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

/// Deadline for a single print-mode renewal.
const RENEWAL_TIMEOUT: Duration = Duration::from_secs(90);

/// Outcome of one renewal check. Errors are fixed credential codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Renewal {
    /// The source did not opt in.
    NotConfigured,
    /// The token has no recorded expiry or is outside the renewal window.
    NotDue,
    /// The store now holds a token outside the renewal window.
    Renewed,
    /// The unrenewed token is still valid.
    StillValid,
}

/// Renews an opted-in login near expiry and checks the store against the current wall clock.
pub(crate) async fn renew_if_due(
    source: &SourceConfig,
    home: &Path,
    at: f64,
) -> Result<Renewal, &'static str> {
    if !source.harness.refresh_oauth {
        return Ok(Renewal::NotConfigured);
    }
    renew(source, home, at, &Host::current(), RENEWAL_TIMEOUT, || {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|time| time.as_secs_f64())
            .map_err(|_| CREDENTIAL_REFRESH_FAILED)
    })
    .await
}

/// Runs the renewal protocol with an injectable host and clock for synthetic qualification.
async fn renew(
    source: &SourceConfig,
    home: &Path,
    at: f64,
    host: &Host,
    timeout: Duration,
    now: impl Fn() -> Result<f64, &'static str>,
) -> Result<Renewal, &'static str> {
    if !source.harness.refresh_oauth {
        return Ok(Renewal::NotConfigured);
    }
    if host.unsupported {
        return Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST);
    }
    let login = source
        .credential
        .claude_login(
            source.app_home.as_deref(),
            source.harness.home.as_deref(),
            source.native_directory.as_deref(),
        )?
        .ok_or(CREDENTIAL_UNAVAILABLE)?;
    host.check_store_path(&login.directory)?;
    let store = Store::open(&login.directory).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            CREDENTIAL_UNAVAILABLE
        } else {
            CREDENTIAL_REFRESH_UNSUPPORTED_HOST
        }
    })?;
    Ledger::load(&store)?.require_clean()?;
    let before = login.state()?;
    if !before.renewal_due(at) || !before.refreshable {
        return if !before.renewal_due(at) {
            Ok(Renewal::NotDue)
        } else if before.expired(at) {
            Err(CREDENTIAL_EXPIRED)
        } else {
            Ok(Renewal::StillValid)
        };
    }

    let lock = store.try_lock().map_err(|_| CREDENTIAL_REFRESH_LIMITED)?;
    let Some(_lock) = lock else {
        Ledger::load(&store)?.require_clean()?;
        return after_attempt(&login, now()?, CREDENTIAL_REFRESH_FAILED);
    };
    let mut ledger = Ledger::load(&store)?;
    ledger.require_clean()?;
    let before = login.state()?;
    let at = now()?;
    if !before.renewal_due(at) {
        return Ok(Renewal::Renewed);
    }
    if !before.refreshable {
        return after_attempt(&login, at, CREDENTIAL_EXPIRED);
    }
    if !before.personal_subscription {
        return Err(CREDENTIAL_REFRESH_UNSUPPORTED_HOST);
    }

    let cli = host.prepare(source, &login, &store, home)?;
    if !ledger.available(home, at)? {
        return after_attempt(&login, at, CREDENTIAL_REFRESH_LIMITED);
    }
    // Fence qualification before spawn. Only a print turn consumes the rolling allowance.
    ledger.pending = true;
    ledger.save(&store)?;
    let result = async {
        cli.qualify().await?;
        let command = cli.print_command(&store)?;
        let started_at = now()?;
        if !ledger.available(home, started_at)? {
            return after_attempt(&login, started_at, CREDENTIAL_REFRESH_LIMITED);
        }
        ledger.charge(started_at);
        ledger.save(&store)?;
        if collector::run_helper(command, timeout, None).await
            == Err(HelperError::CleanupUnconfirmed)
        {
            return Err(CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED);
        }
        store
            .verify_path(&login.directory)
            .map_err(|_| CREDENTIAL_REFRESH_UNSUPPORTED_HOST)?;
        after_attempt(&login, now()?, CREDENTIAL_REFRESH_FAILED)
    }
    .await;
    if result != Err(CREDENTIAL_REFRESH_CLEANUP_UNCONFIRMED) {
        ledger.complete(&store)?;
    }
    result
}

/// Determines the result using the current expiry and a fresh clock reading supplied by the caller.
fn after_attempt(
    login: &ClaudeLogin,
    at: f64,
    expired: &'static str,
) -> Result<Renewal, &'static str> {
    match login.state() {
        Ok(state) if !state.renewal_due(at) && state.expires_at_ms.is_some() => {
            Ok(Renewal::Renewed)
        }
        Ok(state) if !state.expired(at) && state.expires_at_ms.is_some() => Ok(Renewal::StillValid),
        _ => Err(expired),
    }
}

#[cfg(test)]
mod tests;
