//! Durable account-scoped quota observations and exhaustion latches.
//!
//! Each mutating observation advances one global revision in the same SQLite
//! transaction. History is bounded globally, while the newest sample needed
//! to interpret every durable latch survives retention.

use crate::quota::{
    AccountId, NormalizedQuotaSnapshot, PhysicalQuotaKey, QuotaError, QuotaModelObservation,
    QuotaWindow,
};
use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use std::{
    collections::{BTreeMap, BTreeSet},
    fmt,
    fs::OpenOptions,
    io::{self, Read},
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
};

/// A quota database failure, including the underlying I/O or SQLite cause.
#[derive(Debug)]
pub enum StoreError {
    /// The supplied snapshot or persistence argument was invalid.
    Invalid(String),
    /// An existing file does not carry this application's database identity.
    ForeignDatabase,
    /// The database version is not supported by this build.
    UnsupportedVersion(i64),
    /// The account's retained history exceeds the read budget.
    HistoryTooLarge,
    /// Filesystem access failed.
    Io(io::Error),
    /// SQLite access or an atomic transaction failed.
    Sqlite(rusqlite::Error),
}

impl fmt::Display for StoreError {
    /// Describes the error without exposing collected data.
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(formatter, "{message}"),
            Self::ForeignDatabase => write!(formatter, "unrecognized quota database"),
            Self::UnsupportedVersion(version) => {
                write!(formatter, "unsupported quota database version {version}")
            }
            Self::HistoryTooLarge => write!(formatter, "quota history exceeds the read budget"),
            Self::Io(error) => write!(formatter, "quota database I/O: {error}"),
            Self::Sqlite(error) => write!(formatter, "quota database: {error}"),
        }
    }
}

impl std::error::Error for StoreError {
    /// Preserves the underlying filesystem or SQLite failure.
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io(error) => Some(error),
            Self::Sqlite(error) => Some(error),
            _ => None,
        }
    }
}
impl From<io::Error> for StoreError {
    /// Wraps a filesystem failure.
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}
impl From<rusqlite::Error> for StoreError {
    /// Wraps a SQLite failure.
    fn from(error: rusqlite::Error) -> Self {
        Self::Sqlite(error)
    }
}
impl From<QuotaError> for StoreError {
    /// Wraps quota validation failure for callers of the persistence API.
    fn from(error: QuotaError) -> Self {
        Self::Invalid(error.to_string())
    }
}
/// A quota persistence operation result.
pub type Result<T> = std::result::Result<T, StoreError>;

/// Builds an invalid-input error.
fn invalid(message: &str) -> StoreError {
    StoreError::Invalid(message.to_owned())
}

/// One persisted physical window; `models: None` means historical membership
/// was absent or malformed, whereas `Some(empty)` means explicitly empty.
#[derive(Debug, Clone, PartialEq)]
pub struct StoredWindow {
    /// Monotonic SQLite row identifier, used to break observation-time ties.
    pub id: i64,
    /// Observing runtime.
    pub runtime: String,
    /// Account-local physical lane.
    pub lane: String,
    /// Provider window name.
    pub window: String,
    /// Legacy target identifier, retained for compatible diagnostics.
    pub target: Option<String>,
    /// Stable collector identity.
    pub source: String,
    /// Remaining percentage, or unknown.
    pub remaining_percent: Option<f64>,
    /// Provider reset time as Unix seconds, or unknown.
    pub reset_at: Option<f64>,
    /// Observation time in Unix seconds, or absent for legacy history.
    pub observed_at: Option<f64>,
    /// Validity limit in Unix seconds, or absent for legacy history.
    pub valid_until: Option<f64>,
    /// Exact models governed by this window, or absent legacy membership.
    pub models: Option<BTreeSet<String>>,
}

/// An active exhausted physical window, independent of sample retention.
#[derive(Debug, Clone, PartialEq)]
pub struct ExhaustionLatch {
    /// Account-local physical lane.
    pub lane: String,
    /// Stable collector identity.
    pub source: String,
    /// Provider window name.
    pub window: String,
    /// Time of the exhaustion observation in Unix seconds.
    pub observed_at: f64,
    /// Provider reset time in Unix seconds, or unknown.
    pub reset_at: Option<f64>,
    /// Newest retained membership; `None` retains the legacy fallback.
    pub models: Option<BTreeSet<String>>,
}

/// Consistent snapshot of one account's history and durable latches.
#[derive(Debug, Clone, PartialEq)]
pub struct AccountQuotaHistory {
    /// Global committed capacity revision, shared by all accounts.
    pub revision: i64,
    /// Samples ordered newest first by `(observed_at, id)`.
    pub windows: Vec<StoredWindow>,
    /// Latches ordered by `(lane, source, window)`.
    pub latches: Vec<ExhaustionLatch>,
}

/// An owned SQLite connection to this application's versioned quota database.
#[derive(Debug)]
pub struct QuotaStore {
    conn: Connection,
}

/// The database application identifier, ASCII `AQTA`.
const APPLICATION_ID: i64 = 1_095_849_025;
/// The only supported database format.
const SCHEMA_VERSION: i64 = 1;
/// One account read may not silently load more than this many history rows.
const MAX_ACCOUNT_HISTORY: i64 = 50_000;

/// Verifies identity and version without changing database content.
fn verify_database(conn: &Connection) -> Result<()> {
    let app: i64 = conn.pragma_query_value(None, "application_id", |row| row.get(0))?;
    if app != APPLICATION_ID {
        return Err(StoreError::ForeignDatabase);
    }
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version != SCHEMA_VERSION {
        return Err(StoreError::UnsupportedVersion(version));
    }
    Ok(())
}

