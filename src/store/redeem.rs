//! Persistence of owner-requested reset-credit redemptions and their latest state.
//!
//! Every transition runs in one immediate transaction. A request is handed out at
//! most once, under a random claim token, and its first reported verdict is final.
//! Nothing here retries a provider call or infers an outcome from later inventories.

use super::{QuotaStore, Result, invalid};
use crate::quota::{AccountId, ResetCredits};
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use serde::Serialize;

/// A pending request that no launcher claimed within this time was never executed.
pub const CLAIM_TTL_SECONDS: f64 = 600.0;
/// A claimed request without a report after this time has an unknown provider outcome.
pub const REPORT_TTL_SECONDS: f64 = 900.0;

/// Latest state of one redemption request, as returned to clients.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RedeemRecord {
    pub request_id: String,
    pub account: String,
    pub credit_id: String,
    /// `pending`, `redeemed`, `rejected`, `unknown` or `failed`.
    pub state: String,
    /// Fixed lowercase code, or null.
    pub reason: Option<String>,
    /// Unix seconds.
    pub created_at: i64,
    /// Unix seconds of the latest state change.
    pub updated_at: i64,
}

/// A claimed request with the inventory facts the provider call needs.
#[derive(Debug, Clone, PartialEq)]
pub struct RedeemClaim {
    pub request_id: String,
    pub account: String,
    pub credit_id: String,
    pub scope: String,
    pub provider_kind: String,
    /// Random per-claim secret between service and launcher; never shown to clients.
    pub claim_token: String,
    /// Configuration revision the request was admitted under.
    pub config_revision: String,
}

/// Outcome of a store operation that can be refused without a storage failure.
#[derive(Debug)]
pub enum RedeemError {
    /// The request id is already bound to other arguments, or the transition is not allowed.
    Conflict,
    /// No such request.
    Unknown,
    /// SQLite or data failure.
    Store,
}

impl From<super::StoreError> for RedeemError {
    /// Hides storage details behind one fixed failure.
    fn from(_: super::StoreError) -> Self {
        Self::Store
    }
}

impl From<rusqlite::Error> for RedeemError {
    /// Hides SQLite details behind one fixed failure.
    fn from(_: rusqlite::Error) -> Self {
        Self::Store
    }
}

const COLUMNS: &str = "request_id,account_id,credit_id,state,reason,created_at,updated_at";

/// Maps one selected row in [`COLUMNS`] order.
fn record(row: &rusqlite::Row<'_>) -> rusqlite::Result<RedeemRecord> {
    Ok(RedeemRecord {
        request_id: row.get(0)?,
        account: row.get(1)?,
        credit_id: row.get(2)?,
        state: row.get(3)?,
        reason: row.get(4)?,
        created_at: row.get::<_, f64>(5)? as i64,
        updated_at: row.get::<_, f64>(6)? as i64,
    })
}

/// Reads one request by id.
fn find(tx: &Transaction<'_>, request_id: &str) -> Result<Option<RedeemRecord>> {
    Ok(tx
        .query_row(
            &format!("SELECT {COLUMNS} FROM quota_redeem_requests WHERE request_id=?"),
            [request_id],
            record,
        )
        .optional()?)
}

/// Ends requests whose launcher never arrived or never reported. Rows are never
/// deleted: each is the permanent tombstone of its request id, so an old id can be
/// replayed forever without becoming a new request.
fn expire(tx: &Transaction<'_>, at: f64) -> Result<()> {
    tx.execute(
        "UPDATE quota_redeem_requests SET state='failed',reason='executor_unavailable',updated_at=?1 \
         WHERE state='pending' AND claimed_at IS NULL AND created_at < ?2",
        params![at, at - CLAIM_TTL_SECONDS],
    )?;
    tx.execute(
        "UPDATE quota_redeem_requests SET state='unknown',reason='outcome_unreported',updated_at=?1 \
         WHERE state='pending' AND claimed_at IS NOT NULL AND claimed_at < ?2",
        params![at, at - REPORT_TTL_SECONDS],
    )?;
    Ok(())
}

