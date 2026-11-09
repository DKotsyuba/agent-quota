//! Quota collection and read-only projections over the independent local store.
use crate::{
    advice,
    backoff::AccountBackoff,
    collector,
    config::{ExecutionMode, LimitsSource, QuotaConfig, SourceConfig},
    credentials, delegated,
    forecast::{self, Forecast, Key, Sample, WindowExplanation},
    quota::{
        AccountId, CollectorScope, MAX_OUTPUT_MODELS, MAX_OUTPUT_WINDOWS,
        normalize_collector_output,
    },
    store::{AccountQuotaHistory, ExhaustionLatch, QuotaStore, StoreError, StoredWindow},
};
use serde::Serialize;
use serde_json::{Value, json};
use std::{
    collections::{BTreeMap, BTreeSet},
    fs::{DirBuilder, File, OpenOptions},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
    str::FromStr,
    sync::{Arc, Mutex},
    time::{Duration, SystemTime, UNIX_EPOCH},
};
use tokio::sync::watch;

/// Limits expansion of account/model views; oversized scopes fail explicitly.
const MAX_VIEW_MODELS: usize = 4096;
/// Bounds the total physical windows and latches returned by one request.
const MAX_VIEW_WINDOWS: usize = 4096;
/// Bounds per-request window and model comparisons before constructing a response.
const MAX_VIEW_COMPARISONS: usize = 250_000;

/// One validated, last-accepted configuration and its deduplicated collector plan.
#[derive(Clone)]
struct Settings {
    config: QuotaConfig,
    sources: Vec<SourceConfig>,
    revision: String,
}

/// Safe, nonsecret status of one configured collector round.
#[derive(Clone, Serialize)]
struct SourceReport {
    account: String,
    provider: String,
    source: String,
    status: &'static str,
    windows: usize,
    issues: Vec<String>,
}

/// The latest collection attempt; absence means no round has completed yet.
#[derive(Clone, Serialize)]
struct RoundReport {
    observed_at: f64,
    ok: bool,
    error: Option<String>,
    config_revision: Option<String>,
    sources: Vec<SourceReport>,
}

/// Last accepted owner-run external observation for one account and source.
#[derive(Clone, Serialize)]
struct ExternalReport {
    account: String,
    provider: String,
    source: String,
    config_revision: String,
    accepted_at: f64,
    revision: i64,
}

/// Owned service state. Mutexes serialize the single SQLite connection and reports.
pub(crate) struct Engine {
    home: PathBuf,
    _home_lock: File,
    config_path: PathBuf,
    settings: Mutex<Settings>,
    store: Mutex<QuotaStore>,
    backoff: Mutex<AccountBackoff>,
    last_round: Mutex<Option<RoundReport>>,
    external_reports: Mutex<BTreeMap<(String, String), ExternalReport>>,
    started_at: f64,
    source_epoch: String,
}

/// Returns Unix seconds; a pre-epoch clock is represented by zero, as in agent-run.
fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Creates or validates an owner-private service home without changing existing permissions.
pub(crate) fn prepare_home(home: &Path) -> Result<PathBuf, String> {
    if !home.is_absolute() {
        return Err("home_must_be_absolute".into());
    }
    match DirBuilder::new().mode(0o700).create(home) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err("home_unavailable".into()),
    }
    let metadata = std::fs::symlink_metadata(home).map_err(|_| "home_unavailable")?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if !metadata.file_type().is_dir() || metadata.uid() != uid || metadata.mode() & 0o077 != 0 {
        return Err("home_not_private".into());
    }
    std::fs::canonicalize(home).map_err(|_| "home_unavailable".into())
}

/// Holds an owner-private lock so two services cannot share one database and schedule.
fn lock_home(home: &Path) -> Result<File, String> {
    let path = home.join("quota.lock");
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)
        .map_err(|_| "unsafe_home_lock")?;
    let opened = file.metadata().map_err(|_| "unsafe_home_lock")?;
    let named = std::fs::symlink_metadata(&path).map_err(|_| "unsafe_home_lock")?;
    // SAFETY: geteuid has no pointer arguments or side effects.
    let uid = unsafe { libc::geteuid() };
    if !opened.is_file()
        || opened.uid() != uid
        || opened.mode() & 0o077 != 0
        || opened.nlink() != 1
        || opened.dev() != named.dev()
        || opened.ino() != named.ino()
    {
        return Err("unsafe_home_lock".into());
    }
    fs2::FileExt::try_lock_exclusive(&file).map_err(|_| "home_already_owned".to_owned())?;
    Ok(file)
}

impl Engine {
    /// Opens one private quota home and validates configuration before starting work.
    pub(crate) fn open(home: &Path, config_path: &Path) -> Result<Arc<Self>, String> {
        if !config_path.is_absolute() {
            return Err("config_path_must_be_absolute".into());
        }
        let home = prepare_home(home)?;
        let home_lock = lock_home(&home)?;
        let (config, revision) = QuotaConfig::load_versioned(config_path)?;
        let sources = config.plan()?;
        let mut store =
            QuotaStore::open(&home.join("quota.sqlite")).map_err(|_| "storage_error".to_owned())?;
        for source in &sources {
            store
                .register_account(&source.account)
                .map_err(|_| "storage_error")?;
        }
        let source_epoch = store.epoch().map_err(|_| "storage_error")?;
        let backoff = AccountBackoff::load(&home).map_err(|_| "backoff_unavailable")?;
        Ok(Arc::new(Self {
            home,
            _home_lock: home_lock,
            config_path: config_path.to_path_buf(),
            settings: Mutex::new(Settings {
                config,
                sources,
                revision,
            }),
            store: Mutex::new(store),
            backoff: Mutex::new(backoff),
            last_round: Mutex::new(None),
            external_reports: Mutex::new(BTreeMap::new()),
            started_at: now(),
            source_epoch,
        }))
    }

    /// Reloads valid configuration; a failed revision never replaces the last accepted one.
    fn reload(&self) -> Result<Settings, String> {
        let (config, revision) = QuotaConfig::load_versioned(&self.config_path)?;
        let sources = config.plan()?;
        {
            let mut store = self.store.lock().map_err(|_| "internal_error")?;
            for source in &sources {
                store
                    .register_account(&source.account)
                    .map_err(|_| "storage_error")?;
            }
        }
        let settings = Settings {
            config,
            sources,
            revision,
        };
        *self.settings.lock().map_err(|_| "internal_error")? = settings.clone();
        Ok(settings)
    }