/// Current Unix seconds, matching the source store's revision timestamp.
fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64()
}

/// Reads the singleton revision from the caller's transaction.
fn revision(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row(
        "SELECT revision FROM quota_capacity_revision WHERE id=1",
        [],
        |row| row.get(0),
    )?)
}

/// Advances the revision exactly once, rejecting integer exhaustion atomically.
fn advance_revision(conn: &Connection) -> Result<i64> {
    if conn.execute("UPDATE quota_capacity_revision SET revision=revision+1,updated_at=? WHERE id=1 AND revision<9223372036854775807", [now()])? != 1 {
        return Err(invalid("quota capacity revision cannot advance"));
    }
    revision(conn)
}

/// Reads optional membership exactly as agent-run does for historical rows.
fn parse_membership(payload: Option<String>) -> Option<BTreeSet<String>> {
    payload
        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
        .and_then(|value| {
            value["models"].as_array().map(|models| {
                models
                    .iter()
                    .filter_map(|model| model.as_str().map(str::to_owned))
                    .collect()
            })
        })
}

impl QuotaStore {
    /// Opens an existing recognized database or initializes a newly created file.
    /// Existing foreign and unsupported versions are rejected before schema changes.
    ///
    /// # Errors
    /// Returns I/O, SQLite, identity, and schema version errors without replacing
    /// existing data. A failed initialization can leave its newly created file.
    pub fn open(path: &Path) -> Result<Self> {
        let parent = path
            .parent()
            .ok_or_else(|| invalid("quota database needs a parent"))?;
        let parent_metadata = std::fs::symlink_metadata(parent)?;
        // SAFETY: geteuid has no pointer arguments or side effects.
        let uid = unsafe { libc::geteuid() };
        if !parent_metadata.file_type().is_dir()
            || parent_metadata.uid() != uid
            || parent_metadata.mode() & 0o077 != 0
        {
            return Err(invalid("quota database directory must be owner-private"));
        }
        let created = match OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(path)
        {
            Ok(file) => {
                drop(file);
                true
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => false,
            Err(error) => return Err(error.into()),
        };
        if !created {
            let metadata = std::fs::symlink_metadata(path)?;
            if !metadata.file_type().is_file()
                || metadata.uid() != uid
                || metadata.mode() & 0o077 != 0
                || metadata.nlink() != 1
            {
                return Err(StoreError::ForeignDatabase);
            }
            let readonly = Connection::open_with_flags(
                path,
                OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )?;
            verify_database(&readonly)?;
        }
        let mut conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NOFOLLOW,
        )?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if created {
            // Recheck under the write lock: another opener must not have populated
            // the newly created file between filesystem creation and this lock.
            let app: i64 = tx.pragma_query_value(None, "application_id", |row| row.get(0))?;
            let version: i64 = tx.pragma_query_value(None, "user_version", |row| row.get(0))?;
            let objects: i64 =
                tx.query_row("SELECT count(*) FROM sqlite_schema", [], |row| row.get(0))?;
            if app != 0 || version != 0 || objects != 0 {
                return Err(StoreError::ForeignDatabase);
            }
            let mut random = [0u8; 16];
            std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
            let epoch: String = random.iter().map(|byte| format!("{byte:02x}")).collect();
            tx.execute_batch("\
                CREATE TABLE provider_accounts (account_id TEXT PRIMARY KEY);\
                CREATE TABLE capacity_samples (\
                  id INTEGER PRIMARY KEY AUTOINCREMENT, runtime TEXT NOT NULL, lane TEXT NOT NULL,\
                  window TEXT NOT NULL, target TEXT, source TEXT NOT NULL, remaining_percent REAL,\
                  reset_at REAL, observed_at REAL, valid_until REAL, payload_json TEXT,\
                  account_id TEXT NOT NULL REFERENCES provider_accounts(account_id), quota_key TEXT NOT NULL,\
                  CHECK (substr(quota_key,1,length(account_id)+2)=account_id || '::' AND length(quota_key)>length(account_id)+2));\
                CREATE TABLE quota_exhaustion (\
                  account_id TEXT NOT NULL REFERENCES provider_accounts(account_id), quota_key TEXT NOT NULL,\
                  source TEXT NOT NULL CHECK(length(trim(source))>0), window_id TEXT NOT NULL CHECK(length(trim(window_id))>0),\
                  observed_at REAL NOT NULL, reset_at REAL, collector_revision TEXT,\
                  PRIMARY KEY(account_id,quota_key,source,window_id),\
                  CHECK (substr(quota_key,1,length(account_id)+2)=account_id || '::' AND length(quota_key)>length(account_id)+2));\
                CREATE TABLE quota_capacity_revision (id INTEGER PRIMARY KEY CHECK(id=1), revision INTEGER NOT NULL CHECK(revision>=0), updated_at REAL NOT NULL);\
                INSERT INTO quota_capacity_revision VALUES(1,0,0.0);\
                CREATE TABLE quota_instance (id INTEGER PRIMARY KEY CHECK(id=1), epoch TEXT NOT NULL);\
                CREATE INDEX capacity_samples_account_order ON capacity_samples(account_id,observed_at DESC,id DESC);\
                CREATE INDEX capacity_samples_latch ON capacity_samples(quota_key,source,window,observed_at DESC,id DESC);\
                PRAGMA application_id=1095849025; PRAGMA user_version=1;")?;
            tx.execute("INSERT INTO quota_instance(id,epoch) VALUES(1,?)", [&epoch])?;
        } else {
            // The file may have changed since the read-only check.
            verify_database(&tx)?;
        }
        tx.commit()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        Ok(Self { conn })
    }