/// Reads the account's latest stored inventory and its observation time.
fn inventory(tx: &Transaction<'_>, account: &str) -> Result<Option<(f64, ResetCredits)>> {
    let stored: Option<(f64, String)> = tx
        .query_row(
            "SELECT observed_at,inventory_json FROM quota_reset_credits WHERE account_id=?",
            [account],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    stored
        .map(|(observed_at, json)| {
            serde_json::from_str(&json)
                .map(|inventory| (observed_at, inventory))
                .map_err(|_| invalid("invalid stored reset credit inventory"))
        })
        .transpose()
}

/// Decides whether `credit_id` may be spent now, against the latest stored inventory.
///
/// Used both when a request is admitted and again when it is handed to a launcher.
/// Another request of the account that was redeemed, ended unknown or is claimed after
/// the inventory was observed makes that inventory stale. `this` is the request being
/// decided and is not counted against itself.
fn refusal(
    tx: &Transaction<'_>,
    account: &str,
    credit_id: &str,
    this: &str,
) -> Result<Option<&'static str>> {
    let last_spend: Option<f64> = tx.query_row(
        "SELECT max(CASE WHEN state='pending' THEN claimed_at ELSE updated_at END) \
         FROM quota_redeem_requests WHERE account_id=?1 AND request_id!=?2 AND \
         (state IN ('redeemed','unknown') OR (state='pending' AND claimed_at IS NOT NULL))",
        params![account, this],
        |row| row.get(0),
    )?;
    Ok(match inventory(tx, account)? {
        None => Some("inventory_unknown"),
        Some((_, credits)) if !credits.known => Some("inventory_unknown"),
        // A spend newer than the inventory means the listed credit may be gone.
        Some((observed_at, _)) if last_spend.is_some_and(|spent| spent > observed_at) => {
            Some("inventory_stale")
        }
        Some((_, credits))
            if credits
                .credits
                .iter()
                .find(|credit| credit.id == credit_id)
                .is_none_or(|credit| credit.usable_now != Some(true)) =>
        {
            Some("credit_not_usable")
        }
        Some(_) => None,
    })
}

/// Returns 128 random bits as lowercase hex.
fn claim_token() -> Result<String> {
    use std::io::Read;
    let mut random = [0u8; 16];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut random)?;
    Ok(random.iter().map(|byte| format!("{byte:02x}")).collect())
}

