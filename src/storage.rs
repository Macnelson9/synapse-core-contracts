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

use crate::types::{ContractError, StorageKey, Transaction};

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
    /// mistyped or already-archived ID.
    pub fn bump_transaction_ttl(env: &Env, tx_id: &String) -> Result<(), ContractError> {
        let key = StorageKey::Transaction(tx_id.clone());
        if !env.storage().persistent().has(&key) {
            return Err(ContractError::TransactionNotFound);
        }
        env.storage().persistent().extend_ttl(
            &key,
            TRANSACTION_BUMP_TTL_LEDGERS,
            TRANSACTION_BUMP_TTL_LEDGERS,
        );
        Ok(())
    }

    /// Extend the persistent-storage TTL of up to [`TTL_BUMP_BATCH_SIZE`]
    /// transaction records in a single pass.
    ///
    /// Returns the number of records actually bumped.  The batch stops at the
    /// bound rather than silently truncating: callers observe the returned
    /// count and re-invoke with the remaining IDs to signal partial completion
    /// (see `DEPLOYMENT.md` for the recommended scheduled-job pattern).
    ///
    /// Fails cleanly with [`ContractError::TransactionNotFound`] if any supplied
    /// `tx_id` has no record, so a partially-applied batch is never reported as
    /// a success.
    pub fn bump_transactions_ttl(
        env: &Env,
        tx_ids: &soroban_sdk::Vec<String>,
    ) -> Result<u32, ContractError> {
        let mut bumped: u32 = 0;
        for tx_id in tx_ids.iter() {
            if bumped >= TTL_BUMP_BATCH_SIZE {
                break;
            }
            Self::bump_transaction_ttl(env, &tx_id)?;
            bumped += 1;
        }
        Ok(bumped)
    }

    // ── Idempotency keys ──────────────────────────────────────────────────────

    /// Return the ledger sequence at which an idempotency key was first stored,
    /// or `None` if the key is unknown / expired.
    pub fn get_idempotency_key(env: &Env, key: &String) -> Option<u32> {
        env.storage()
            .temporary()
            .get::<StorageKey, u32>(&StorageKey::IdempotencyKey(key.clone()))
    }

    /// Record an idempotency key with a ~24-hour TTL.
    pub fn set_idempotency_key(env: &Env, key: &String) {
        let storage_key = StorageKey::IdempotencyKey(key.clone());
        env.storage()
            .temporary()
            .set(&storage_key, &env.ledger().sequence());
        env.storage().temporary().extend_ttl(
            &storage_key,
            IDEMPOTENCY_TTL_LEDGERS,
            IDEMPOTENCY_TTL_LEDGERS,
        );
    }

    /// Returns `true` if the temporary idempotency key `key` is still within
    /// its valid TTL window and must therefore not be drained.
    ///
    /// The key is considered *expiring* (safe to evict) only once the ledger
    /// sequence has advanced at least [`IDEMPOTENCY_TTL_LEDGERS`] past the
    /// sequence at which it was recorded.  The boundary is inclusive: a key
    /// recorded exactly `IDEMPOTENCY_TTL_LEDGERS` ledgers ago is exp

/* … truncated 1057 chars — edit only what you need near the top … */