    /// Returns the database generation identity, which persists across service restarts.
    pub fn epoch(&self) -> Result<String> {
        Ok(self
            .conn
            .query_row("SELECT epoch FROM quota_instance WHERE id=1", [], |row| {
                row.get(0)
            })?)
    }

    /// Registers an account identity without storing credentials or policy.
    /// A newly inserted identity advances the revision; a repeat is a no-op.
    ///
    /// # Errors
    /// A failed insert or revision update rolls back the entire transaction.
    pub fn register_account(&mut self, account: &AccountId) -> Result<()> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let changed = tx.execute(
            "INSERT OR IGNORE INTO provider_accounts(account_id) VALUES(?)",
            [account.as_str()],
        )?;
        if changed != 0 {
            advance_revision(&tx)?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Atomically records a validated observation and returns its global revision.
    /// `retention` is a global row budget; samples required by latches are kept.
    /// `at` is the host time in Unix seconds used to evaluate resets and freshness.
    ///
    /// # Errors
    /// Invalid arguments, unregistered accounts, and storage failures leave the
    /// history, latches and revision unchanged.
    pub fn record_snapshot(
        &mut self,
        runtime: &str,
        snapshot: &NormalizedQuotaSnapshot,
        retention: usize,
        at: f64,
    ) -> Result<i64> {
        record_quota_snapshot_on(&mut self.conn, runtime, snapshot, retention, at)
    }

    /// Reads one account's history, latches, and revision in a single snapshot.
    ///
    /// # Errors
    /// Returns a SQLite error if the database cannot be read.
    pub fn read_account(&self, account: &AccountId) -> Result<AccountQuotaHistory> {
        let tx = self.conn.unchecked_transaction()?;
        let revision = revision(&tx)?;
        let windows = {
            let mut stmt = tx.prepare("SELECT id,runtime,lane,window,target,source,remaining_percent,reset_at,observed_at,valid_until,payload_json FROM capacity_samples WHERE account_id=? ORDER BY observed_at DESC,id DESC LIMIT ?")?;
            let rows =
                stmt.query_map(params![account.as_str(), MAX_ACCOUNT_HISTORY + 1], |row| {
                    Ok(StoredWindow {
                        id: row.get(0)?,
                        runtime: row.get(1)?,
                        lane: row.get(2)?,
                        window: row.get(3)?,
                        target: row.get(4)?,
                        source: row.get(5)?,
                        remaining_percent: row.get(6)?,
                        reset_at: row.get(7)?,
                        observed_at: row.get(8)?,
                        valid_until: row.get(9)?,
                        models: parse_membership(row.get(10)?),
                    })
                })?;
            let windows = rows.collect::<rusqlite::Result<Vec<_>>>()?;
            if windows.len() > MAX_ACCOUNT_HISTORY as usize {
                return Err(StoreError::HistoryTooLarge);
            }
            windows
        };
        let latch_count: i64 = tx.query_row(
            "SELECT count(*) FROM quota_exhaustion WHERE account_id=?",
            [account.as_str()],
            |row| row.get(0),
        )?;
        if latch_count > MAX_ACCOUNT_HISTORY {
            return Err(StoreError::HistoryTooLarge);
        }
        let facts = latched_windows(&tx, account)?;
        let membership = latched_membership(&tx, account, &facts)?;
        let latches = facts
            .into_iter()
            .map(|((lane, source, window), fact)| {
                let models = membership
                    .get(&(lane.clone(), source.clone(), window.clone()))
                    .cloned()
                    .flatten();
                ExhaustionLatch {
                    lane,
                    source,
                    window,
                    observed_at: fact.observed_at,
                    reset_at: fact.reset_at,
                    models,
                }
            })
            .collect();
        tx.commit()?;
        Ok(AccountQuotaHistory {
            revision,
            windows,
            latches,
        })
    }
}
/// Shelf life granted to a latched exhausted fact whose provider reset time is
/// unknown; positive fresh evidence is then the only release.
pub const UNRESET_EXHAUSTION_TTL_SECONDS: f64 = 900.0;

/// Returns the provider-independent lane of `key`, which must belong to
/// `account`; the account prefix is never persisted as a lane.
fn lane_of<'a>(key: &'a PhysicalQuotaKey, account: &AccountId) -> Result<&'a str> {
    key.as_str()
        .strip_prefix(account.as_str())
        .and_then(|rest| rest.strip_prefix("::"))
        .filter(|lane| !lane.is_empty())
        .ok_or_else(|| invalid("quota key does not belong to snapshot account"))
}

/// One physical quota window: `(lane, source, window)`. The source is part
/// of the identity, exactly as in the snapshot, the persisted membership and
/// the ranker: the same window name from another source is another fact.
type WindowKey = (String, String, String);