impl QuotaStore {
    /// Creates the additive request table; older binaries ignore it.
    pub(super) fn redeem_schema(tx: &Transaction<'_>) -> Result<()> {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS quota_redeem_requests (\
             request_id TEXT PRIMARY KEY NOT NULL,\
             account_id TEXT NOT NULL REFERENCES provider_accounts(account_id),\
             credit_id TEXT NOT NULL, state TEXT NOT NULL, reason TEXT,\
             created_at REAL NOT NULL, updated_at REAL NOT NULL, claimed_at REAL,\
             claim_token TEXT, config_revision TEXT, instance TEXT);\
             CREATE INDEX IF NOT EXISTS quota_redeem_requests_account ON quota_redeem_requests(account_id,state);",
        )?;
        Ok(())
    }

    /// Records a redemption request once per `request_id` and decides it against the
    /// account's latest stored inventory.
    ///
    /// A repeated id with the same arguments returns the existing record; other
    /// arguments are a conflict, permanently. `binding` is the configuration revision
    /// and launcher instance the request is pinned to. A new request is stored as `rejected` when
    /// `refusal` is given, another request for the account is pending, an earlier
    /// redemption is newer than the inventory, or the credit is not listed as
    /// usable now. Otherwise it is stored as `pending`.
    pub fn redeem_request(
        &mut self,
        account: &AccountId,
        credit_id: &str,
        request_id: &str,
        refused: Option<&str>,
        binding: Option<(&str, &str)>,
        at: f64,
    ) -> std::result::Result<RedeemRecord, RedeemError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, at)?;
        if let Some(existing) = find(&tx, request_id)? {
            tx.commit()?;
            return if existing.account == account.as_str() && existing.credit_id == credit_id {
                Ok(existing)
            } else {
                Err(RedeemError::Conflict)
            };
        }
        let pending: i64 = tx.query_row(
            "SELECT count(*) FROM quota_redeem_requests WHERE account_id=? AND state='pending'",
            [account.as_str()],
            |row| row.get(0),
        )?;
        let refused = if let Some(code) = refused {
            Some(code)
        } else if pending != 0 {
            Some("redeem_in_progress")
        } else {
            refusal(&tx, account.as_str(), credit_id, request_id)?
        };
        tx.execute(
            "INSERT INTO quota_redeem_requests(request_id,account_id,credit_id,state,reason,created_at,updated_at,config_revision,instance) \
             VALUES(?,?,?,?,?,?,?,?,?)",
            params![
                request_id,
                account.as_str(),
                credit_id,
                if refused.is_some() { "rejected" } else { "pending" },
                refused,
                at,
                at,
                binding.map(|(revision, _)| revision),
                binding.map(|(_, instance)| instance)
            ],
        )?;
        let stored = find(&tx, request_id)?.ok_or(RedeemError::Store)?;
        tx.commit()?;
        Ok(stored)
    }

    /// Returns the latest state of one request after applying time limits.
    pub fn redeem_status(
        &mut self,
        request_id: &str,
        at: f64,
    ) -> std::result::Result<RedeemRecord, RedeemError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, at)?;
        let found = find(&tx, request_id)?;
        tx.commit()?;
        found.ok_or(RedeemError::Unknown)
    }

    /// Applies the time limits without reading a request.
    pub fn redeem_expire(&mut self, at: f64) -> std::result::Result<(), RedeemError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, at)?;
        tx.commit()?;
        Ok(())
    }

    /// Hands the account's pending request to one launcher, at most once, under a
    /// fresh random claim token that every later report must repeat.
    ///
    /// A request pinned to another configuration revision ends as
    /// `failed/binding_changed`. The admission check is repeated: a request whose credit is no longer
    /// usable, or whose inventory another redemption has overtaken, is stored as
    /// `rejected` and not handed out.
    pub fn redeem_claim(
        &mut self,
        account: &AccountId,
        revision: &str,
        at: f64,
    ) -> std::result::Result<Option<RedeemClaim>, RedeemError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, at)?;
        let waiting: Option<(String, String, Option<String>)> = tx
            .query_row(
                "SELECT request_id,credit_id,config_revision FROM quota_redeem_requests \
                 WHERE account_id=? AND state='pending' AND claimed_at IS NULL ORDER BY created_at LIMIT 1",
                [account.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        let Some((request_id, credit_id, pinned)) = waiting else {
            tx.commit()?;
            return Ok(None);
        };
        // The request was admitted for one exact configuration; any change since then
        // could point it at another credential, script or launcher.
        if pinned.as_deref() != Some(revision) {
            tx.execute(
                "UPDATE quota_redeem_requests SET state='failed',reason='binding_changed',updated_at=? WHERE request_id=?",
                params![at, request_id],
            )?;
            tx.commit()?;
            return Ok(None);
        }
        if let Some(code) = refusal(&tx, account.as_str(), &credit_id, &request_id)? {
            tx.execute(
                "UPDATE quota_redeem_requests SET state='rejected',reason=?,updated_at=? WHERE request_id=?",
                params![code, at, request_id],
            )?;
            tx.commit()?;
            return Ok(None);
        }
        let credit = inventory(&tx, account.as_str())?.and_then(|(_, credits)| {
            credits
                .credits
                .into_iter()
                .find(|credit| credit.id == credit_id)
        });
        let credit = credit.ok_or(RedeemError::Store)?;
        let token = claim_token()?;
        tx.execute(
            "UPDATE quota_redeem_requests SET claimed_at=?,claim_token=? WHERE request_id=?",
            params![at, token, request_id],
        )?;
        tx.commit()?;
        Ok(Some(RedeemClaim {
            request_id,
            account: account.as_str().to_owned(),
            credit_id,
            scope: credit.scope,
            provider_kind: credit.provider_kind,
            claim_token: token,
            config_revision: revision.to_owned(),
        }))
    }

    /// Records the executor's verdict for exactly the claim that carries `token`.
    ///
    /// The first verdict wins: a repeated identical report is a no-op and a different
    /// one is a conflict. The only replacement allowed is the executor's own verdict
    /// arriving after the request timed out as `unknown/outcome_unreported`.
    pub fn redeem_report(
        &mut self,
        account: &AccountId,
        request_id: &str,
        token: &str,
        state: &str,
        reason: Option<&str>,
        at: f64,
    ) -> std::result::Result<RedeemRecord, RedeemError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        expire(&tx, at)?;
        let existing = find(&tx, request_id)?.ok_or(RedeemError::Unknown)?;
        let stored_token: Option<String> = tx.query_row(
            "SELECT claim_token FROM quota_redeem_requests WHERE request_id=?",
            [request_id],
            |row| row.get(0),
        )?;
        if existing.account != account.as_str() || stored_token.as_deref() != Some(token) {
            return Err(RedeemError::Conflict);
        }
        if existing.state == state && existing.reason.as_deref() == reason {
            tx.commit()?;
            return Ok(existing);
        }
        let changed = tx.execute(
            "UPDATE quota_redeem_requests SET state=?,reason=?,updated_at=? WHERE request_id=? AND claim_token=? AND \
             ((state='pending' AND claimed_at IS NOT NULL) OR (state='unknown' AND reason='outcome_unreported'))",
            params![state, reason, at, request_id, token],
        )?;
        if changed != 1 {
            return Err(RedeemError::Conflict);
        }
        let stored = find(&tx, request_id)?.ok_or(RedeemError::Store)?;
        tx.commit()?;
        Ok(stored)
    }

    /// Lists every launcher instance a request was ever pinned to, with whether one of
    /// its requests still waits for a launcher.
    pub fn redeem_instances(&self) -> std::result::Result<Vec<(String, bool)>, RedeemError> {
        let mut statement = self.conn.prepare(
            "SELECT instance,max(state='pending' AND claimed_at IS NULL) FROM quota_redeem_requests \
             WHERE instance IS NOT NULL GROUP BY instance",
        )?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)))?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    /// True while the account has a pending request no launcher has taken yet.
    #[cfg(test)]
    pub fn redeem_waiting(&self, account: &AccountId) -> std::result::Result<bool, RedeemError> {
        let count: i64 = self.conn.query_row(
            "SELECT count(*) FROM quota_redeem_requests \
             WHERE account_id=? AND state='pending' AND claimed_at IS NULL",
            [account.as_str()],
            |row| row.get(0),
        )?;
        Ok(count != 0)
    }

    /// Ends a still-pending request that the service could not hand to a launcher.
    pub fn redeem_abort(
        &mut self,
        request_id: &str,
        reason: &str,
        at: f64,
    ) -> std::result::Result<RedeemRecord, RedeemError> {
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "UPDATE quota_redeem_requests SET state='failed',reason=?,updated_at=? \
             WHERE request_id=? AND state='pending' AND claimed_at IS NULL",
            params![reason, at, request_id],
        )?;
        let stored = find(&tx, request_id)?.ok_or(RedeemError::Unknown)?;
        tx.commit()?;
        Ok(stored)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, reason = "Synthetic store assertions")]