    /// Returns the latest nonsecret service and collection status.
    pub(crate) fn status(&self) -> Result<Value, &'static str> {
        let source_count = self
            .settings
            .lock()
            .map_err(|_| "internal_error")?
            .sources
            .len();
        let last_round = self
            .last_round
            .lock()
            .map_err(|_| "internal_error")?
            .clone();
        let external: Vec<ExternalReport> = self
            .external_reports
            .lock()
            .map_err(|_| "internal_error")?
            .values()
            .cloned()
            .collect();
        Ok(
            json!({"product":"agent-quota", "status":"running", "started_at":self.started_at,
            "source_count":source_count, "last_collection":last_round, "external_collection":external}),
        )
    }

    /// Runs one sequential collection round and retains only bounded safe diagnostics.
    pub(crate) async fn collect_round(&self) -> Result<(), String> {
        let started = now();
        let settings = match self.reload() {
            Ok(settings) => settings,
            Err(error) => {
                let mut last = self.last_round.lock().map_err(|_| "internal_error")?;
                *last = Some(RoundReport {
                    observed_at: started,
                    ok: false,
                    error: Some(error.clone()),
                    config_revision: None,
                    sources: Vec::new(),
                });
                return Err(error);
            }
        };
        let mut results = Vec::with_capacity(settings.sources.len());
        let mut all_ok = true;
        for source in &settings.sources {
            let at = now();
            let source_id = source.source().ok_or("collector_config_invalid")?;
            let binding = source
                .collector
                .as_ref()
                .ok_or("collector_config_invalid")?;
            if source.execution == ExecutionMode::External {
                results.push(SourceReport {
                    account: source.account.as_str().to_owned(),
                    provider: source.provider.clone(),
                    source: source_id,
                    status: "external",
                    windows: 0,
                    issues: Vec::new(),
                });
                continue;
            }
            let mut issues = Vec::new();
            let mut windows = 0;
            let suppressed = {
                let backoff = self.backoff.lock().map_err(|_| "internal_error")?;
                backoff.suppressed(&source.account, &source_id, at)
            };
            if source.models.is_empty() {
                issues.push("no_bound_models".to_owned());
            } else if suppressed {
                let guard = self.backoff.lock().map_err(|_| "internal_error")?;
                issues.push("backoff".to_owned());
                if let Some(issue) = guard.last_failure(&source.account, &source_id) {
                    issues.push(format!("last_failure:{issue}"));
                }
            } else {
                let outcome = async {
                    let context = source.context(at).map_err(str::to_owned)?;
                    let raw = collector::run(binding, &context, &self.home).await?;
                    if credentials::output_contains_credential(&raw, &context) {
                        return Err("collector_output_contains_credential".to_owned());
                    }
                    let scope = CollectorScope {
                        runtime: source.provider.clone(),
                        source: source_id.clone(),
                        models: source.model_ids(),
                    };
                    let snapshot = normalize_collector_output(
                        &source.account,
                        &scope,
                        &raw,
                        now(),
                        MAX_OUTPUT_WINDOWS,
                        MAX_OUTPUT_MODELS,
                    )
                    .map_err(|_| "collector_output_invalid".to_owned())?;
                    let count = snapshot
                        .models
                        .iter()
                        .flat_map(|model| &model.pools)
                        .flat_map(|pool| {
                            pool.windows
                                .iter()
                                .map(|window| (pool.key.as_str(), window.name.as_str()))
                        })
                        .collect::<BTreeSet<_>>()
                        .len();
                    self.store
                        .lock()
                        .map_err(|_| "internal_error")?
                        .record_snapshot(
                            &scope.runtime,
                            &snapshot,
                            settings.config.sample_retention,
                            now(),
                        )
                        .map_err(|_| "store_failed".to_owned())?;
                    Ok::<usize, String>(count)
                }
                .await;
                let mut backoff = self.backoff.lock().map_err(|_| "internal_error")?;
                match outcome {
                    Ok(count) => {
                        windows = count;
                        backoff.record_success(&source.account, &source_id);
                    }
                    Err(code) => {
                        backoff.record_failure(&source.account, &source_id, at, code.clone());
                        issues.push(code);
                    }
                }
            }
            let status = if issues.is_empty() {
                if windows == 0 { "no_data" } else { "collected" }
            } else {
                "failed"
            };
            all_ok &= status != "failed";
            results.push(SourceReport {
                account: source.account.as_str().to_owned(),
                provider: source.provider.clone(),
                source: source_id,
                status,
                windows,
                issues,
            });
        }
        let save_failed = self
            .backoff
            .lock()
            .map_err(|_| "internal_error")?
            .save(&self.home, now())
            .is_err();
        let report = RoundReport {
            observed_at: started,
            ok: all_ok && !save_failed,
            error: save_failed.then(|| "backoff_persist_failed".to_owned()),
            config_revision: Some(settings.revision),
            sources: results,
        };
        *self.last_round.lock().map_err(|_| "internal_error")? = Some(report);
        Ok(())
    }

    /// Validates and commits one credential-owner probe without resolving its secrets.
    pub(crate) fn ingest(&self, envelope: Value) -> Result<Value, &'static str> {
        let settings = self.settings.lock().map_err(|_| "internal_error")?;
        let (config, revision) =
            QuotaConfig::load_versioned(&self.config_path).map_err(|_| "config_changed")?;
        if revision != settings.revision {
            return Err("config_changed");
        }
        let verified = delegated::verify(&config, &revision, envelope, now())
            .map_err(|_| "invalid_observation")?;
        let mut store = self.store.lock().map_err(|_| "internal_error")?;
        let mut reports = self.external_reports.lock().map_err(|_| "internal_error")?;
        let (_, rechecked) =
            QuotaConfig::load_versioned(&self.config_path).map_err(|_| "config_changed")?;
        if rechecked != revision {
            return Err("config_changed");
        }
        let at = now();
        let committed = store
            .record_snapshot(
                &verified.runtime,
                &verified.snapshot,
                config.sample_retention,
                at,
            )
            .map_err(|_| "outcome_unknown")?;
        let account = verified.snapshot.account.as_str().to_owned();
        let report = ExternalReport {
            account: account.clone(),
            provider: verified.runtime,
            source: verified.source.clone(),
            config_revision: revision,
            accepted_at: at,
            revision: committed,
        };
        reports.insert((account, verified.source), report);
        Ok(json!({"stored":true,"revision":committed}))
    }

    /// Runs collections without overlap until shutdown, preserving the configured cadence.
    pub(crate) async fn poll(self: Arc<Self>, mut shutdown: watch::Receiver<bool>) {
        loop {
            if *shutdown.borrow() {
                return;
            }
            tokio::select! {
                _ = shutdown.changed() => return,
                result = self.collect_round() => {
                    if let Err(code) = result { eprintln!("quota collection: {code}"); }
                }
            }
            let interval = match self.settings.lock() {
                Ok(settings) => settings.config.interval_seconds,
                Err(_) => return,
            };
            tokio::select! {
                _ = shutdown.changed() => return,
                _ = tokio::time::sleep(Duration::from_secs(interval)) => {}
            }
        }
    }
}

