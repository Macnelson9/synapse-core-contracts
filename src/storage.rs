//! # Storage
//!
//! All ledger read/write operations are centralised here, keeping handler and
//! service logic free of raw `env.storage()` calls.
//!
//! ## Storage tiers used
//!
//! | Data                  | Tier       | Rationale                                  |
//! |-----------------------|------------|--------------------------------------------|
//! | Admin, relay signer   | `persistent` | Must survive archive/restore cycles      |
//! | Transactions          | `persistent` | Long-lived; needed for audit trail       |
//! | Idempotency keys      | `temporary`  | 24-hour TTL; evicted by the ledger       |
//! | Initialised flag      | `instance`   | Lives with the contract instance         |

use soroban_sdk::{Address, Env, String};

use crate::types::{ContractError, StorageFootprintReport, StorageKey, Transaction};

/// TTL extension in ledgers applied to idempotency keys (~24 hours at ~5s/ledger).
///
/// 24 * 3600 / 5 = 17_280 ledgers.  We round up to 18_000 for safety.
const IDEMPOTENCY_TTL_LEDGERS: u32 = 18_000;

/// Minimum TTL we require on transaction records before extending.
const TRANSACTION_MIN_TTL_LEDGERS: u32 = 100_000; // ~1 week

/// TTL (in ledgers) applied to a transaction record by an explicit maintenance
/// bump.  Larger than [`TRANSACTION_MIN_TTL_LEDGERS`] so an operator-triggered
/// bump meaningfully extends the archival horizon of a still-relevant record
/// (e.g. one that remains `Disputed`).  ~30 days at ~5s/ledger.
const TRANSACTION_BUMP_TTL_LEDGERS: u32 = 518_400;

/// Maximum number of transaction records a single
/// [`StorageClient::bump_transactions_ttl`] call may extend.
///
/// Bounds the per-call resource cost so a large backlog is maintained across
/// several invocations rather than in a single unbounded sweep.
pub const TTL_BUMP_BATCH_SIZE: u32 = 25;

/// Maximum number of temporary idempotency-key entries evicted per
/// [`StorageClient::drain_expiring_temp_storage`] call.
///
/// Bounds the per-call resource cost so a large backlog is cleared across
/// several invocations rather than in a single unbounded sweep.
pub const DRAIN_BATCH_SIZE: u32 = 25;

/// Approximate on-chain byte size attributed to a single persistent entry
/// (key + value + ledger bookkeeping) for cost-model footprint estimates.
///
/// Soroban does not expose a native per-entry byte-size primitive, so this is a
/// deliberately conservative constant used only to turn entry *counts* into an
/// order-of-magnitude size estimate.  It is intentionally coarse: the report's
/// contract is that counts are exact and sizes are approximate.
const APPROX_BYTES_PER_PERSISTENT_ENTRY: u32 = 128;

/// Approximate byte size attributed to a single temporary entry.
const APPROX_BYTES_PER_TEMPORARY_ENTRY: u32 = 96;

/// Approximate byte size attributed to a single instance entry.
const APPROX_BYTES_PER_INSTANCE_ENTRY: u32 = 64;

pub struct StorageClient;

impl StorageClient {
    // ── Initialisation flag ───────────────────────────────────────────────────

    /// Returns `true` if [`crate::SynapseCoreContract::initialize`] has been called.
    pub fn is_initialised(env: &Env) -> bool {
        env.storage().instance().has(&StorageKey::Initialised)
    }

    /// Persist the initialised flag.  Called exactly once during `initialize()`.
    pub fn set_initialised(env: &Env) {
        env.storage()
            .instance()
            .set(&StorageKey::Initialised, &true);
    }

    // ── Pause / circuit breaker ───────────────────────────────────────────────