mod tests {
    use super::*;
    use crate::quota::{NormalizedQuotaSnapshot, ResetCredit, ResetCreditSnapshot};
    use std::os::unix::fs::PermissionsExt;
    use std::str::FromStr;

    const ONE: &str = "11111111-1111-4111-8111-111111111111";
    const TWO: &str = "22222222-2222-4222-8222-222222222222";
    const THREE: &str = "33333333-3333-4333-8333-333333333333";
    const PIN: Option<(&str, &str)> = Some(("rev-1", "fixture-instance"));

    /// Opens a private store with one registered account.
    fn fixture() -> (tempfile::TempDir, QuotaStore, AccountId) {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut store = QuotaStore::open(&root.path().join("quota.sqlite")).unwrap();
        let account = AccountId::from_str("fixture").unwrap();
        store.register_account(&account).unwrap();
        (root, store, account)
    }

    /// Stores an inventory observation with the given credits.
    fn observe(store: &mut QuotaStore, account: &AccountId, at: f64, credits: &[(&str, bool)]) {
        let snapshot = NormalizedQuotaSnapshot {
            account: account.clone(),
            models: Vec::new(),
            reset_credits: ResetCreditSnapshot {
                observed_at: Some(at),
                inventory: ResetCredits {
                    known: true,
                    available_count: None,
                    credits: credits
                        .iter()
                        .map(|(id, usable)| ResetCredit {
                            id: (*id).into(),
                            provider_kind: "FIVE_HOUR".into(),
                            label: "Fixture".into(),
                            scope: "five_hour".into(),
                            resets_left: None,
                            usable_now: Some(*usable),
                            granted_at: None,
                            expires_at: None,
                        })
                        .collect(),
                },
            },
        };
        store.record_snapshot("fixture", &snapshot, 10, at).unwrap();
    }