#[cfg(test)]
#[allow(
    clippy::unwrap_used,
    reason = "Explicit synthetic regression assertions"
)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Case-sensitive provider aliases survive projection; missing bounds cannot confirm quota.
    #[test]
    fn projection_preserves_provider_ids_and_requires_observation_bounds() {
        let text = ["Claude", "claude", "1gateway"]
            .iter()
            .map(|provider| {
                format!(
                    "[[sources]]\naccount='acct'\nprovider='{provider}'\nauth_family='openai'\ncredential='native:codex'\nnative_directory='/synthetic'\nlimits_source='none'\nmodels=[{{ id='a' }}]\n[sources.harness]\nid='codex'\nbinary='/bin/true'\n"
                )
            })
            .collect::<String>();
        let config = QuotaConfig::parse(&text).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let mut history = AccountQuotaHistory {
            revision: 1,
            windows: vec![StoredWindow {
                id: 1,
                runtime: "Claude".into(),
                lane: "a".into(),
                window: "hour".into(),
                target: None,
                source: "fixture".into(),
                remaining_percent: Some(80.0),
                reset_at: Some(2000.0),
                observed_at: Some(900.0),
                valid_until: Some(1500.0),
                models: Some(BTreeSet::from(["a".into()])),
            }],
            latches: vec![],
        };
        let view = account_view(&account, history.clone(), &config, 1000.0, None).unwrap();
        assert_eq!(
            view.models
                .iter()
                .map(|model| model.provider.as_str())
                .collect::<BTreeSet<_>>(),
            BTreeSet::from(["Claude", "claude", "1gateway"])
        );
        assert!(view.models.iter().all(|model| model.state == "known"));
        assert_eq!(
            view.models[0].state_validity,
            StateValidity::Until { at: 1500.0 }
        );
        assert!(view.windows[0].forecast.known);
        let stale = account_view(&account, history.clone(), &config, 1501.0, None).unwrap();
        assert!(!stale.windows[0].forecast.known);
        assert_eq!(stale.windows[0].forecast.reset_at, None);
        assert_eq!(stale.windows[0].last_reset_at, Some(2000.0));
        for (observed, valid) in [(None, Some(1500.0)), (Some(900.0), None), (None, None)] {
            history.windows[0].observed_at = observed;
            history.windows[0].valid_until = valid;
            let view = account_view(&account, history.clone(), &config, 1000.0, None).unwrap();
            assert!(view.models.iter().all(|model| model.state == "unknown"));
            assert_eq!(view.models[0].state_validity, StateValidity::Unknown);
            assert!(!view.windows[0].forecast.known);
            assert!(view.windows[0].evaluation.is_none());
            assert_eq!(view.windows[0].last_remaining_percent, Some(80.0));
            assert_eq!(view.windows[0].last_reset_at, Some(2000.0));
        }
    }

    /// Clock boundaries respect all windows and keep durable latches independent of sample expiry.
    #[test]
    fn model_state_validity_covers_windows_resets_and_latches() {
        let config = QuotaConfig::parse(
            "[[sources]]\naccount='acct'\nprovider='codex'\nauth_family='openai'\ncredential='native:codex'\nnative_directory='/synthetic'\nlimits_source='none'\nmodels=[{ id='a' }]\n[sources.harness]\nid='codex'\nbinary='/bin/true'\n",
        ).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let window = |name: &str, remaining, valid, reset| StoredWindow {
            id: 1,
            runtime: "codex".into(),
            lane: "a".into(),
            window: name.into(),
            target: None,
            source: "fixture".into(),
            remaining_percent: Some(remaining),
            reset_at: reset,
            observed_at: Some(100.0),
            valid_until: Some(valid),
            models: Some(BTreeSet::from(["a".into()])),
        };
        let latch = |name: &str, reset| ExhaustionLatch {
            lane: "a".into(),
            source: "fixture".into(),
            window: name.into(),
            observed_at: 100.0,
            reset_at: reset,
            models: Some(BTreeSet::from(["a".into()])),
        };
        let mut history = AccountQuotaHistory {
            revision: 1,
            windows: vec![
                window("limiting", 20.0, 600.0, Some(1000.0)),
                window("other", 80.0, 300.0, Some(700.0)),
            ],
            latches: vec![],
        };
        let view = account_view(&account, history.clone(), &config, 200.0, None).unwrap();
        assert_eq!(view.models[0].state, "known");
        assert_eq!(
            view.models[0].limiting_window.as_ref().unwrap()["window"],
            "limiting"
        );
        assert_eq!(
            view.models[0].state_validity,
            StateValidity::Until { at: 300.0 }
        );
        let at_expiry = account_view(&account, history.clone(), &config, 300.0, None).unwrap();
        assert_eq!(at_expiry.models[0].state, "known");
        assert_eq!(
            at_expiry.models[0].state_validity,
            StateValidity::Until { at: 300.0 }
        );
        let expired = account_view(&account, history.clone(), &config, 301.0, None).unwrap();
        assert_eq!(expired.models[0].state, "unknown");
        assert_eq!(expired.models[0].state_validity, StateValidity::Unknown);

        history.windows[1].valid_until = Some(600.0);
        history.windows[1].reset_at = Some(250.0);
        let before_reset = account_view(&account, history.clone(), &config, 200.0, None).unwrap();
        assert_eq!(
            before_reset.models[0].state_validity,
            StateValidity::Until { at: 250.0 }
        );
        let at_reset = account_view(&account, history.clone(), &config, 250.0, None).unwrap();
        assert_eq!(at_reset.models[0].state, "unknown");

        history.windows[0].remaining_percent = Some(0.0);
        let zero = account_view(&account, history.clone(), &config, 200.0, None).unwrap();
        assert_eq!(zero.models[0].state, "exhausted");
        assert_eq!(
            zero.models[0].state_validity,
            StateValidity::Until { at: 250.0 }
        );
        let zero_after_reset =
            account_view(&account, history.clone(), &config, 250.0, None).unwrap();
        assert_eq!(zero_after_reset.models[0].state, "unknown");

        for sample in &mut history.windows {
            sample.valid_until = Some(150.0);
        }
        history.latches = vec![latch("limiting", Some(500.0)), latch("other", Some(400.0))];
        let held = account_view(&account, history.clone(), &config, 200.0, None).unwrap();
        assert_eq!(held.models[0].state, "exhausted");
        assert_eq!(
            held.models[0].state_validity,
            StateValidity::Until { at: 400.0 }
        );
        let after_one = account_view(&account, history.clone(), &config, 400.0, None).unwrap();
        assert_eq!(after_one.models[0].state, "exhausted");
        assert_eq!(
            after_one.models[0].state_validity,
            StateValidity::Until { at: 500.0 }
        );
        let after_both = account_view(&account, history.clone(), &config, 500.0, None).unwrap();
        assert_eq!(after_both.models[0].state, "unknown");
        history.latches.push(latch("no-reset", None));
        let no_reset = account_view(&account, history, &config, 10000.0, None).unwrap();
        assert_eq!(no_reset.models[0].state, "exhausted");
        assert_eq!(
            no_reset.models[0].state_validity,
            StateValidity::LatchedNoReset
        );
        assert_eq!(
            serde_json::to_value(&no_reset.models[0].state_validity).unwrap(),
            json!({"kind":"latched_no_reset"})
        );
    }

    /// A state deadline does not freeze time-dependent quota scores or risk.
    #[test]
    fn state_deadline_does_not_freeze_forecast() {
        let config = QuotaConfig::parse(
            "[[sources]]\naccount='acct'\nprovider='codex'\nauth_family='openai'\ncredential='native:codex'\nnative_directory='/synthetic'\nlimits_source='none'\nmodels=[{ id='a' }]\n[sources.harness]\nid='codex'\nbinary='/bin/true'\n",
        ).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let window = |id, remaining, observed| StoredWindow {
            id,
            runtime: "codex".into(),
            lane: "a".into(),
            window: "hour".into(),
            target: None,
            source: "fixture".into(),
            remaining_percent: Some(remaining),
            reset_at: Some(13600.0),
            observed_at: Some(observed),
            valid_until: Some(13000.0),
            models: Some(BTreeSet::from(["a".into()])),
        };
        let history = AccountQuotaHistory {
            revision: 1,
            windows: vec![window(2, 40.0, 10000.0), window(1, 100.0, 6400.0)],
            latches: vec![],
        };
        let first = account_view(&account, history.clone(), &config, 10000.0, None).unwrap();
        let later = account_view(&account, history, &config, 11200.0, None).unwrap();
        assert_eq!(
            first.models[0].state_validity,
            StateValidity::Until { at: 13000.0 }
        );
        assert_eq!(
            later.models[0].state_validity,
            StateValidity::Until { at: 13000.0 }
        );
        assert_eq!(first.models[0].state, "known");
        assert_eq!(later.models[0].state, "known");
        assert_ne!(first.models[0].quota_score, later.models[0].quota_score);
        assert_eq!(first.models[0].risk, "medium");
        assert_eq!(later.models[0].risk, "low");
    }

    /// An overflowing burn defers only its model, preserving other accounts and exhaustion latches.
    #[test]
    fn nonfinite_burn_does_not_discard_other_accounts() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let config_path = root.path().join("config.toml");
        let text = ["acct-bad", "acct-good", "acct-exhausted"]
            .iter()
            .map(|account| format!("[[sources]]\naccount='{account}'\nprovider='codex'\nauth_family='openai'\ncredential='native:codex'\nnative_directory='/synthetic'\nmodels=[{{ id='a' }}]\n[sources.harness]\nid='codex'\nbinary='/bin/true'\n[sources.collector]\ncommand='/bin/true'\nsource='fixture'\n"))
            .collect::<String>();
        std::fs::write(&config_path, text).unwrap();
        let engine = Engine::open(&root.path().join("home"), &config_path).unwrap();
        let scope = CollectorScope {
            runtime: "codex".into(),
            source: "fixture".into(),
            models: BTreeSet::from(["a".into()]),
        };
        for (name, remaining, observed) in [
            ("acct-bad", 100.0, 0.0),
            ("acct-bad", 80.0, 1e-305),
            ("acct-good", 50.0, 0.0),
            ("acct-exhausted", 0.0, 0.0),
        ] {
            let account: AccountId = name.parse().unwrap();
            let reset = if name == "acct-bad" { 2e-305 } else { 2000.0 };
            let raw = json!({"version":1,"windows":[{"pool":"a","window":"hour","models":["a"],
                "remaining_percent":remaining,"observed_at":observed,"valid_until":1000.0,"reset_at":reset}]});
            let snapshot =
                normalize_collector_output(&account, &scope, &raw, 1.0, 256, 256).unwrap();
            engine
                .store
                .lock()
                .unwrap()
                .record_snapshot("codex", &snapshot, 1000, 1e-305)
                .unwrap();
        }
        let result = engine.snapshot_at(&[], None, 1e-305).unwrap();
        let accounts = result["accounts"].as_array().unwrap();
        let account = |name: &str| {
            accounts
                .iter()
                .find(|account| account["account"] == name)
                .unwrap()
        };
        let bad = account("acct-bad");
        assert_eq!(bad["models"][0]["state"], "unknown");
        assert_eq!(bad["windows"][0]["forecast"]["known"], true);
        assert_eq!(bad["windows"][0]["forecast"]["remaining_percent"], 80.0);
        assert!(bad["windows"][0]["forecast"]["burn_percent_per_hour"].is_null());
        assert!(bad["windows"][0]["forecast"]["sustainable_percent_per_hour"].is_null());
        assert!(bad["windows"][0]["evaluation"].is_null());
        assert_eq!(account("acct-good")["models"][0]["state"], "known");
        assert_eq!(account("acct-exhausted")["models"][0]["state"], "exhausted");
        assert_eq!(
            account("acct-exhausted")["models"][0]["state_validity"]["at"],
            2000.0
        );
        let mut overflow = engine
            .snapshot_at(&["acct-bad".into()], Some("a"), 1e-305)
            .unwrap();
        overflow["source_epoch"] = json!("00000000000000000000000000000000");
        overflow["config_revision"] =
            json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/overflow-snapshot-v2.json"))
                .unwrap();
        let wire = serde_json::to_vec(&json!({"version":1,"result":overflow})).unwrap();
        let decoded: Value = serde_json::from_slice(&wire).unwrap();
        assert_eq!(decoded, expected);
    }

    /// An unrepresentable sustainable estimate alone does not invalidate the Run ranker's score.
    #[test]
    fn nonfinite_sustainable_rate_keeps_usable_score() {
        let config = QuotaConfig::parse(
            "[[sources]]\naccount='acct'\nprovider='codex'\nauth_family='openai'\ncredential='native:codex'\nnative_directory='/synthetic'\nlimits_source='none'\nmodels=[{ id='a' }]\n[sources.harness]\nid='codex'\nbinary='/bin/true'\n",
        ).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let reset = f64::from_bits(1);
        let history = AccountQuotaHistory {
            revision: 1,
            windows: vec![StoredWindow {
                id: 1,
                runtime: "codex".into(),
                lane: "a".into(),
                window: "hour".into(),
                target: None,
                source: "fixture".into(),
                remaining_percent: Some(80.0),
                reset_at: Some(reset),
                observed_at: Some(0.0),
                valid_until: Some(1.0),
                models: Some(BTreeSet::from(["a".into()])),
            }],
            latches: vec![],
        };
        let view = account_view(&account, history, &config, 0.0, None).unwrap();
        assert_eq!(view.models[0].state, "known");
        assert_eq!(
            view.models[0].state_validity,
            StateValidity::Until { at: reset }
        );
        assert_eq!(view.models[0].quota_score, Some(1.6));
        assert!(
            view.windows[0]
                .forecast
                .sustainable_percent_per_hour
                .is_none()
        );
        assert!(view.windows[0].evaluation.is_some());
    }

    /// Equal signed-zero slacks use the earlier reset, matching Run's ranker.
    #[test]
    fn signed_zero_slack_uses_reset_tie_break() {
        let config = QuotaConfig::parse(
            "[[sources]]\naccount='acct'\nprovider='codex'\nauth_family='openai'\ncredential='native:codex'\nnative_directory='/synthetic'\nlimits_source='none'\nmodels=[{ id='a' }]\n[sources.harness]\nid='codex'\nbinary='/bin/true'\n",
        ).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let window = |id, name: &str, remaining, observed, reset| StoredWindow {
            id,
            runtime: "codex".into(),
            lane: "a".into(),
            window: name.into(),
            target: None,
            source: "fixture".into(),
            remaining_percent: Some(remaining),
            reset_at: Some(reset),
            observed_at: Some(observed),
            valid_until: Some(15000.0),
            models: Some(BTreeSet::from(["a".into()])),
        };
        let history = AccountQuotaHistory {
            revision: 1,
            windows: vec![
                window(3, "early", 50.0, 10000.0, 11800.0),
                window(2, "later", f64::from_bits(1), 10000.0, 13600.0),
                window(1, "later", f64::from_bits(3), 6400.0, 13600.0),
            ],
            latches: vec![],
        };
        let view = account_view(&account, history, &config, 10000.0, None).unwrap();
        let early = view
            .windows
            .iter()
            .find(|window| window.window == "early")
            .unwrap();
        let later = view
            .windows
            .iter()
            .find(|window| window.window == "later")
            .unwrap();
        assert!(early.evaluation.as_ref().unwrap().slack.is_sign_positive());
        assert!(later.evaluation.as_ref().unwrap().slack.is_sign_negative());
        assert_eq!(view.models[0].state, "known");
        assert_eq!(
            view.models[0].limiting_window.as_ref().unwrap()["window"],
            "early"
        );
        assert_eq!(view.models[0].reset_at, Some(11800.0));
        assert_eq!(view.models[0].risk, "low");
        assert_eq!(view.models[0].quota_score, Some(1.0));
    }

    /// Shared physical windows govern only the model lanes named on each window.
    #[test]
    fn projection_preserves_window_membership_and_latched_exhaustion() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let config = QuotaConfig::parse(
            r#"
[[sources]]
account = "acct"
provider = "codex"
auth_family = "openai"
credential = "native:codex"
native_directory = "/synthetic"
limits_source = "none"
models = [{ id = "a" }, { id = "b" }]
[sources.harness]
id = "codex"
binary = "/bin/true"
"#,
        )
        .unwrap();
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite")).unwrap();
        store.register_account(&account).unwrap();
        let scope = CollectorScope {
            runtime: "codex".into(),
            source: "fixture".into(),
            models: BTreeSet::from(["a".into(), "b".into()]),
        };
        let raw = json!({"version":1,"windows":[
            {"pool":"shared","window":"hour","models":["a","b"],"remaining_percent":80,"observed_at":900,"valid_until":1500,"reset_at":2000},
            {"pool":"shared","window":"week","models":["b"],"remaining_percent":0,"observed_at":900,"valid_until":1500,"reset_at":3000}
        ]});
        let snapshot =
            normalize_collector_output(&account, &scope, &raw, 1000.0, 256, 256).unwrap();
        store
            .record_snapshot("codex", &snapshot, 1000, 1000.0)
            .unwrap();
        let view = account_view(
            &account,
            store.read_account(&account).unwrap(),
            &config,
            1000.0,
            None,
        )
        .unwrap();
        assert_eq!(view.windows.len(), 2);
        assert_eq!(view.models[0].model, "a");
        assert_eq!(view.models[0].state, "known");
        assert_eq!(view.models[1].model, "b");
        assert_eq!(view.models[1].state, "exhausted");
        assert_eq!(view.models[1].exhausted_until, Some(3000.0));
        assert_eq!(
            view.models[1].limiting_window.as_ref().unwrap()["window"],
            "week"
        );
        assert_eq!(view.models[0].pools, vec!["acct::shared"]);
        let expired = account_view(
            &account,
            store.read_account(&account).unwrap(),
            &config,
            3001.0,
            None,
        )
        .unwrap();
        assert_eq!(expired.models[1].state, "unknown");
    }

    /// A fresh zero wins over an equally constrained positive window; unknown evidence defers it.
    #[test]
    fn projection_matches_exhausted_and_unknown_ordering() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let config = QuotaConfig::parse(
            r#"
[[sources]]
account = "acct"
provider = "codex"
auth_family = "openai"
credential = "native:codex"
native_directory = "/synthetic"
limits_source = "none"
models = [{ id = "a" }]
[sources.harness]
id = "codex"
binary = "/bin/true"
"#,
        )
        .unwrap();
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite")).unwrap();
        store.register_account(&account).unwrap();
        for (source, remaining, observed, reset) in [
            ("b", Some(100), 6400, 20800),
            ("a", Some(0), 9000, 30000),
            ("b", Some(10), 10000, 20800),
        ] {
            let scope = CollectorScope {
                runtime: "codex".into(),
                source: source.into(),
                models: BTreeSet::from(["a".into()]),
            };
            let raw = json!({"version":1,"windows":[{"pool":"shared","window":"hour","models":["a"],
                "remaining_percent":remaining,"observed_at":observed,"valid_until":40000,"reset_at":reset}]});
            let snapshot =
                normalize_collector_output(&account, &scope, &raw, observed as f64, 256, 256)
                    .unwrap();
            store
                .record_snapshot("codex", &snapshot, 1000, observed as f64)
                .unwrap();
        }
        let view = account_view(
            &account,
            store.read_account(&account).unwrap(),
            &config,
            10000.0,
            None,
        )
        .unwrap();
        assert_eq!(view.models[0].state, "exhausted");
        assert_eq!(view.models[0].remaining_percent, Some(0.0));
        assert_eq!(view.models[0].exhausted_until, Some(30000.0));
        assert_eq!(
            view.models[0].limiting_window.as_ref().unwrap()["source"],
            "a"
        );
        let scope = CollectorScope {
            runtime: "codex".into(),
            source: "c".into(),
            models: BTreeSet::from(["a".into()]),
        };
        let raw = json!({"version":1,"windows":[{"pool":"shared","window":"hour","models":["a"],
            "remaining_percent":null,"observed_at":10000,"valid_until":40000,"reset_at":30000}]});
        let snapshot =
            normalize_collector_output(&account, &scope, &raw, 10000.0, 256, 256).unwrap();
        store
            .record_snapshot("codex", &snapshot, 1000, 10000.0)
            .unwrap();
        let unknown = account_view(
            &account,
            store.read_account(&account).unwrap(),
            &config,
            10000.0,
            None,
        )
        .unwrap();
        assert_eq!(unknown.models[0].state, "unknown");
        assert!(unknown.models[0].limiting_window.is_none());
    }

    /// A model filter narrows the response before enforcing the projection budget.
    #[test]
    fn model_scope_is_applied_before_view_budget() {
        let mut text = String::new();
        for provider in 0..17 {
            let models = (0..256)
                .map(|model| format!("{{ id = \"model-{provider}-{model}\" }}"))
                .collect::<Vec<_>>()
                .join(",");
            text.push_str(&format!("[[sources]]\naccount='acct'\nprovider='p{provider}'\nauth_family='openai'\ncredential='native:codex'\nnative_directory='/synthetic'\nlimits_source='none'\nmodels=[{models}]\n[sources.harness]\nid='codex'\nbinary='/bin/true'\n"));
        }
        let config = QuotaConfig::parse(&text).unwrap();
        let account: AccountId = "acct".parse().unwrap();
        let history = AccountQuotaHistory {
            revision: 0,
            windows: Vec::new(),
            latches: Vec::new(),
        };
        assert!(matches!(
            account_view(&account, history.clone(), &config, 1.0, None),
            Err("response_too_large")
        ));
        let narrow = account_view(&account, history, &config, 1.0, Some("model-1-2")).unwrap();
        assert_eq!(narrow.models.len(), 1);
        assert_eq!(narrow.models[0].model, "model-1-2");
    }

    /// A read waiting for the store must keep its configuration revision pinned.
    #[test]
    fn snapshot_pins_configuration_until_store_read_finishes() {
        use std::sync::TryLockError;
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path().join("home");
        let config = root.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let engine = Engine::open(&home, &config).unwrap();
        let original = engine.settings.lock().unwrap().revision.clone();
        let store_guard = engine.store.lock().unwrap();
        let reader = Arc::clone(&engine);
        let reading = std::thread::spawn(move || reader.snapshot(&[], None));
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let pinned = loop {
            if matches!(engine.settings.try_lock(), Err(TryLockError::WouldBlock)) {
                std::thread::sleep(Duration::from_millis(20));
                if matches!(engine.settings.try_lock(), Err(TryLockError::WouldBlock)) {
                    break true;
                }
            }
            if std::time::Instant::now() >= deadline {
                break false;
            }
            std::thread::yield_now();
        };
        drop(store_guard);
        let result = reading.join().unwrap().unwrap();
        assert!(
            pinned,
            "snapshot released its configuration before reading the store"
        );
        assert_eq!(result["config_revision"], original);
    }

    /// Two services cannot share a home and collect the same configured sources concurrently.
    #[test]
    fn service_home_has_one_live_owner() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path().join("home");
        let config = root.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let first = Engine::open(&home, &config).unwrap();
        assert!(matches!(Engine::open(&home, &config), Err(code) if code == "home_already_owned"));
        drop(first);
        assert!(Engine::open(&home, &config).is_ok());
    }

    /// Produces the fixed synthetic snapshot used by the consumer contract fixture.
    #[test]
    fn snapshot_v2_fixture() {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let home = root.path().join("home");
        let config_path = root.path().join("config.toml");
        std::fs::write(
            &config_path,
            r#"
[[sources]]
account = "acct-test"
provider = "codex"
auth_family = "openai"
credential = "native:codex"
native_directory = "/synthetic"
models = [{ id = "model-a" }]
[sources.harness]
id = "codex"
binary = "/bin/true"
[sources.collector]
command = "/bin/true"
source = "fixture-source"
"#,
        )
        .unwrap();
        let engine = Engine::open(&home, &config_path).unwrap();
        let account: AccountId = "acct-test".parse().unwrap();
        let config_revision = engine.settings.lock().unwrap().revision.clone();
        *engine.last_round.lock().unwrap() = Some(RoundReport {
            observed_at: 10000.0,
            ok: false,
            error: None,
            config_revision: Some(config_revision.clone()),
            sources: vec![SourceReport {
                account: "acct-test".into(),
                provider: "codex".into(),
                source: "fixture-source".into(),
                status: "failed",
                windows: 0,
                issues: vec!["collector_timeout".into()],
            }],
        });
        let mut unknown = engine
            .snapshot_at(&["acct-test".into()], Some("model-a"), 10000.0)
            .unwrap();
        unknown["source_epoch"] = json!("00000000000000000000000000000000");
        unknown["config_revision"] =
            json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        unknown["coverage"]["last_collection"]["config_revision"] =
            unknown["config_revision"].clone();
        let expected_unknown: Value =
            serde_json::from_str(include_str!("../tests/fixtures/unknown-snapshot-v2.json"))
                .unwrap();
        assert_eq!(json!({"version":1,"result":unknown}), expected_unknown);
        let scope = CollectorScope {
            runtime: "codex".into(),
            source: "fixture-source".into(),
            models: BTreeSet::from(["model-a".into()]),
        };
        let raw = json!({"version":1,"windows":[{"pool":"primary","window":"five_hour",
            "models":["model-a"],"remaining_percent":80,"observed_at":10000,
            "valid_until":10900,"reset_at":20000}]});
        let snapshot =
            normalize_collector_output(&account, &scope, &raw, 10000.0, 256, 256).unwrap();
        engine
            .store
            .lock()
            .unwrap()
            .record_snapshot("codex", &snapshot, 1000, 10000.0)
            .unwrap();
        *engine.last_round.lock().unwrap() = Some(RoundReport {
            observed_at: 10000.0,
            ok: true,
            error: None,
            config_revision: Some(config_revision),
            sources: vec![SourceReport {
                account: "acct-test".into(),
                provider: "codex".into(),
                source: "fixture-source".into(),
                status: "collected",
                windows: 1,
                issues: vec![],
            }],
        });
        let mut result = engine
            .snapshot_at(&["acct-test".into()], Some("model-a"), 10000.0)
            .unwrap();
        result["source_epoch"] = json!("00000000000000000000000000000000");
        result["config_revision"] =
            json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        result["coverage"]["last_collection"]["config_revision"] =
            result["config_revision"].clone();
        let expected: Value =
            serde_json::from_str(include_str!("../tests/fixtures/snapshot-v2.json")).unwrap();
        assert_eq!(json!({"version":1,"result":result}), expected);
        let zero = json!({"version":1,"windows":[{"pool":"primary","window":"five_hour",
            "models":["model-a"],"remaining_percent":0,"observed_at":10001,
            "valid_until":10901,"reset_at":20000}]});
        let zero = normalize_collector_output(&account, &scope, &zero, 10001.0, 256, 256).unwrap();
        engine
            .store
            .lock()
            .unwrap()
            .record_snapshot("codex", &zero, 1000, 10001.0)
            .unwrap();
        let config_revision = engine.settings.lock().unwrap().revision.clone();
        *engine.last_round.lock().unwrap() = Some(RoundReport {
            observed_at: 10001.0,
            ok: true,
            error: None,
            config_revision: Some(config_revision),
            sources: vec![SourceReport {
                account: "acct-test".into(),
                provider: "codex".into(),
                source: "fixture-source".into(),
                status: "collected",
                windows: 1,
                issues: vec![],
            }],
        });
        let mut exhausted = engine
            .snapshot_at(&["acct-test".into()], Some("model-a"), 10001.0)
            .unwrap();
        exhausted["source_epoch"] = json!("00000000000000000000000000000000");
        exhausted["config_revision"] =
            json!("aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        exhausted["coverage"]["last_collection"]["config_revision"] =
            exhausted["config_revision"].clone();
        let expected_exhausted: Value =
            serde_json::from_str(include_str!("../tests/fixtures/exhausted-snapshot-v2.json"))
                .unwrap();
        assert_eq!(json!({"version":1,"result":exhausted}), expected_exhausted);
    }
}