/// Reads the durable exhaustion latches of one global account, one per
/// physical window [`WindowKey`].
///
/// Latch rows live in `quota_exhaustion`, keyed by account, physical key,
/// stable collector source, and provider window, so they are independent of
/// rolling sample retention and of script revisions.
fn latched_windows(
    tx: &rusqlite::Connection,
    account: &AccountId,
) -> Result<BTreeMap<WindowKey, QuotaWindow>> {
    let mut stmt = tx.prepare(
        "SELECT quota_key,source,window_id,observed_at,reset_at FROM quota_exhaustion WHERE account_id=?1",
    )?;
    let rows = stmt.query_map(params![account.as_str()], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
            row.get::<_, Option<f64>>(4)?,
        ))
    })?;
    let mut latched = BTreeMap::new();
    for row in rows {
        let (quota_key, source, window_id, observed_at, reset_at) = row?;
        let lane = quota_key
            .strip_prefix(account.as_str())
            .and_then(|rest| rest.strip_prefix("::"))
            .unwrap_or_default()
            .to_owned();
        if lane.is_empty() {
            continue;
        }
        latched.insert(
            (lane, source.clone(), window_id.clone()),
            QuotaWindow {
                source,
                name: window_id,
                remaining_percent: Some(0.0),
                reset_at,
                observed_at,
                valid_until: reset_at.unwrap_or(observed_at),
            },
        );
    }
    Ok(latched)
}

/// Recorded model membership per latched physical window; `None` for a
/// legacy row without a valid `models` array.
type WindowMembership = BTreeMap<WindowKey, Option<BTreeSet<String>>>;

/// The model lanes each latched window of `account` governs, keyed by its
/// [`WindowKey`]: the membership of that exact `(lane, source, window)`'s
/// newest sample in the same `(observed_at, id)` order the ranker reads.
/// Another source's sample of the same window name never stands in for it.
/// `None` when that newest row records no valid `models` array (rows
/// written before membership was recorded); such a window governs only the
/// lane equal to its pool id, the same legacy rule the ranker applies —
/// malformed metadata is never read as a model mapping.
fn latched_membership(
    tx: &rusqlite::Connection,
    account: &AccountId,
    latched: &BTreeMap<WindowKey, QuotaWindow>,
) -> Result<WindowMembership> {
    let mut membership = BTreeMap::new();
    for (lane, source, window) in latched.keys() {
        let payload: Option<String> = tx
            .query_row(
                "SELECT payload_json FROM capacity_samples WHERE quota_key=?1 AND source=?2 \
                 AND window=?3 ORDER BY observed_at DESC,id DESC LIMIT 1",
                params![format!("{}::{}", account.as_str(), lane), source, window],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let models = payload
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|value| {
                value["models"].as_array().map(|models| {
                    models
                        .iter()
                        .filter_map(|model| model.as_str().map(str::to_owned))
                        .collect::<BTreeSet<_>>()
                })
            });
        membership.insert((lane.clone(), source.clone(), window.clone()), models);
    }
    Ok(membership)
}

/// Whether latched window `key` governs model lane `model`: through its
/// recorded membership, or for a legacy row without membership, only when
/// the pool id equals the model lane.
fn window_governs(membership: &WindowMembership, key: &WindowKey, model: &str) -> bool {
    match membership.get(key) {
        Some(Some(models)) => models.contains(model),
        _ => key.0 == model,
    }
}

/// The latches this round's evidence settles: fresh positive evidence
/// ([`authoritative_positive`]) or a fresher zero (observed after the latch
/// and not after `at`) for the same pool and window name, from any source,
/// that COVERS every model the latch governs. A latch with recorded
/// membership is settled only when each governed model reports such a
/// window in its own view, so a newer observation of another model set (for
/// example after a collector source switch) never releases it; a legacy
/// latch without membership keeps the historical any-model rule.
fn settled_latches(
    exhausted: &BTreeMap<WindowKey, QuotaWindow>,
    membership: &WindowMembership,
    snapshot: &NormalizedQuotaSnapshot,
    account: &AccountId,
    at: f64,
) -> BTreeSet<WindowKey> {
    let mut settled = BTreeSet::new();
    for (key, fact) in exhausted {
        let (lane, _, name) = key;
        let settles = |window: &QuotaWindow| {
            window.name == *name
                && (authoritative_positive(window, fact, at)
                    || (window.remaining_percent == Some(0.0)
                        && window.observed_at > fact.observed_at
                        && window.observed_at <= at))
        };
        let reports = |model: &QuotaModelObservation| {
            model.pools.iter().any(|pool| {
                lane_of(&pool.key, account).ok() == Some(lane.as_str())
                    && pool.windows.iter().any(settles)
            })
        };
        let covered = match membership.get(key) {
            Some(Some(governed)) => governed.iter().all(|governed| {
                snapshot
                    .models
                    .iter()
                    .any(|model| model.model == *governed && reports(model))
            }),
            _ => snapshot.models.iter().any(reports),
        };
        if covered {
            settled.insert(key.clone());
        }
    }
    settled
}

/// Extends one latched exhausted window's survival horizon without inventing a
/// new observation time: validity reaches the known reset, or one TTL past
/// `at` when the provider reported none.
fn extend_survival(window: &QuotaWindow, at: f64) -> QuotaWindow {
    let mut carried = window.clone();
    carried.valid_until = carried
        .reset_at
        .unwrap_or(at + UNRESET_EXHAUSTION_TTL_SECONDS)
        .max(carried.valid_until);
    carried
}

/// Whether a positive `window` can release `fact` at host time `at`.
fn authoritative_positive(window: &QuotaWindow, fact: &QuotaWindow, at: f64) -> bool {
    window
        .remaining_percent
        .is_some_and(|remaining| remaining > 0.0)
        && window.observed_at >= fact.observed_at
        && window.observed_at <= at
        && window.valid_until >= at
        && window.reset_at.is_none_or(|reset| reset > at)
}