    /// Returns state and reason of a request.
    fn state(store: &mut QuotaStore, id: &str, at: f64) -> (String, Option<String>) {
        let record = store.redeem_status(id, at).unwrap();
        (record.state, record.reason)
    }

    #[test]
    fn request_is_idempotent_and_refuses_unusable_or_concurrent_credits() {
        let (_root, mut store, account) = fixture();
        let unknown = store
            .redeem_request(&account, "credit-a", ONE, None, PIN, 10.0)
            .unwrap();
        assert_eq!(
            (unknown.state.as_str(), unknown.reason.as_deref()),
            ("rejected", Some("inventory_unknown"))
        );
        observe(
            &mut store,
            &account,
            20.0,
            &[("credit-a", true), ("credit-b", false)],
        );
        // The rejected record is final for its id even after the inventory improved.
        assert_eq!(
            store
                .redeem_request(&account, "credit-a", ONE, None, PIN, 21.0)
                .unwrap(),
            unknown
        );
        assert!(matches!(
            store.redeem_request(&account, "credit-b", ONE, None, PIN, 21.0),
            Err(RedeemError::Conflict)
        ));
        let unusable = store
            .redeem_request(&account, "credit-b", TWO, None, PIN, 22.0)
            .unwrap();
        assert_eq!(unusable.reason.as_deref(), Some("credit_not_usable"));
        let pending = store
            .redeem_request(&account, "credit-a", THREE, None, PIN, 23.0)
            .unwrap();
        assert_eq!(
            (pending.state.as_str(), pending.reason.clone()),
            ("pending", None)
        );
        assert_eq!((pending.created_at, pending.updated_at), (23, 23));
        assert_eq!(
            store
                .redeem_request(&account, "credit-a", THREE, None, PIN, 24.0)
                .unwrap(),
            pending
        );
        let fourth = "44444444-4444-4444-8444-444444444444";
        let busy = store
            .redeem_request(&account, "credit-a", fourth, None, PIN, 25.0)
            .unwrap();
        assert_eq!(busy.reason.as_deref(), Some("redeem_in_progress"));
        assert_eq!(store.redeem_status(fourth, 26.0).unwrap(), busy);
        assert!(matches!(
            store.redeem_status("missing", 26.0),
            Err(RedeemError::Unknown)
        ));
    }