/// Waits for a cooperative stop signal or the sender's disappearance.
async fn stopped(mut shutdown: watch::Receiver<bool>) {
    loop {
        if *shutdown.borrow() || shutdown.changed().await.is_err() {
            return;
        }
    }
}

/// Owns the local API, collector task, signal handling, and their shutdown.
pub(crate) async fn serve(
    engine: Arc<Engine>,
    socket: PathBuf,
    allowed_uids: Vec<u32>,
) -> Result<(), String> {
    let mut term = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .map_err(|_| "signal_setup_failed".to_owned())?;
    let (stop, receiver) = watch::channel(false);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let api_engine = Arc::clone(&engine);
    let api_stop = receiver.clone();
    let mut server = tokio::spawn(async move {
        crate::ipc::serve_ready(
            &socket,
            allowed_uids,
            move |request| {
                let result = match request.method.as_str() {
                    "status" => api_engine.status(),
                    "snapshot" => api_engine.snapshot(&request.accounts, request.model.as_deref()),
                    "ingest" => request
                        .observation
                        .ok_or("invalid_observation")
                        .and_then(|value| api_engine.ingest(value)),
                    _ => Err("unsupported_method"),
                };
                match result {
                    Ok(value) => crate::ipc::Reply::ok(value),
                    Err(code) => crate::ipc::Reply::error(code),
                }
            },
            stopped(api_stop),
            ready_tx,
        )
        .await
    });
    tokio::select! {
        ready = ready_rx => {
            if ready.is_err() {
                return server.await.map_err(|_| "api_task_failed".to_owned())?.and(Err("api_start_failed".into()));
            }
        }
        result = &mut server => {
            return result.map_err(|_| "api_task_failed".to_owned())?.and(Err("api_stopped".into()));
        }
    }
    let mut worker = tokio::spawn(Arc::clone(&engine).poll(receiver));
    let mut server_result = None;
    let mut worker_result = None;
    let mut failure = None;
    tokio::select! {
        result = &mut server => { server_result = Some(result); failure = Some("api_stopped".to_owned()); }
        result = &mut worker => { worker_result = Some(result); failure = Some("poller_stopped".to_owned()); }
        result = tokio::signal::ctrl_c() => { if result.is_err() { failure = Some("signal_failed".to_owned()); } }
        _ = term.recv() => {}
    }
    let _ = stop.send(true);
    let server_result = match server_result {
        Some(result) => result,
        None => server.await,
    };
    let worker_result = match worker_result {
        Some(result) => result,
        None => worker.await,
    };
    server_result.map_err(|_| "api_task_failed".to_owned())??;
    worker_result.map_err(|_| "poller_task_failed".to_owned())?;
    match failure {
        Some(code) => Err(code),
        None => Ok(()),
    }
}