/// Applies the exhaustion latch to `snapshot` in place.
///
/// A latched zero-remaining window survives a round that only produced
/// unknown data for its pool (empty or `None` remaining) until its known
/// reset time passes or complete fresh evidence settles every governed model,
/// including when the prior reset is known. Positive evidence requires
/// `remaining > 0`, observation at least as late as the exhausted fact and not
/// later than `at`, unexpired validity and no passed reset. A fresher zero can
/// replace the old fact as described by `settled_latches`, from any source.
/// Fresh exhaustion, fresh
/// percentages, and expired resets are never overridden; carried facts keep
/// their original observation times and collector source.
///
/// A latched window is carried only into models it governs according to
/// `membership` (the newest recorded membership of that exact source's
/// window, see `window_governs`); it is never copied into another model
/// that merely shares the pool, even when the window is omitted or unknown
/// this round. Only evidence that covers every governed model settles it
/// (see `settled_latches`).
fn retain_exhausted(
    exhausted: &BTreeMap<WindowKey, QuotaWindow>,
    membership: &WindowMembership,
    snapshot: &mut NormalizedQuotaSnapshot,
    account: &AccountId,
    at: f64,
) -> Result<()> {
    let settled = settled_latches(exhausted, membership, snapshot, account, at);
    for model in &mut snapshot.models {
        let model_lane = model.model.clone();
        for pool in &mut model.pools {
            let lane = lane_of(&pool.key, account)?.to_owned();
            // Only actually fresh covering evidence releases the latch: a
            // stale or expired positive observation never revives capacity,
            // and unknown or missing data displaces nothing. The carried fact
            // replaces this model's same-named windows from any source.
            let mut carried: Vec<QuotaWindow> = Vec::new();
            for (key, fact) in exhausted {
                let (l, _, name) = key;
                if l != &lane
                    || fact.reset_at.is_some_and(|reset| reset <= at)
                    || settled.contains(key)
                    || !window_governs(membership, key, &model_lane)
                {
                    continue;
                }
                pool.windows.retain(|w| w.name != *name);
                carried.push(extend_survival(fact, at));
            }
            pool.windows.extend(carried);
        }
    }
    Ok(())
}
/// Body of [`QuotaStore::record_snapshot`] on one connection.
fn record_quota_snapshot_on(
    conn: &mut rusqlite::Connection,
    runtime: &str,
    snapshot: &NormalizedQuotaSnapshot,
    retention: usize,
    at: f64,
) -> Result<i64> {
    if runtime.is_empty() || runtime.len() > 512 || retention == 0 {
        return Err(invalid("invalid quota observation persistence"));
    }
    snapshot.validate()?;
    let account = snapshot.account.clone();
    let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
    let registered: i64 = tx.query_row(
        "SELECT COUNT(*) FROM provider_accounts WHERE account_id=?1",
        params![account.as_str()],
        |row| row.get(0),
    )?;
    if registered != 1 {
        return Err(invalid("quota account is not registered"));
    }
    let exhausted = latched_windows(&tx, &account)?;
    let membership = latched_membership(&tx, &account, &exhausted)?;
    // A latch is released when its reset passed or this round's evidence
    // covers every model it governs; a settling fresher zero is re-latched
    // below under its own source.
    let mut released = settled_latches(&exhausted, &membership, snapshot, &account, at);
    released.extend(
        exhausted
            .iter()
            .filter(|(_, fact)| fact.reset_at.is_some_and(|reset| reset <= at))
            .map(|(key, _)| key.clone()),
    );
    let mut snapshot = snapshot.clone();
    retain_exhausted(&exhausted, &membership, &mut snapshot, &account, at)?;

    // One physical window is selected at most once with its full model
    // membership, however many configured models share the pool.
    let mut physical: BTreeMap<(&str, &str, &str), &QuotaWindow> = BTreeMap::new();
    for model in &snapshot.models {
        for pool in &model.pools {
            let lane = lane_of(&pool.key, &account)?;
            for window in &pool.windows {
                // Shared pools repeat identical facts across models (the
                // validator proved it); only a latch carried into the models
                // it governs can differ, and that exhausted fact wins.
                let slot = physical
                    .entry((lane, window.source.as_str(), window.name.as_str()))
                    .or_insert(window);
                if window.remaining_percent == Some(0.0) && slot.remaining_percent != Some(0.0) {
                    *slot = window;
                }
            }
        }
    }
    // Membership is recorded per physical window, and only for the models
    // whose own view of that window is exactly the persisted fact: a narrower
    // window never inherits every model of its pool. An unsettled carried
    // exhaustion also retains its previous members when this round omits
    // some governed models. It is
    // keyed by the full physical identity (lane, source, window): the same
    // window name from another source (a carried latch after a source
    // switch) is a different fact with its own members.
    let mut pool_models: BTreeMap<(&str, &str, &str), BTreeSet<&str>> = BTreeMap::new();
    for model in &snapshot.models {
        for pool in &model.pools {
            let lane = lane_of(&pool.key, &account)?;
            for window in &pool.windows {
                let key = (lane, window.source.as_str(), window.name.as_str());
                if physical.get(&key).is_some_and(|chosen| *chosen == window) {
                    pool_models
                        .entry(key)
                        .or_default()
                        .insert(model.model.as_str());
                }
            }
        }
    }
    for ((lane, source, name), governed) in &membership {
        let identity = (lane.as_str(), source.as_str(), name.as_str());
        if released.contains(&(lane.clone(), source.clone(), name.clone()))
            || !physical
                .get(&identity)
                .is_some_and(|window| window.remaining_percent == Some(0.0))
        {
            continue;
        }
        if let Some(governed) = governed {
            pool_models
                .entry(identity)
                .or_default()
                .extend(governed.iter().map(String::as_str));
        }
    }
    let mut mutations = 0usize;
    for (identity @ (lane, source, name), window) in &physical {
        let old_key = (lane.to_string(), source.to_string(), name.to_string());
        // A disjoint nonzero report cannot settle this latch. Persisting it
        // as the newest row would erase the only membership that keeps the
        // exhausted model blocked; retain the older row instead.
        if window.remaining_percent != Some(0.0)
            && !released.contains(&old_key)
            && membership
                .get(&old_key)
                .and_then(Option::as_ref)
                .is_some_and(|old| {
                    !old.is_empty()
                        && pool_models.get(identity).is_some_and(|current| {
                            old.iter().all(|model| !current.contains(model.as_str()))
                        })
                })
        {
            continue;
        }
        window.validate()?;
        let quota_key = format!("{}::{}", account.as_str(), lane);
        let models = pool_models
            .get(identity)
            .map(|set| serde_json::json!({ "models": set.iter().collect::<Vec<_>>() }));
        tx.execute(
            "INSERT INTO capacity_samples(runtime,lane,window,target,source,remaining_percent,reset_at,observed_at,valid_until,payload_json,account_id,quota_key) VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
            params![
                runtime,
                lane,
                window.name,
                account.as_str(),
                window.source,
                window.remaining_percent,
                window.reset_at,
                window.observed_at,
                window.valid_until,
                models
                    .unwrap_or(serde_json::json!({ "models": [] }))
                    .to_string(),
                account.as_str(),
                quota_key,
            ],
        )?;
        mutations += 1;
    }

    // Release old latches before inserting this round's exhausted facts: a
    // new zero observed after a reset must remain latched.
    for (lane, source, name) in &released {
        let quota_key = format!("{}::{}", account.as_str(), lane);
        mutations += tx.execute(
            "DELETE FROM quota_exhaustion WHERE account_id=?1 AND quota_key=?2 AND source=?3 AND window_id=?4",
            params![account.as_str(), quota_key, source, name],
        )?;
    }

    // Durable latch maintenance: every currently exhausted physical window
    // upserts its fact, and sample retention never erases the latch. A
    // conflicting zero cannot shorten a shared window's reset horizon.
    for ((lane, source, name), window) in &physical {
        if window.remaining_percent != Some(0.0)
            || window.reset_at.is_some_and(|reset| reset <= at)
            || exhausted
                .get(&(lane.to_string(), source.to_string(), name.to_string()))
                .is_some_and(|fact| window.observed_at < fact.observed_at)
        {
            continue;
        }
        let quota_key = format!("{}::{}", account.as_str(), lane);
        tx.execute(
            "INSERT INTO quota_exhaustion(account_id,quota_key,source,window_id,observed_at,reset_at,collector_revision) \
             VALUES(?,?,?,?,?,?,NULL) \
             ON CONFLICT(account_id,quota_key,source,window_id) DO UPDATE SET \
             observed_at=excluded.observed_at,reset_at=MAX(quota_exhaustion.reset_at,excluded.reset_at)",
            params![
                account.as_str(),
                quota_key,
                *source,
                window.name,
                window.observed_at,
                window.reset_at,
            ],
        )?;
        mutations += 1;
    }
    let revision = if mutations > 0 {
        // Retention keeps, for every latched physical window, that exact
        // (key, source, window)'s newest row in the ranker's (observed_at,
        // id) order: it carries the membership the latch needs to keep
        // restricting exactly the models it governs, independently of other
        // sources' rows of the same window name and of insertion order.
        tx.execute(
            "DELETE FROM capacity_samples WHERE id NOT IN (SELECT id FROM capacity_samples ORDER BY observed_at DESC,id DESC LIMIT ?) \
             AND id NOT IN (SELECT id FROM (SELECT (SELECT s.id FROM capacity_samples s \
               WHERE s.quota_key=q.quota_key AND s.source=q.source AND s.window=q.window_id \
               ORDER BY s.observed_at DESC,s.id DESC LIMIT 1) AS id \
               FROM quota_exhaustion q) WHERE id IS NOT NULL)",
            [retention as i64],
        )?;
        advance_revision(&tx)?
    } else {
        revision(&tx)?
    };
    tx.commit()?;
    Ok(revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quota::{CollectorScope, normalize_collector_output};
    use serde_json::{Value, json};
    use std::os::unix::fs::PermissionsExt;

    /// Creates the private state directory required by the production store.
    fn private_dir() -> io::Result<tempfile::TempDir> {
        let dir = tempfile::tempdir()?;
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o700))?;
        Ok(dir)
    }

    /// Refuses special files and symlinks before SQLite can block or follow them.
    #[test]
    fn rejects_fifo_and_symlink_without_touching_target()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::{ffi::OsStrExt, fs::symlink};

        let dir = private_dir()?;
        let target = dir.path().join("quota.sqlite");
        drop(QuotaStore::open(&target)?);
        let before = std::fs::read(&target)?;
        let link = dir.path().join("link.sqlite");
        symlink(&target, &link)?;
        assert!(matches!(
            QuotaStore::open(&link),
            Err(StoreError::ForeignDatabase)
        ));
        assert_eq!(std::fs::read(&target)?, before);

        let fifo = dir.path().join("fifo.sqlite");
        let name = std::ffi::CString::new(fifo.as_os_str().as_bytes())?;
        // SAFETY: name is a valid NUL-terminated path under this test's private directory.
        if unsafe { libc::mkfifo(name.as_ptr(), 0o600) } != 0 {
            return Err(io::Error::last_os_error().into());
        }
        assert!(matches!(
            QuotaStore::open(&fifo),
            Err(StoreError::ForeignDatabase)
        ));
        Ok(())
    }

    /// Creates a normalized synthetic observation from one collector source.
    fn snapshot(
        account: &AccountId,
        source: &str,
        windows: Value,
    ) -> Result<NormalizedQuotaSnapshot> {
        let scope = CollectorScope {
            runtime: "runtime".into(),
            source: source.into(),
            models: BTreeSet::from(["a".into(), "b".into(), "c".into()]),
        };
        Ok(normalize_collector_output(
            account,
            &scope,
            &json!({"version":1,"windows":windows}),
            10_000.0,
            256,
            256,
        )?)
    }

    /// Returns one synthetic window with optional remaining and reset.
    fn window(
        models: &[&str],
        remaining: Option<f64>,
        observed: f64,
        valid_until: f64,
        reset: Option<f64>,
    ) -> Value {
        json!({"pool":"shared","window":"hour","models":models,"remaining_percent":remaining,
            "observed_at":observed,"valid_until":valid_until,"reset_at":reset})
    }

    #[test]
    fn shared_membership_latch_requires_covering_fresh_evidence()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = private_dir()?;
        let account: AccountId = "acct".parse()?;
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite"))?;
        store.register_account(&account)?;
        let exhausted = snapshot(
            &account,
            "s1",
            json!([window(&["a", "b"], Some(0.0), 1000.0, 2000.0, Some(3000.0))]),
        )?;
        assert_eq!(store.record_snapshot("runtime", &exhausted, 20, 1000.0)?, 2);
        let first = store.read_account(&account)?;
        assert_eq!(first.windows.len(), 1);
        assert_eq!(
            first.windows[0].models,
            Some(BTreeSet::from(["a".into(), "b".into()]))
        );
        assert_eq!(first.latches.len(), 1);
        let unknown = snapshot(
            &account,
            "s2",
            json!([window(&["a"], None, 1100.0, 2000.0, None)]),
        )?;
        store.record_snapshot("runtime", &unknown, 20, 1100.0)?;
        let partial = store.read_account(&account)?;
        assert_eq!(partial.latches.len(), 1);
        assert_eq!(
            partial.latches[0].models,
            Some(BTreeSet::from(["a".into(), "b".into()]))
        );
        let stale = snapshot(
            &account,
            "s1",
            json!([window(&["a", "b"], Some(50.0), 1200.0, 1201.0, None)]),
        )?;
        store.record_snapshot("runtime", &stale, 20, 1300.0)?;
        assert_eq!(store.read_account(&account)?.latches.len(), 1);
        let disjoint = snapshot(
            &account,
            "s1",
            json!([window(&["c"], Some(70.0), 1400.0, 3000.0, None)]),
        )?;
        let before = store.read_account(&account)?;
        assert_eq!(
            store.record_snapshot("runtime", &disjoint, 20, 1400.0)?,
            before.revision
        );
        assert_eq!(store.read_account(&account)?, before);
        let incomplete = snapshot(
            &account,
            "s2",
            json!([window(&["a"], Some(80.0), 1500.0, 3000.0, None)]),
        )?;
        store.record_snapshot("runtime", &incomplete, 20, 1500.0)?;
        assert_eq!(store.read_account(&account)?.latches.len(), 1);
        let fresh = snapshot(
            &account,
            "s2",
            json!([window(&["a", "b"], Some(90.0), 1600.0, 3000.0, None)]),
        )?;
        store.record_snapshot("runtime", &fresh, 20, 1600.0)?;
        assert!(store.read_account(&account)?.latches.is_empty());
        let exhausted_again = snapshot(
            &account,
            "s1",
            json!([window(&["a"], Some(0.0), 1700.0, 1800.0, Some(1800.0))]),
        )?;
        store.record_snapshot("runtime", &exhausted_again, 20, 1700.0)?;
        let empty = snapshot(&account, "s1", json!([]))?;
        store.record_snapshot("runtime", &empty, 20, 1800.0)?;
        assert!(store.read_account(&account)?.latches.is_empty());
        Ok(())
    }

    #[test]
    fn newer_zero_replaces_latch_and_disjoint_zero_preserves_reset()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = private_dir()?;
        let account: AccountId = "acct".parse()?;
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite"))?;
        store.register_account(&account)?;
        let first = snapshot(
            &account,
            "s1",
            json!([window(&["a"], Some(0.0), 100.0, 2000.0, Some(2000.0))]),
        )?;
        store.record_snapshot("runtime", &first, 20, 100.0)?;
        let disjoint = snapshot(
            &account,
            "s1",
            json!([window(&["b"], Some(0.0), 200.0, 1500.0, Some(1500.0))]),
        )?;
        store.record_snapshot("runtime", &disjoint, 20, 200.0)?;
        let held = store.read_account(&account)?;
        assert_eq!(held.latches[0].reset_at, Some(2000.0));
        assert_eq!(
            held.latches[0].models,
            Some(BTreeSet::from(["a".into(), "b".into()]))
        );
        let newer = snapshot(
            &account,
            "s2",
            json!([window(&["a", "b"], Some(0.0), 300.0, 3000.0, Some(3000.0))]),
        )?;
        store.record_snapshot("runtime", &newer, 20, 300.0)?;
        let replaced = store.read_account(&account)?;
        assert_eq!(replaced.latches.len(), 1);
        assert_eq!(replaced.latches[0].source, "s2");
        assert_eq!(replaced.latches[0].reset_at, Some(3000.0));
        Ok(())
    }

    #[test]
    fn retention_separates_accounts_and_keeps_latest_latch_membership()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = private_dir()?;
        let a: AccountId = "account-a".parse()?;
        let b: AccountId = "account-b".parse()?;
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite"))?;
        store.register_account(&a)?;
        store.register_account(&a)?;
        store.register_account(&b)?;
        assert_eq!(store.read_account(&a)?.revision, 2);
        let first = snapshot(
            &a,
            "s1",
            json!([window(&["a"], Some(0.0), 100.0, 2000.0, None)]),
        )?;
        store.record_snapshot("one", &first, 1, 100.0)?;
        let second = snapshot(
            &b,
            "s1",
            json!([window(&["b"], Some(80.0), 200.0, 2000.0, None)]),
        )?;
        store.record_snapshot("two", &second, 1, 200.0)?;
        let a_history = store.read_account(&a)?;
        let b_history = store.read_account(&b)?;
        assert_eq!(a_history.windows.len(), 1);
        assert_eq!(
            a_history.windows[0].models,
            Some(BTreeSet::from(["a".into()]))
        );
        assert_eq!(a_history.latches.len(), 1);
        assert_eq!(b_history.windows.len(), 1);
        assert_eq!(
            b_history.windows[0].models,
            Some(BTreeSet::from(["b".into()]))
        );
        assert!(b_history.latches.is_empty());
        assert_eq!(a_history.revision, b_history.revision);
        let empty = snapshot(&b, "s1", json!([]))?;
        assert_eq!(
            store.record_snapshot("two", &empty, 1, 300.0)?,
            b_history.revision
        );
        assert_eq!(store.read_account(&b)?, b_history);
        Ok(())
    }

    #[test]
    fn failure_rolls_back_rows_latches_and_revision()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = private_dir()?;
        let a: AccountId = "acct".parse()?;
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite"))?;
        store.register_account(&a)?;
        let snapshot = snapshot(
            &a,
            "s1",
            json!([window(&["a"], Some(0.0), 100.0, 1000.0, None)]),
        )?;
        store.conn.execute_batch("CREATE TRIGGER reject_latch BEFORE INSERT ON quota_exhaustion BEGIN SELECT RAISE(ABORT,'fixture failure'); END;")?;
        let before = store.read_account(&a)?;
        assert!(
            store
                .record_snapshot("runtime", &snapshot, 20, 100.0)
                .is_err()
        );
        assert_eq!(store.read_account(&a)?, before);
        store.conn.execute_batch("DROP TRIGGER reject_latch")?;
        store.record_snapshot("runtime", &snapshot, 20, 100.0)?;
        Ok(())
    }

    #[test]
    fn missing_membership_differs_from_explicit_empty_membership()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = private_dir()?;
        let a: AccountId = "acct".parse()?;
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite"))?;
        store.register_account(&a)?;
        store.conn.execute("INSERT INTO capacity_samples(runtime,lane,window,source,observed_at,valid_until,payload_json,account_id,quota_key) VALUES('r','lane','old','s',1,2,'{}','acct','acct::lane')", [])?;
        store.conn.execute("INSERT INTO capacity_samples(runtime,lane,window,source,observed_at,valid_until,payload_json,account_id,quota_key) VALUES('r','lane','empty','s',1,2,'{\"models\":[]}','acct','acct::lane')", [])?;
        let rows = store.read_account(&a)?.windows;
        assert_eq!(rows[0].window, "empty");
        assert_eq!(rows[0].models, Some(BTreeSet::new()));
        assert_eq!(rows[1].window, "old");
        assert_eq!(rows[1].models, None);
        Ok(())
    }

    /// Legacy samples may have absent time bounds; the reader preserves both absences.
    #[test]
    fn legacy_missing_time_bounds_are_preserved()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = private_dir()?;
        let mut store = QuotaStore::open(&dir.path().join("quota.sqlite"))?;
        let account: AccountId = "acct".parse()?;
        store.register_account(&account)?;
        store.conn.execute("INSERT INTO capacity_samples(runtime,lane,window,source,observed_at,valid_until,payload_json,account_id,quota_key) VALUES('r','lane','legacy','s',NULL,NULL,'{}','acct','acct::lane')", [])?;
        let view = store.read_account(&account)?;
        assert_eq!(view.windows[0].observed_at, None);
        assert_eq!(view.windows[0].valid_until, None);
        Ok(())
    }

    #[test]
    fn rejects_foreign_and_newer_database_without_changing_either()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let dir = private_dir()?;
        let foreign = dir.path().join("foreign.sqlite");
        {
            let conn = Connection::open(&foreign)?;
            conn.execute_batch(
                "CREATE TABLE unrelated(value TEXT); INSERT INTO unrelated VALUES('keep');",
            )?;
        }
        let before = std::fs::read(&foreign)?;
        assert!(matches!(
            QuotaStore::open(&foreign),
            Err(StoreError::ForeignDatabase)
        ));
        assert_eq!(std::fs::read(&foreign)?, before);
        let newer = dir.path().join("newer.sqlite");
        drop(QuotaStore::open(&newer)?);
        {
            let conn = Connection::open(&newer)?;
            conn.pragma_update(None, "user_version", 2)?;
        }
        let before = std::fs::read(&newer)?;
        assert!(matches!(
            QuotaStore::open(&newer),
            Err(StoreError::UnsupportedVersion(2))
        ));
        assert_eq!(std::fs::read(&newer)?, before);
        Ok(())
    }
}