    #[test]
    fn claim_is_single_use_and_only_its_token_can_report_once() {
        let (_root, mut store, account) = fixture();
        observe(&mut store, &account, 20.0, &[("credit-a", true)]);
        assert!(
            store
                .redeem_claim(&account, "rev-1", 21.0)
                .unwrap()
                .is_none()
        );
        store
            .redeem_request(&account, "credit-a", ONE, None, PIN, 22.0)
            .unwrap();
        assert!(store.redeem_waiting(&account).unwrap());
        // An unclaimed request has no token, so nothing can report it.
        for token in ["", "00000000000000000000000000000000"] {
            assert!(matches!(
                store.redeem_report(&account, ONE, token, "redeemed", None, 22.0),
                Err(RedeemError::Conflict)
            ));
        }
        let claim = store
            .redeem_claim(&account, "rev-1", 23.0)
            .unwrap()
            .unwrap();
        assert_eq!(
            (
                claim.request_id.as_str(),
                claim.scope.as_str(),
                claim.provider_kind.as_str(),
                claim.claim_token.len()
            ),
            (ONE, "five_hour", "FIVE_HOUR", 32)
        );
        assert!(!store.redeem_waiting(&account).unwrap());
        assert!(
            store
                .redeem_claim(&account, "rev-1", 24.0)
                .unwrap()
                .is_none()
        );
        // A wrong token, another account or another request id is refused.
        let other = AccountId::from_str("other").unwrap();
        store.register_account(&other).unwrap();
        assert!(matches!(
            store.redeem_report(
                &account,
                ONE,
                "ffffffffffffffffffffffffffffffff",
                "redeemed",
                None,
                25.0
            ),
            Err(RedeemError::Conflict)
        ));
        assert!(matches!(
            store.redeem_report(&other, ONE, &claim.claim_token, "redeemed", None, 25.0),
            Err(RedeemError::Conflict)
        ));
        assert!(matches!(
            store.redeem_report(&account, TWO, &claim.claim_token, "redeemed", None, 25.0),
            Err(RedeemError::Unknown)
        ));
        assert_eq!(state(&mut store, ONE, 25.0).0, "pending");
        let done = store
            .redeem_report(&account, ONE, &claim.claim_token, "redeemed", None, 25.0)
            .unwrap();
        assert_eq!((done.state.as_str(), done.updated_at), ("redeemed", 25));
        // The same verdict again is a no-op; the first verdict wins over any other.
        assert_eq!(
            store
                .redeem_report(&account, ONE, &claim.claim_token, "redeemed", None, 26.0)
                .unwrap(),
            done
        );
        for (verdict, reason) in [
            ("failed", None),
            ("unknown", Some("provider_timeout")),
            ("redeemed", Some("already_redeemed")),
        ] {
            assert!(matches!(
                store.redeem_report(&account, ONE, &claim.claim_token, verdict, reason, 26.0),
                Err(RedeemError::Conflict)
            ));
        }
        assert_eq!(state(&mut store, ONE, 27.0), ("redeemed".into(), None));
    }

    #[test]
    fn stale_inventory_blocks_admission_and_hand_out() {
        let (_root, mut store, account) = fixture();
        observe(
            &mut store,
            &account,
            20.0,
            &[("credit-a", true), ("credit-b", true)],
        );
        store
            .redeem_request(&account, "credit-a", ONE, None, PIN, 30.0)
            .unwrap();
        let claim = store
            .redeem_claim(&account, "rev-1", 31.0)
            .unwrap()
            .unwrap();
        store
            .redeem_report(
                &account,
                ONE,
                &claim.claim_token,
                "unknown",
                Some("provider_timeout"),
                32.0,
            )
            .unwrap();
        // The inventory predates the spend, so no credit of the account is offered.
        let stale = store
            .redeem_request(&account, "credit-b", TWO, None, PIN, 33.0)
            .unwrap();
        assert_eq!(stale.reason.as_deref(), Some("inventory_stale"));
        // Later inventories never change an unknown outcome, whatever they list.
        observe(&mut store, &account, 40.0, &[("credit-b", true)]);
        observe(
            &mut store,
            &account,
            41.0,
            &[("credit-a", false), ("credit-b", true)],
        );
        assert_eq!(
            state(&mut store, ONE, 42.0),
            ("unknown".into(), Some("provider_timeout".into()))
        );

        // Hand-out repeats the admission check: the credit vanished after admission.
        store
            .redeem_request(&account, "credit-b", THREE, None, PIN, 43.0)
            .unwrap();
        observe(&mut store, &account, 44.0, &[("credit-a", true)]);
        assert!(
            store
                .redeem_claim(&account, "rev-1", 45.0)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state(&mut store, THREE, 46.0),
            ("rejected".into(), Some("credit_not_usable".into()))
        );
        assert!(!store.redeem_waiting(&account).unwrap());
    }