/// A current forecast of one physical window and its exact model membership.
#[derive(Serialize)]
struct WindowView {
    runtime: String,
    lane: String,
    pool: String,
    source: String,
    window: String,
    target: Option<String>,
    models: Option<BTreeSet<String>>,
    observed_at: Option<f64>,
    valid_until: Option<f64>,
    last_remaining_percent: Option<f64>,
    last_reset_at: Option<f64>,
    forecast: Forecast,
    evaluation: Option<WindowExplanation>,
}

/// Clock validity of a model's state class for the evaluated snapshot.
#[derive(Debug, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum StateValidity {
    /// The state class may change at this exclusive Unix-seconds boundary.
    Until { at: f64 },
    /// An active latch has no clock reset; a new decision must still reread Quota.
    LatchedNoReset,
    /// The state class is already unknown.
    Unknown,
}

/// One configured provider/model view, using the quota-only score from agent-run.
#[derive(Serialize)]
struct ModelView {
    provider: String,
    model: String,
    native_model: String,
    state: &'static str,
    state_validity: StateValidity,
    quota_score: Option<f64>,
    limiting_window: Option<Value>,
    remaining_percent: Option<f64>,
    burn_percent_per_hour: Option<f64>,
    reset_at: Option<f64>,
    risk: String,
    evidence: &'static str,
    newest_observed_at: Option<f64>,
    exhausted_until: Option<f64>,
    pools: Vec<String>,
}