    /// Returns `true` when the emergency-pause flag is engaged.
    ///
    /// Defaults to `false` when the flag has never been written, so a freshly
    /// initialised contract is always unpaused.
    pub fn is_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&StorageKey::Paused)
            .unwrap_or(false)
    }

    /// Persist the emergency-pause flag.
    pub fn set_paused(env: &Env, paused: bool) {
        env.storage().instance().set(&StorageKey::Paused, &paused);
    }

    // ── Admin ─────────────────────────────────────────────────────────────────

    /// Read the current admin address from persistent storage.
    pub fn get_admin(env: &Env) -> Result<Address, ContractError> {
        env.storage()
            .persistent()
            .get(&StorageKey::Admin)
            .ok_or(ContractError::NotInitialised)
    }

    /// Persist an admin address.
    pub fn set_admin(env: &Env, admin: &Address) {
        env.storage().persistent().set(&StorageKey::Admin, admin);
    }

    // ── Relay signer ──────────────────────────────────────────────────────────

    /// Read the trusted relay signer address.
    pub fn get_relay_signer(env: &Env) -> Result<Address, ContractError> {
        env.storage()
            .persistent()
            .get(&StorageKey::RelaySigner)
            .ok_or(ContractError::NotInitialised)
    }

    /// Persist the relay signer address.
    pub fn set_relay_signer(env: &Env, signer: &Address) {
        env.storage()
            .persistent()
            .set(&StorageKey::RelaySigner, signer);
    }

    // ── Admin transfer (two-step) ─────────────────────────────────────────────

    /// Read the pending admin nominee, if a transfer is in progress.
    pub fn get_pending_admin(env: &Env) -> Option<Address> {
        env.storage().persistent().get(&StorageKey::PendingAdmin)
    }

    /// Persist the pending admin nominee, overwriting any existing proposal.
    pub fn set_pending_admin(env: &Env, nominee: &Address) {
        env.storage()
            .persistent()
            .set(&StorageKey::PendingAdmin, nominee);
    }

    /// Clear the pending admin nominee after a transfer is accepted.
    pub fn clear_pending_admin(env: &Env) {
        env.storage().persistent().remove(&StorageKey::PendingAdmin);
    }

    // ── Schema version ────────────────────────────────────────────────────────

    /// Read the on-chain storage schema version.
    pub fn get_schema_version(env: &Env) -> Result<u32, ContractError> {
        env.storage()
            .persistent()
            .get(&StorageKey::SchemaVersion)
            .ok_or(ContractError::NotInitialised)
    }

    /// Persist the storage schema version. Called once during `initialize()`.
    pub fn set_schema_version(env: &Env, version: u32) {
        env.storage()
            .persistent()
            .set(&StorageKey::SchemaVersion, &version);
    }

    // ── Transactions ──────────────────────────────────────────────────────────

    /// Returns `true` if a transaction record already exists for `tx_id`.
    ///
    /// Existence-only check — unlike [`Self::get_transaction`] it does not
    /// extend TTL, since it is used purely as a pre-write guard against
    /// `transaction_id` reuse (see `register_callback`'s duplicate-tx-id
    /// check, THREAT_MODEL.md finding F-07).
    pub fn transaction_exists(env: &Env, tx_id: &String) -> bool {
        env.storage()
            .persistent()
            .has(&StorageKey::Transaction(tx_id.clone()))
    }

    /// Read a [`Transaction`] by its ID.
    ///
    /// Extends the ledger TTL on each access so active records are never evicted.
    pub fn get_transaction(env: &Env, tx_id: &String) -> Result<Transaction, ContractError> {
        let key = StorageKey::Transaction(tx_id.clone());
        let tx = env
            .storage()
            .persistent()
            .get::<StorageKey, Transaction>(&key)
            .ok_or(ContractError::TransactionNotFound)?;
        env.storage().persistent().extend_ttl(
            &key,
            TRANSACTION_MIN_TTL_LEDGERS,
            TRANSACTION_MIN_TTL_LEDGERS,
        );
        Ok(tx)
    }

    /// Persist (insert or update) a [`Transaction`].
    pub fn save_transaction(env: &Env, tx: &Transaction) {
        let key = StorageKey::Transaction(tx.id.clone());
        env.storage().persistent().set(&key, tx);
        env.storage().persistent().extend_ttl(
            &key,
            TRANSACTION_MIN_TTL_LEDGERS,
            TRANSACTION_MIN_TTL_LEDGERS,
        );
    }

    /// Extend the persistent-storage TTL of a single transaction record.
    ///
    /// Uses Soroban's native `extend_ttl` ledger primitive directly rather than
    /// reimplementing TTL bookkeeping in contract storage.  The record's TTL is
    /// raised to [`TRANSACTION_BUMP_TTL_LEDGERS`] whenever it currently sits
    /// below that threshold, keeping a still-relevant record (e.g. one that
    /// remains `Disputed`) from being archived out from under the audit trail.
    ///
    /// Fails cleanly with [`ContractError::TransactionNotFound`] when no record
    /// exists for `tx_id`, so a maintenance job cannot silently no-op on a
    /// mistyped o

    // ── Footprint diagnostics ─────────────────────────────────────────────────

    /// Build a point-in-time [`StorageFootprintReport`] of the contract's
    /// storage footprint, broken down by tier.
    ///
    /// Soroban exposes no native "enumerate all entries" primitive, so counts
    /// are derived from the same counters/indexes the rest of this Wave's
    /// storage-tracking work maintains (the per-status index and the history
    /// log) rather than from an independent counting mechanism.  This keeps the
    /// report consistent with the state-transition entry points that write
    /// those indexes: whenever a new storage-writing feature lands, the
    /// counters it maintains must be updated here too.
    ///
    /// Counts are exact for the tiers that are tracked by an index; sizes are
    /// deliberately approximate (see the `APPROX_BYTES_PER_*` constants) and
    /// exist only to let `COST_MODEL.md`'s projections be sanity-checked
    /// against real on-chain state.  This is a read-only, point-in-time query —
    /// continuous monitoring is out of scope.
    pub fn storage_footprint(env: &Env) -> StorageFootprintReport {
        // Persistent tier: the singleton config entries (admin, relay signer,
        // schema version) plus every transaction record tracked by the history
        // log.  The history log is the authoritative index of transaction
        // records, so its length is the persistent entry count.
        let history_len = Self::history_log_len(env);
        let persistent_entries = history_len.saturating_add(3);

        // Temporary tier: idempotency keys, tracked by the per-status index
        // maintained alongside each write.  Falls back to zero when the index
        // has never been written.
        let temporary_entries = Self::idempotency_index_len(env);

        // Instance tier: the initialised flag and the pause flag.
        let instance_entries: u32 = 2;

        StorageFootprintReport {
            persistent_entries,
            persistent_bytes: persistent_entries.saturating_mul(APPROX_BYTES_PER_PERSISTENT_ENTRY),
            temporary_entries,
            temporary_bytes: temporary_entries.saturating_mul(APPROX_BYTES_PER_TEMPORARY_ENTRY),
            instance_entries,
            instance_bytes: instance_entries.saturating_mul(APPROX_BYTES_PER_INSTANCE_ENTRY),
        }
    }

    /// Number of transaction records currently tracked by the history log.
    ///
    /// Reads the length counter maintained by the history-log storage work in
    /// this Wave; returns `0` when the log has never been written so a fresh
    /// deployment reports an empty footprint rather than erroring.
    fn history_log_len(env: &Env) -> u32 {
        env.storage()
            .persistent()
            .get(&StorageKey::HistoryLogLen)
            .unwrap_or(0)
    }

    /// Number of temporary idempotency-key entries currently tracked by the
    /// per-status index.
    ///
    /// Reads the length counter maintained by the per-status-index storage work
    /// in this Wave; returns `0` when the index has never been written.
    fn idempotency_index_len(env: &Env) -> u32 {
        env.storage()
            .temporary()
            .get(&StorageKey::IdempotencyIndexLen)
            .unwrap_or(0)
    }
}