    #[test]
    fn hand_out_is_refused_when_another_redemption_overtook_the_inventory() {
        let (_root, mut store, account) = fixture();
        observe(&mut store, &account, 20.0, &[("credit-a", true)]);
        store
            .redeem_request(&account, "credit-a", ONE, None, PIN, 30.0)
            .unwrap();
        let first = store
            .redeem_claim(&account, "rev-1", 31.0)
            .unwrap()
            .unwrap();
        // The first request times out unreported, which frees the account for a new one
        // only after a fresh inventory.
        let later = 31.0 + REPORT_TTL_SECONDS + 1.0;
        assert_eq!(
            state(&mut store, ONE, later),
            ("unknown".into(), Some("outcome_unreported".into()))
        );
        observe(&mut store, &account, later + 1.0, &[("credit-a", true)]);
        store
            .redeem_request(&account, "credit-a", TWO, None, PIN, later + 2.0)
            .unwrap();
        assert!(store.redeem_waiting(&account).unwrap());
        // The executor's real verdict arrives late and replaces the timeout, once.
        let real = store
            .redeem_report(
                &account,
                ONE,
                &first.claim_token,
                "redeemed",
                None,
                later + 3.0,
            )
            .unwrap();
        assert_eq!(real.state, "redeemed");
        assert!(matches!(
            store.redeem_report(
                &account,
                ONE,
                &first.claim_token,
                "failed",
                None,
                later + 4.0
            ),
            Err(RedeemError::Conflict)
        ));
        // That spend is newer than the inventory the second request was admitted on.
        assert!(
            store
                .redeem_claim(&account, "rev-1", later + 5.0)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state(&mut store, TWO, later + 6.0),
            ("rejected".into(), Some("inventory_stale".into()))
        );
    }

    #[test]
    fn time_limits_end_unclaimed_and_unreported_requests() {
        let (_root, mut store, account) = fixture();
        observe(&mut store, &account, 20.0, &[("credit-a", true)]);
        store
            .redeem_request(&account, "credit-a", ONE, None, PIN, 100.0)
            .unwrap();
        let late = 100.0 + CLAIM_TTL_SECONDS + 1.0;
        store.redeem_expire(late).unwrap();
        assert!(!store.redeem_waiting(&account).unwrap());
        assert!(
            store
                .redeem_claim(&account, "rev-1", late)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state(&mut store, ONE, late),
            ("failed".into(), Some("executor_unavailable".into()))
        );
        store
            .redeem_request(&account, "credit-a", TWO, None, PIN, late)
            .unwrap();
        let claim = store
            .redeem_claim(&account, "rev-1", late)
            .unwrap()
            .unwrap();
        let later = late + REPORT_TTL_SECONDS + 1.0;
        assert_eq!(
            state(&mut store, TWO, later),
            ("unknown".into(), Some("outcome_unreported".into()))
        );
        // Only the matching token can still settle it.
        assert!(matches!(
            store.redeem_report(
                &account,
                TWO,
                "ffffffffffffffffffffffffffffffff",
                "rejected",
                Some("already_used"),
                later
            ),
            Err(RedeemError::Conflict)
        ));
        let settled = store
            .redeem_report(
                &account,
                TWO,
                &claim.claim_token,
                "rejected",
                Some("already_used"),
                later,
            )
            .unwrap();
        assert_eq!(settled.state, "rejected");
    }