/// A persistent exhausted window, including whether its known reset has passed.
#[derive(Serialize)]
struct LatchView {
    lane: String,
    pool: String,
    source: String,
    window: String,
    observed_at: f64,
    reset_at: Option<f64>,
    models: Option<BTreeSet<String>>,
    active: bool,
}

/// A bounded account response, with physical windows emitted once rather than per alias.
#[derive(Serialize)]
struct AccountView {
    account: String,
    models: Vec<ModelView>,
    windows: Vec<WindowView>,
    latches: Vec<LatchView>,
    advice: Vec<advice::CapacityAdvice>,
    advice_key: String,
}

/// Returns whether a stored window governs a native model, including the legacy fallback.
fn governs(membership: &Option<BTreeSet<String>>, lane: &str, model: &str) -> bool {
    match membership {
        Some(models) => models.contains(model),
        None => lane == model,
    }
}

/// Omits derived values outside finite JSON numbers after ranking has used the original forecast.
fn sanitize_derived(forecast: &mut Forecast, evaluation: &mut Option<WindowExplanation>) {
    for value in [
        &mut forecast.burn_percent_per_hour,
        &mut forecast.sustainable_percent_per_hour,
        &mut forecast.burn_span_seconds,
    ] {
        if value.is_some_and(|number| !number.is_finite()) {
            *value = None;
        }
    }
    if evaluation
        .as_ref()
        .is_some_and(|item| !item.slack.is_finite())
    {
        *evaluation = None;
    }
    if let Some(item) = evaluation
        && item
            .projected_percent
            .is_some_and(|value| !value.is_finite())
    {
        item.projected_percent = None;
    }
}