    #[test]
    fn request_ids_are_tombstoned_forever() {
        let (_root, mut store, account) = fixture();
        observe(&mut store, &account, 20.0, &[("credit-a", true)]);
        store
            .redeem_request(&account, "credit-a", ONE, None, PIN, 30.0)
            .unwrap();
        let claim = store
            .redeem_claim(&account, "rev-1", 31.0)
            .unwrap()
            .unwrap();
        let done = store
            .redeem_report(&account, ONE, &claim.claim_token, "redeemed", None, 32.0)
            .unwrap();
        let rejected = store
            .redeem_request(&account, "missing", TWO, None, PIN, 33.0)
            .unwrap();
        // Years later, with a fresh inventory that lists the credit again, the old ids
        // still answer with their final records and never become new requests.
        let years = 5.0 * 365.0 * 86_400.0;
        observe(&mut store, &account, years, &[("credit-a", true)]);
        store.redeem_expire(years + 1.0).unwrap();
        assert_eq!(
            store
                .redeem_request(&account, "credit-a", ONE, None, PIN, years + 2.0)
                .unwrap(),
            done
        );
        assert_eq!(
            store
                .redeem_request(&account, "missing", TWO, None, PIN, years + 2.0)
                .unwrap(),
            rejected
        );
        assert!(matches!(
            store.redeem_request(&account, "credit-b", ONE, None, PIN, years + 2.0),
            Err(RedeemError::Conflict)
        ));
        assert!(!store.redeem_waiting(&account).unwrap());
        assert!(
            store
                .redeem_claim(&account, "rev-1", years + 3.0)
                .unwrap()
                .is_none()
        );
        assert_eq!(store.redeem_status(ONE, years + 4.0).unwrap(), done);
    }

    #[test]
    fn hand_out_requires_the_configuration_the_request_was_admitted_under() {
        let (_root, mut store, account) = fixture();
        observe(&mut store, &account, 20.0, &[("credit-a", true)]);
        store
            .redeem_request(&account, "credit-a", ONE, None, PIN, 30.0)
            .unwrap();
        assert_eq!(
            store.redeem_instances().unwrap(),
            [("fixture-instance".to_owned(), true)]
        );
        assert!(
            store
                .redeem_claim(&account, "rev-2", 31.0)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state(&mut store, ONE, 32.0),
            ("failed".into(), Some("binding_changed".into()))
        );
        // The instance stays known after its request ended, so its trigger can be swept.
        assert_eq!(
            store.redeem_instances().unwrap(),
            [("fixture-instance".to_owned(), false)]
        );
        // A request recorded without a binding can never be handed out.
        store
            .redeem_request(&account, "credit-a", TWO, None, None, 33.0)
            .unwrap();
        assert!(
            store
                .redeem_claim(&account, "rev-1", 34.0)
                .unwrap()
                .is_none()
        );
        assert_eq!(
            state(&mut store, TWO, 35.0).1.as_deref(),
            Some("binding_changed")
        );
        store
            .redeem_request(&account, "credit-a", THREE, None, PIN, 36.0)
            .unwrap();
        let claim = store
            .redeem_claim(&account, "rev-1", 37.0)
            .unwrap()
            .unwrap();
        assert_eq!(claim.config_revision, "rev-1");
    }

    #[test]
    fn abort_only_ends_an_unclaimed_request() {
        let (_root, mut store, account) = fixture();
        observe(&mut store, &account, 20.0, &[("credit-a", true)]);
        store
            .redeem_request(
                &account,
                "credit-a",
                ONE,
                Some("redeem_not_configured"),
                None,
                21.0,
            )
            .unwrap();
        store
            .redeem_request(&account, "credit-a", TWO, None, PIN, 22.0)
            .unwrap();
        let aborted = store
            .redeem_abort(TWO, "trigger_unavailable", 23.0)
            .unwrap();
        assert_eq!(
            (aborted.state.as_str(), aborted.reason.as_deref()),
            ("failed", Some("trigger_unavailable"))
        );
        assert_eq!(
            store
                .redeem_abort(ONE, "trigger_unavailable", 24.0)
                .unwrap()
                .state,
            "rejected"
        );
    }
}