/// Returns a conservative state-class boundary for a fixed snapshot, not a score guarantee.
fn state_validity(
    state: &str,
    windows: &[&WindowView],
    latches: &[&LatchView],
) -> Result<StateValidity, &'static str> {
    if state == "unknown" {
        return Ok(StateValidity::Unknown);
    }
    if state == "exhausted" && !latches.is_empty() {
        if latches.iter().any(|latch| latch.reset_at.is_none()) {
            return Ok(StateValidity::LatchedNoReset);
        }
        let at = latches
            .iter()
            .filter_map(|latch| latch.reset_at)
            .min_by(f64::total_cmp)
            .filter(|at| at.is_finite())
            .ok_or("internal_error")?;
        return Ok(StateValidity::Until { at });
    }
    if !matches!(state, "known" | "exhausted") || windows.is_empty() {
        return Err("internal_error");
    }
    let mut at = f64::INFINITY;
    for window in windows {
        let valid_until = window
            .valid_until
            .filter(|value| value.is_finite() && *value >= 0.0)
            .ok_or("internal_error")?;
        at = at.min(valid_until);
        if let Some(reset) = window.forecast.reset_at {
            at = at.min(reset);
        }
    }
    if !at.is_finite() {
        return Err("internal_error");
    }
    Ok(StateValidity::Until { at })
}

/// Builds quota-only forecasts and model states without selecting an account or launching work.
/// Rows missing either observation bound are not treated as fresh evidence.
fn account_view(
    account: &AccountId,
    history: AccountQuotaHistory,
    config: &QuotaConfig,
    at: f64,
    model_filter: Option<&str>,
) -> Result<AccountView, &'static str> {
    let mut bindings = BTreeSet::new();
    for source in config
        .sources
        .iter()
        .filter(|source| &source.account == account)
    {
        for model in &source.models {
            if model_filter.is_none_or(|wanted| wanted == model.id || wanted == model.lane()) {
                bindings.insert((
                    source.provider.clone(),
                    model.id.clone(),
                    model.lane().to_owned(),
                ));
            }
        }
    }
    let lanes: BTreeSet<String> = bindings.iter().map(|(_, _, lane)| lane.clone()).collect();
    let mut pool_latest: BTreeMap<String, f64> = BTreeMap::new();
    for window in &history.windows {
        if let Some(observed) = window.observed_at {
            pool_latest
                .entry(window.lane.clone())
                .and_modify(|latest| *latest = (*latest).max(observed))
                .or_insert(observed);
        }
    }
    let mut groups: BTreeMap<(String, String, String), Vec<StoredWindow>> = BTreeMap::new();
    for window in history.windows {
        groups
            .entry((
                window.lane.clone(),
                window.source.clone(),
                window.window.clone(),
            ))
            .or_default()
            .push(window);
    }
    let mut windows = Vec::with_capacity(groups.len());
    for ((lane, source, name), rows) in groups {
        let latest = rows.first().ok_or("internal_error")?;
        if model_filter.is_some()
            && !lanes
                .iter()
                .any(|model| governs(&latest.models, &lane, model))
        {
            continue;
        }
        let key = Key {
            runtime: account.as_str().to_owned(),
            lane: lane.clone(),
            window: name.clone(),
            target: None,
            source: source.clone(),
        };
        let samples: Vec<Sample> = rows
            .iter()
            .map(|row| Sample {
                key: key.clone(),
                remaining_percent: row.remaining_percent,
                reset_at: row.reset_at,
                observed_at: row.observed_at,
                valid_until: row.valid_until,
            })
            .collect();
        // The ingest contract always supplies both bounds. A legacy or manually
        // altered row without them cannot confirm freshness for API consumers.
        let bounded_samples = match (latest.observed_at, latest.valid_until) {
            (Some(observed), Some(valid))
                if observed.is_finite()
                    && observed >= 0.0
                    && valid.is_finite()
                    && valid >= observed
                    && latest.remaining_percent.is_none_or(|value| {
                        value.is_finite() && (0.0..=100.0).contains(&value)
                    })
                    && latest
                        .reset_at
                        .is_none_or(|value| value.is_finite() && value >= 0.0) =>
            {
                samples.as_slice()
            }
            _ => &[],
        };
        let mut predicted = forecast::forecast(&key, bounded_samples, at);
        // Preserve the source ranker's treatment of an invalid forecast before
        // replacing non-finite derived values with JSON null.
        let mut evaluation = forecast::window_explanation(&predicted, at);
        sanitize_derived(&mut predicted, &mut evaluation);
        windows.push(WindowView {
            runtime: latest.runtime.clone(),
            lane: lane.clone(),
            pool: format!("{}::{lane}", account.as_str()),
            source,
            window: name,
            target: latest.target.clone(),
            models: latest.models.clone(),
            observed_at: latest.observed_at,
            valid_until: latest.valid_until,
            last_remaining_percent: latest.remaining_percent,
            last_reset_at: latest.reset_at,
            forecast: predicted,
            evaluation,
        });
    }
    let latches: Vec<LatchView> = history
        .latches
        .into_iter()
        .filter(|latch| {
            model_filter.is_none()
                || lanes
                    .iter()
                    .any(|model| governs(&latch.models, &latch.lane, model))
        })
        .map(|latch: ExhaustionLatch| LatchView {
            pool: format!("{}::{}", account.as_str(), latch.lane),
            active: latch.reset_at.is_none_or(|reset| reset > at),
            lane: latch.lane,
            source: latch.source,
            window: latch.window,
            observed_at: latch.observed_at,
            reset_at: latch.reset_at,
            models: latch.models,
        })
        .collect();
    if bindings.len() > MAX_VIEW_MODELS
        || bindings.len().saturating_mul(windows.len() + latches.len()) > MAX_VIEW_COMPARISONS
    {
        return Err("response_too_large");
    }
    let mut models = Vec::with_capacity(bindings.len());
    for (provider, model, native_model) in bindings {
        let governing: Vec<&WindowView> = windows
            .iter()
            .filter(|window| governs(&window.models, &window.lane, &native_model))
            .collect();
        let mut active: Vec<&LatchView> = latches
            .iter()
            .filter(|latch| latch.active && governs(&latch.models, &latch.lane, &native_model))
            .collect();
        active.sort_by(|left, right| {
            left.pool
                .cmp(&right.pool)
                .then_with(|| left.reset_at.is_none().cmp(&right.reset_at.is_none()))
                .then_with(|| {
                    left.reset_at
                        .unwrap_or(f64::INFINITY)
                        .total_cmp(&right.reset_at.unwrap_or(f64::INFINITY))
                })
                .then_with(|| (&left.source, &left.window).cmp(&(&right.source, &right.window)))
        });
        let mut pools: BTreeSet<String> =
            governing.iter().map(|window| window.pool.clone()).collect();
        pools.extend(active.iter().map(|latch| latch.pool.clone()));
        let unknown =
            governing.is_empty() || governing.iter().any(|window| window.evaluation.is_none());
        let zero_window = governing
            .iter()
            .filter_map(|window| window.evaluation.as_ref().map(|score| (*window, score)))
            .filter(|(_, score)| score.remaining_percent == 0.0)
            .min_by(|(left, a), (right, b)| {
                a.reset_at
                    .is_none()
                    .cmp(&b.reset_at.is_none())
                    .then_with(|| {
                        a.reset_at
                            .unwrap_or(f64::INFINITY)
                            .total_cmp(&b.reset_at.unwrap_or(f64::INFINITY))
                    })
                    .then_with(|| {
                        (&left.lane, &left.window, &left.source).cmp(&(
                            &right.lane,
                            &right.window,
                            &right.source,
                        ))
                    })
            });
        let lowest_slack = governing
            .iter()
            .filter_map(|window| window.evaluation.as_ref().map(|score| (*window, score)))
            .min_by(|(left, a), (right, b)| {
                a.slack
                    .partial_cmp(&b.slack)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a.reset_at.is_none().cmp(&b.reset_at.is_none()))
                    .then_with(|| {
                        a.reset_at
                            .unwrap_or(f64::INFINITY)
                            .total_cmp(&b.reset_at.unwrap_or(f64::INFINITY))
                    })
                    .then_with(|| {
                        (&left.lane, &left.window, &left.source).cmp(&(
                            &right.lane,
                            &right.window,
                            &right.source,
                        ))
                    })
            });
        let state = if !active.is_empty() {
            "exhausted"
        } else if unknown {
            "unknown"
        } else if zero_window.is_some() {
            "exhausted"
        } else {
            "known"
        };
        let state_validity = state_validity(state, &governing, &active)?;
        let limiting = match state {
            "exhausted" if active.is_empty() => zero_window,
            "known" => lowest_slack,
            _ => None,
        };
        let quota_score = if state == "known" {
            limiting.map(|(_, score)| 1.0 + score.slack)
        } else {
            None
        };
        let limiting_window = active.first().map(|latch| json!({"pool":latch.pool,"source":latch.source,"window":latch.window}))
            .or_else(|| limiting.map(|(window, _)| json!({"pool":window.pool,"source":window.source,"window":window.window})));
        let matching = match active.first() {
            Some(latch) => governing.iter().copied().find(|window| {
                window.pool == latch.pool
                    && window.source == latch.source
                    && window.window == latch.window
            }),
            None => limiting.map(|(window, _)| window),
        };
        let remaining_percent = matching.and_then(|window| window.forecast.remaining_percent);
        let burn_percent_per_hour =
            matching.and_then(|window| window.forecast.burn_percent_per_hour);
        let reset_at = match active.first() {
            Some(latch) => latch.reset_at,
            None => matching.and_then(|window| window.forecast.reset_at),
        };
        let risk = matching.map_or_else(
            || "unknown".to_owned(),
            |window| window.forecast.risk.clone(),
        );
        let newest_observed_at = pool_latest
            .iter()
            .filter(|(lane, _)| pools.contains(&format!("{}::{lane}", account.as_str())))
            .map(|(_, observed)| *observed)
            .fold(None, |latest: Option<f64>, value| {
                Some(latest.map_or(value, |current| current.max(value)))
            });
        let evidence = if state == "known" || state == "exhausted" {
            "fresh"
        } else if newest_observed_at.is_some() {
            "stale"
        } else {
            "missing"
        };
        let exhausted_until = if state == "exhausted" { reset_at } else { None };
        models.push(ModelView {
            provider,
            model,
            native_model,
            state,
            state_validity,
            quota_score,
            limiting_window,
            remaining_percent,
            burn_percent_per_hour,
            reset_at,
            risk,
            evidence,
            newest_observed_at,
            exhausted_until,
            pools: pools.into_iter().collect(),
        });
    }
    let forecasts: Vec<Forecast> = windows
        .iter()
        .map(|window| window.forecast.clone())
        .collect();
    let advice = advice::build_advice(&forecasts);
    let advice_key = advice::advice_key(&advice);
    Ok(AccountView {
        account: account.as_str().to_owned(),
        models,
        windows,
        latches,
        advice,
        advice_key,
    })
}

impl Engine {
    /// Reads one bounded, internally consistent snapshot for the requested configured accounts.
    pub(crate) fn snapshot(
        &self,
        accounts: &[String],
        model_filter: Option<&str>,
    ) -> Result<Value, &'static str> {
        self.snapshot_at(accounts, model_filter, now())
    }

    /// Builds a snapshot at one explicit evaluation clock for deterministic parity checks.
    fn snapshot_at(
        &self,
        accounts: &[String],
        model_filter: Option<&str>,
        at: f64,
    ) -> Result<Value, &'static str> {
        if accounts.len() > 256 {
            return Err("invalid_scope");
        }
        let settings = self.settings.lock().map_err(|_| "internal_error")?;
        let configured: BTreeSet<AccountId> = settings
            .config
            .sources
            .iter()
            .map(|source| source.account.clone())
            .collect();
        let requested: BTreeSet<AccountId> = if accounts.is_empty() {
            configured.clone()
        } else {
            accounts
                .iter()
                .map(|account| AccountId::from_str(account).map_err(|_| "invalid_scope"))
                .collect::<Result<_, _>>()?
        };
        if !requested.is_subset(&configured) {
            return Err("unknown_account");
        }
        if let Some(wanted) = model_filter
            && !settings.config.sources.iter().any(|source| {
                requested.contains(&source.account)
                    && source
                        .models
                        .iter()
                        .any(|model| wanted == model.id || wanted == model.lane())
            })
        {
            return Err("unknown_model");
        }
        let mut unobserved: Vec<Value> = settings.config.sources.iter()
            .filter(|source| requested.contains(&source.account) && source.limits_source == LimitsSource::None)
            .map(|source| json!({"account":source.account,"provider":source.provider,"reason":"limits_source_none"}))
            .collect();
        let store = self.store.lock().map_err(|_| "internal_error")?;
        let mut views = Vec::with_capacity(requested.len());
        let mut revision = None;
        let mut model_count = 0usize;
        let mut window_count = 0usize;
        let mut comparison_count = 0usize;
        for account in &requested {
            let history = store.read_account(account).map_err(|error| match error {
                StoreError::HistoryTooLarge => "history_too_large",
                _ => "storage_error",
            })?;
            if revision.is_some_and(|previous| previous != history.revision) {
                return Err("internal_error");
            }
            revision = Some(history.revision);
            let view = account_view(account, history, &settings.config, at, model_filter)?;
            model_count = model_count.saturating_add(view.models.len());
            let windows = view.windows.len().saturating_add(view.latches.len());
            window_count = window_count.saturating_add(windows);
            comparison_count =
                comparison_count.saturating_add(view.models.len().saturating_mul(windows));
            if model_count > MAX_VIEW_MODELS
                || window_count > MAX_VIEW_WINDOWS
                || comparison_count > MAX_VIEW_COMPARISONS
            {
                return Err("response_too_large");
            }
            views.push(view);
        }
        let last_round = self
            .last_round
            .lock()
            .map_err(|_| "internal_error")?
            .clone();
        let external = self.external_reports.lock().map_err(|_| "internal_error")?;
        for source in settings.sources.iter().filter(|source| {
            source.execution == ExecutionMode::External && requested.contains(&source.account)
        }) {
            let id = source.source().ok_or("internal_error")?;
            let key = (source.account.as_str().to_owned(), id.clone());
            if external
                .get(&key)
                .is_none_or(|report| report.config_revision != settings.revision)
            {
                unobserved.push(json!({"account":source.account,"provider":source.provider,
                    "source":id,"reason":"external_not_received"}));
            }
        }
        drop(external);
        drop(store);
        let coverage = match &last_round {
            None => "unknown",
            Some(report)
                if report.ok
                    && unobserved.is_empty()
                    && report.config_revision.as_deref() == Some(&settings.revision) =>
            {
                "complete"
            }
            Some(_) => "partial",
        };
        Ok(
            json!({"schema_version":2,"source_epoch":self.source_epoch,"revision":revision,
            "config_revision":settings.revision,"evaluated_at":at,"accounts":views,
            "coverage":{"state":coverage,"unobserved":unobserved,"last_collection":last_round}}),
        )
    }
}
