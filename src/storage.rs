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
//! | Promoted idem. keys   | `persistent` | Dispute evidence; survives TTL eviction  |
//! | Initialised flag      | `instance`   | Lives with the contract instance         |
//!
//! ## Temporary-tier read semantics
//!
//! Temporary entries are *not* restorable: once an idempotency key passes its
//! TTL the ledger simply drops it, and a subsequent read returns `None` rather
//! than triggering a restoration panic. Every temporary-tier read site in this
//! module therefore treats absence as a well-defined "key not found" business
//! case (see [`StorageClient::get_idempotency_key`]) so callers such as
//! `register_callback` fall through to their durable existing-record guard
//! instead of surfacing an unhandled contract error.

use soroban_sdk::{Address, Env, String, Vec};

use crate::types::{
    DEFAULT_AMOUNT_CEILING, ContractError, PendingRelaySigner, RelaySignerSet, StorageFootprintReport,
    StorageKey, Transaction, TransactionStatus, TransitionRecord, MAX_HISTORY_LEN,
};

/// TTL extension in ledgers applied to idempotency keys (~24 hours at ~5s/ledger).
///
/// 24 * 3600 / 5 = 17_280 ledgers.  We round up to 18_000 for safety.
const IDEMPOTENCY_TTL_LEDGERS: u32 = 18_000;

/// Minimum TTL we require on transaction records before extending.
const TRANSACTION_MIN_TTL_LEDGERS: u32 = 100_000; // ~1 week

/// Minimum TTL applied to promoted (persistent) idempotency keys.
///
/// Promoted keys back an active dispute investigation, so they are kept for
/// the same ~1 week window as transaction records and refreshed on access.
const PROMOTED_IDEMPOTENCY_MIN_TTL_LEDGERS: u32 = 100_000; // ~1 week

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
            .ok_or(if Self::is_admin_vacant(env) {
                ContractError::AdminVacant
            } else {
                ContractError::NotInitialised
            })
    }

    /// Persist an admin address.
    pub fn set_admin(env: &Env, admin: &Address) {
        env.storage().persistent().set(&StorageKey::Admin, admin);
    }

    // ── Relay signer ──────────────────────────────────────────────────────────

    /// Read the trusted relay signer address.
    ///
    /// With an N-of-M set this is the primary (first) signer.
    pub fn get_relay_signer(env: &Env) -> Result<Address, ContractError> {
        Self::get_relay_signer_set(env)?
            .signers
            .get(0)
            .ok_or(ContractError::NotInitialised)
    }

    /// Persist the relay signer address (replaces the primary signer when a
    /// signer set exists).
    pub fn set_relay_signer(env: &Env, signer: &Address) {
        env.storage()
            .persistent()
            .set(&StorageKey::RelaySigner, signer);
        if let Some(mut set) = Self::get_relay_signer_set_opt(env) {
            set.signers.set(0, signer.clone());
            Self::set_relay_signer_set(env, &set);
        }
    }

    fn get_relay_signer_set_opt(env: &Env) -> Option<RelaySignerSet> {
        env.storage().persistent().get(&StorageKey::RelaySignerSet)
    }

    /// Read the relay signer set. Pre-N-of-M deployments migrate lazily:
    /// the legacy single `RelaySigner` becomes `threshold = 1, signers = [it]`.
    pub fn get_relay_signer_set(env: &Env) -> Result<RelaySignerSet, ContractError> {
        if let Some(set) = Self::get_relay_signer_set_opt(env) {
            return Ok(set);
        }
        let legacy: Address = env
            .storage()
            .persistent()
            .get(&StorageKey::RelaySigner)
            .ok_or(ContractError::NotInitialised)?;
        Ok(RelaySignerSet {
            signers: soroban_sdk::vec![env, legacy],
            threshold: 1,
        })
    }

    /// Persist the relay signer set.
    pub fn set_relay_signer_set(env: &Env, set: &RelaySignerSet) {
        env.storage()
            .persistent()
            .set(&StorageKey::RelaySignerSet, set);
    }

    /// Record a signer's approval at the current ledger.
    pub fn set_relay_approval(env: &Env, signer: &Address) {
        env.storage().temporary().set(
            &StorageKey::RelayApproval(signer.clone()),
            &env.ledger().sequence(),
        );
    }

    /// Ledger at which `signer` last approved, if any.
    pub fn get_relay_approval(env: &Env, signer: &Address) -> Option<u32> {
        env.storage()
            .temporary()
            .get(&StorageKey::RelayApproval(signer.clone()))
    }

    /// Consume (remove) `signer`'s approval.
    pub fn clear_relay_approval(env: &Env, signer: &Address) {
        env.storage()
            .temporary()
            .remove(&StorageKey::RelayApproval(signer.clone()));
    }

    // ── Relay signer timelock ─────────────────────────────────────────────────

    /// Read the pending relay-signer change, if any.
    pub fn get_pending_relay_signer(env: &Env) -> Option<PendingRelaySigner> {
        env.storage()
            .persistent()
            .get(&StorageKey::PendingRelaySigner)
    }

    /// Persist (overwriting any existing) pending relay-signer change.
    pub fn set_pending_relay_signer(env: &Env, p: &PendingRelaySigner) {
        env.storage()
            .persistent()
            .set(&StorageKey::PendingRelaySigner, p);
    }

    /// Clear the pending relay-signer change.
    pub fn clear_pending_relay_signer(env: &Env) {
        env.storage()
            .persistent()
            .remove(&StorageKey::PendingRelaySigner);
    }

    /// Timelock delay in ledgers; defaults to the ~24h constant.
    pub fn get_relay_signer_delay(env: &Env) -> Option<u32> {
        env.storage().persistent().get(&StorageKey::RelaySignerDelay)
    }

    /// Persist the timelock delay in ledgers.
    pub fn set_relay_signer_delay(env: &Env, delay: u32) {
        env.storage()
            .persistent()
            .set(&StorageKey::RelaySignerDelay, &delay);
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
        env.storage()
            .persistent()
            .remove(&StorageKey::PendingAdminExpiry);
    }

    /// Next expected nonce for `addr` (0 if never used).
    pub fn get_nonce(env: &Env, addr: &Address) -> u64 {
        env.storage()
            .persistent()
            .get(&StorageKey::Nonce(addr.clone()))
            .unwrap_or(0)
    }

    /// Persist the next expected nonce for `addr`.
    pub fn set_nonce(env: &Env, addr: &Address, next: u64) {
        env.storage()
            .persistent()
            .set(&StorageKey::Nonce(addr.clone()), &next);
    }

    /// Allowed anchor/issuer IDs for `signer`; empty means unrestricted.
    pub fn get_relay_anchors(env: &Env, signer: &Address) -> Vec<String> {
        env.storage()
            .persistent()
            .get(&StorageKey::RelayAnchors(signer.clone()))
            .unwrap_or(Vec::new(env))
    }

    /// Replace the anchor allowlist for `signer`.
    pub fn set_relay_anchors(env: &Env, signer: &Address, anchors: &Vec<String>) {
        env.storage()
            .persistent()
            .set(&StorageKey::RelayAnchors(signer.clone()), anchors);
    }

    /// Read the pending relay-signer nominee.
    pub fn get_pending_relay_signer(env: &Env) -> Option<Address> {
        env.storage()
            .persistent()
            .get(&StorageKey::PendingRelaySigner)
    }

    /// Store (`Some`) or clear (`None`) the pending relay-signer nominee.
    pub fn set_pending_relay_signer(env: &Env, nominee: Option<&Address>) {
        match nominee {
            Some(a) => env
                .storage()
                .persistent()
                .set(&StorageKey::PendingRelaySigner, a),
            None => env
                .storage()
                .persistent()
                .remove(&StorageKey::PendingRelaySigner),
        }
    }

    /// Read the optional expiry timestamp of the pending admin proposal.
    pub fn get_pending_admin_expiry(env: &Env) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&StorageKey::PendingAdminExpiry)
    }

    /// Set (or clear, with `None`) the pending admin proposal expiry.
    pub fn set_pending_admin_expiry(env: &Env, expiry: Option<u64>) {
        match expiry {
            Some(t) => env
                .storage()
                .persistent()
                .set(&StorageKey::PendingAdminExpiry, &t),
            None => env
                .storage()
                .persistent()
                .remove(&StorageKey::PendingAdminExpiry),
        }
    }

    /// Read the outgoing admin recorded at the last successful `accept_admin`.
    pub fn get_previous_admin(env: &Env) -> Option<Address> {
        env.storage().persistent().get(&StorageKey::PreviousAdmin)
    }

    /// Persist the outgoing admin so they may later call `renounce_admin`.
    pub fn set_previous_admin(env: &Env, previous: &Address) {
        env.storage()
            .persistent()
            .set(&StorageKey::PreviousAdmin, previous);
    }

    /// Clear the outgoing-admin marker after a successful renounce.
    pub fn clear_previous_admin(env: &Env) {
        env.storage()
            .persistent()
            .remove(&StorageKey::PreviousAdmin);
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

    // ── Guardian (#78) ────────────────────────────────────────────────────────

    /// Read the guardian address, if configured.
    pub fn get_guardian(env: &Env) -> Option<Address> {
        env.storage().persistent().get(&StorageKey::Guardian)
    }

    /// Persist the guardian address.
    pub fn set_guardian(env: &Env, guardian: &Address) {
        env.storage()
            .persistent()
            .set(&StorageKey::Guardian, guardian);
    }

    // ── Auto-pause origin (#78) ───────────────────────────────────────────────

    /// Returns `true` when the current pause was engaged by an automatic trip.
    pub fn is_auto_paused(env: &Env) -> bool {
        env.storage()
            .instance()
            .get(&StorageKey::AutoPaused)
            .unwrap_or(false)
    }

    /// Persist whether the current pause is automatic.
    pub fn set_auto_paused(env: &Env, auto: bool) {
        env.storage().instance().set(&StorageKey::AutoPaused, &auto);
    }

    // ── Auto-unpause votes (#78) ──────────────────────────────────────────────

    /// Read accumulated auto-unpause role votes (defaults to empty).
    pub fn get_auto_unpause_votes(env: &Env) -> crate::types::AutoUnpauseVotes {
        env.storage()
            .instance()
            .get(&StorageKey::AutoUnpauseVotes)
            .unwrap_or_else(crate::types::AutoUnpauseVotes::empty)
    }

    /// Persist auto-unpause role votes.
    pub fn set_auto_unpause_votes(env: &Env, votes: &crate::types::AutoUnpauseVotes) {
        env.storage()
            .instance()
            .set(&StorageKey::AutoUnpauseVotes, votes);
    }

    /// Clear auto-unpause votes after a successful unpause.
    pub fn clear_auto_unpause_votes(env: &Env) {
        env.storage()
            .instance()
            .remove(&StorageKey::AutoUnpauseVotes);
    }

    // ── Admin rate limit (#75) ────────────────────────────────────────────────

    /// Read the admin rate-limit config, if configured.
    pub fn get_admin_rate_limit_config(env: &Env) -> Option<crate::types::AdminRateLimitConfig> {
        env.storage()
            .instance()
            .get(&StorageKey::AdminRateLimitConfig)
    }

    /// Persist the admin rate-limit config.
    pub fn set_admin_rate_limit_config(env: &Env, config: &crate::types::AdminRateLimitConfig) {
        env.storage()
            .instance()
            .set(&StorageKey::AdminRateLimitConfig, config);
    }

    /// Read the admin rate-limit counter state.
    pub fn get_admin_rate_limit_state(env: &Env) -> Option<crate::types::AdminRateLimitState> {
        env.storage()
            .instance()
            .get(&StorageKey::AdminRateLimitState)
    }

    /// Persist the admin rate-limit counter state.
    pub fn set_admin_rate_limit_state(env: &Env, state: &crate::types::AdminRateLimitState) {
        env.storage()
            .instance()
            .set(&StorageKey::AdminRateLimitState, state);
    }

    // ── Signer attestation (#77) ──────────────────────────────────────────────

    /// Read a signer's self-reported build fingerprint, if any.
    pub fn get_signer_attestation(env: &Env, signer: &Address) -> Option<soroban_sdk::BytesN<32>> {
        env.storage()
            .persistent()
            .get(&StorageKey::SignerAttestation(signer.clone()))
    }

    /// Persist a signer's self-reported build fingerprint.
    pub fn set_signer_attestation(
        env: &Env,
        signer: &Address,
        build_hash: &soroban_sdk::BytesN<32>,
    ) {
        env.storage()
            .persistent()
            .set(&StorageKey::SignerAttestation(signer.clone()), build_hash);
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
    ///
    /// Also keeps the per-status index in sync. Trade-off: each status change
    /// costs one extra read plus up to two index writes (removal is O(n) in
    /// the size of the old status bucket), in exchange for O(page) reads in
    /// `get_transactions_by_status`.
    pub fn save_transaction(env: &Env, tx: &Transaction) {
        let key = StorageKey::Transaction(tx.id.clone());
        let old = env
            .storage()
            .persistent()
            .get::<StorageKey, Transaction>(&key);
        match old {
            Some(old) if old.status == tx.status => {}
            Some(old) => {
                Self::index_remove(env, &old.status, &tx.id);
                Self::index_push(env, &tx.status, &tx.id);
            }
            None => Self::index_push(env, &tx.status, &tx.id),
        }
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

    // ── Merge markers ─────────────────────────────────────────────────────────

    /// Return the canonical tx id `tx_id` was merged into, if any.
    pub fn get_merged_into(env: &Env, tx_id: &String) -> Option<String> {
        env.storage()
            .persistent()
            .get(&StorageKey::MergedInto(tx_id.clone()))
    }

    /// Persist the `MergedInto(canonical)` marker for `duplicate`.
    pub fn set_merged_into(env: &Env, duplicate: &String, canonical: &String) {
        let key = StorageKey::MergedInto(duplicate.clone());
        env.storage().persistent().set(&key, canonical);
    }

    // ── Transaction history ───────────────────────────────────────────────────

    /// Append a transition record to the transaction's history (persistent).
    ///
    /// Bounded at [`MAX_HISTORY_LEN`]; once full the oldest entry is dropped
    /// so the newest are kept and the transition itself is never blocked.
    pub fn append_history(env: &Env, tx_id: &String, status: TransactionStatus, caller: &Address) {
        let key = StorageKey::History(tx_id.clone());
        let mut h: Vec<TransitionRecord> = env
            .storage()
            .persistent()
            .get(&key)
            .unwrap_or_else(|| Vec::new(env));
        if h.len() >= MAX_HISTORY_LEN {
            h.pop_front();
        }
        h.push_back(TransitionRecord {
            status,
            caller: caller.clone(),
            timestamp: env.ledger().timestamp(),
        });
        env.storage().persistent().set(&key, &h);
        env.storage().persistent().extend_ttl(
            &key,
            TRANSACTION_MIN_TTL_LEDGERS,
            TRANSACTION_MIN_TTL_LEDGERS,
        );
    }

    // ── Forwarding routes ─────────────────────────────────────────────────────

    /// Return the configured `next_phase` for `tx_id`, if any.
    pub fn get_forward_route(env: &Env, tx_id: &String) -> Option<u32> {
        env.storage()
            .persistent()
            .get(&StorageKey::ForwardRoute(tx_id.clone()))
    }

    /// Set (`Some`) or clear (`None`) the forwarding route for `tx_id`.
    pub fn set_forward_route(env: &Env, tx_id: &String, next_phase: Option<u32>) {
        let key = StorageKey::ForwardRoute(tx_id.clone());
        match next_phase {
            Some(p) => env.storage().persistent().set(&key, &p),
            None => env.storage().persistent().remove(&key),
        }
    }

    /// Read a transaction's history, oldest first (empty if none recorded).
    pub fn get_history(env: &Env, tx_id: &String) -> Vec<TransitionRecord> {
        env.storage()
            .persistent()
            .get(&StorageKey::History(tx_id.clone()))
            .unwrap_or_else(|| Vec::new(env))
    }

    // ── Per-signer outstanding-Pending cap ────────────────────────────────────

    /// Configured cap on outstanding `Pending` transactions per signer
    /// (`None` = unlimited).
    pub fn get_max_pending_per_signer(env: &Env) -> Option<u32> {
        env.storage()
            .persistent()
            .get(&StorageKey::MaxPendingPerSigner)
    }

    /// Persist the per-signer outstanding-Pending cap.
    pub fn set_max_pending_per_signer(env: &Env, cap: u32) {
        env.storage()
            .persistent()
            .set(&StorageKey::MaxPendingPerSigner, &cap);
    }

    /// Current outstanding `Pending` count for `signer`.
    pub fn get_pending_count(env: &Env, signer: &Address) -> u32 {
        env.storage()
            .persistent()
            .get(&StorageKey::PendingCount(signer.clone()))
            .unwrap_or(0)
    }

    /// Record that `signer` registered `tx_id` and bump its counter.
    pub fn inc_pending(env: &Env, signer: &Address, tx_id: &String) {
        let n = Self::get_pending_count(env, signer) + 1;
        env.storage()
            .persistent()
            .set(&StorageKey::PendingCount(signer.clone()), &n);
        env.storage()
            .persistent()
            .set(&StorageKey::TxSigner(tx_id.clone()), signer);
    }

    /// Decrement the counter of the signer that registered `tx_id`, called
    /// when the transaction leaves `Pending`. Never underflows.
    pub fn dec_pending(env: &Env, tx_id: &String) {
        let key = StorageKey::TxSigner(tx_id.clone());
        if let Some(signer) = env.storage().persistent().get::<StorageKey, Address>(&key) {
            let n = Self::get_pending_count(env, &signer);
            debug_assert!(n > 0, "pending counter underflow");
            env.storage().persistent().set(
                &StorageKey::PendingCount(signer),
                &n.saturating_sub(1),
            );
            env.storage().persistent().remove(&key);
        }
    }

    // ── Dispute overlay flag ──────────────────────────────────────────────────

    /// Whether `tx_id` is currently flagged as disputed (overlay on status).
    pub fn is_disputed(env: &Env, tx_id: &String) -> bool {
        env.storage()
            .persistent()
            .has(&StorageKey::Disputed(tx_id.clone()))
    }

    /// Set or clear the dispute overlay flag.
    pub fn set_disputed(env: &Env, tx_id: &String, disputed: bool) {
        let key = StorageKey::Disputed(tx_id.clone());
        if disputed {
            env.storage().persistent().set(&key, &true);
        } else {
            env.storage().persistent().remove(&key);
        }
    }

    // ── Amount ceilings ───────────────────────────────────────────────────────

    /// Effective ceiling for `anchor`: explicit entry, else the default.
    pub fn get_amount_ceiling(env: &Env, anchor: &String) -> i128 {
        let p = env.storage().persistent();
        p.get(&StorageKey::AnchorCeiling(anchor.clone()))
            .or_else(|| p.get(&StorageKey::DefaultCeiling))
            .unwrap_or(DEFAULT_AMOUNT_CEILING)
    }

    /// Set an explicit ceiling for `anchor`.
    pub fn set_anchor_ceiling(env: &Env, anchor: &String, ceiling: i128) {
        env.storage()
            .persistent()
            .set(&StorageKey::AnchorCeiling(anchor.clone()), &ceiling);
    }

    /// Set the default ceiling for anchors without an explicit entry.
    pub fn set_default_ceiling(env: &Env, ceiling: i128) {
        env.storage()
            .persistent()
            .set(&StorageKey::DefaultCeiling, &ceiling);
    }

    // ── Expiry window ─────────────────────────────────────────────────────────

    /// Read the `Pending` expiry window in seconds, if configured.
    pub fn get_expiry_window(env: &Env) -> Option<u64> {
        env.storage().persistent().get(&StorageKey::ExpiryWindow)
    }

    /// Persist the `Pending` expiry window in seconds.
    pub fn set_expiry_window(env: &Env, seconds: u64) {
        env.storage()
            .persistent()
            .set(&StorageKey::ExpiryWindow, &seconds);
    }

    // ── Standby signer ────────────────────────────────────────────────────────

    /// Read the admin-approved standby relay signer, if any.
    pub fn get_standby_signer(env: &Env) -> Option<Address> {
        env.storage().persistent().get(&StorageKey::StandbySigner)
    }

    /// Persist the admin-approved standby relay signer.
    pub fn set_standby_signer(env: &Env, signer: &Address) {
        env.storage()
            .persistent()
            .set(&StorageKey::StandbySigner, signer);
    }

    fn index_push(env: &Env, status: &TransactionStatus, id: &String) {
        let key = StorageKey::StatusIndex(status.clone());
        let mut ids = env
            .storage()
            .persistent()
            .get::<StorageKey, Vec<String>>(&key)
            .unwrap_or(Vec::new(env));
        ids.push_back(id.clone());
        env.storage().persistent().set(&key, &ids);
        env.storage()
            .persistent()
            .extend_ttl(&key, TRANSACTION_MIN_TTL_LEDGERS, TRANSACTION_MIN_TTL_LEDGERS);
    }

    fn index_remove(env: &Env, status: &TransactionStatus, id: &String) {
        let key = StorageKey::StatusIndex(status.clone());
        if let Some(mut ids) = env
            .storage()
            .persistent()
            .get::<StorageKey, Vec<String>>(&key)
        {
            if let Some(pos) = ids.first_index_of(id) {
                ids.remove(pos);
                env.storage().persistent().set(&key, &ids);
            }
        }
    }

    /// Return up to `limit` transaction IDs in `status`, starting at `start`.
    pub fn get_ids_by_status(
        env: &Env,
        status: &TransactionStatus,
        start: u32,
        limit: u32,
    ) -> Vec<String> {
        let ids = env
            .storage()
            .persistent()
            .get::<StorageKey, Vec<String>>(&StorageKey::StatusIndex(status.clone()))
            .unwrap_or(Vec::new(env));
        let end = start.saturating_add(limit).min(ids.len());
        let mut out = Vec::new(env);
        let mut i = start;
        while i < end {
            out.push_back(ids.get_unchecked(i));
            i += 1;
        }
        out
    }

    // ── Idempotency keys ──────────────────────────────────────────────────────

    /// Return the ledger sequence at which an idempotency key was first stored,
    /// or `None` if the key is unknown / expired.
    ///
    /// This is the sole temporary-tier read site in the contract. Temporary
    /// entries are not restorable: once the key's TTL elapses the ledger evicts
    /// it and this read yields `None` — it never triggers a restoration panic.
    /// Callers must therefore treat `None` as the well-defined "key absent"
    /// business case (e.g. `register_callback` falls through to its durable
    /// `transaction_exists` guard) rather than assuming presence.
    pub fn get_idempotency_key(env: &Env, key: &String) -> Option<u32> {
        env.storage()
            .temporary()
            .get::<StorageKey, u32>(&StorageKey::IdempotencyKey(key.clone()))
    }

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

    /// Number
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

    // ── Idempotency key promotion (dispute evidence) ──────────────────────────

    /// Returns `true` if `key` currently lives in the persistent (promoted) tier.
    pub fn is_idempotency_key_promoted(env: &Env, key: &String) -> bool {
        env.storage()
            .persistent()
            .has(&StorageKey::PromotedIdempotencyKey(key.clone()))
    }

    /// Read a promoted idempotency key's original ledger sequence, if present.
    ///
    /// Extends the persistent TTL on each access so an in-flight dispute
    /// investigation never loses its evidence mid-review.
    pub fn get_promoted_idempotency_key(env: &Env, key: &String) -> Option<u32> {
        let storage_key = StorageKey::PromotedIdempotencyKey(key.clone());
        let seq = env
            .storage()
            .persistent()
            .get::<StorageKey, u32>(&storage_key)?;
        env.storage().persistent().extend_ttl(
            &storage_key,
            PROMOTED_IDEMPOTENCY_MIN_TTL_LEDGERS,
            PROMOTED_IDEMPOTENCY_MIN_TTL_LEDGERS,
        );
        Some(seq)
    }

    /// Promote an idempotency key from the temporary tier to the persistent tier.
    ///
    /// Reads the key's original ledger sequence from temporary storage and
    /// re-writes it under [`StorageKey::PromotedIdempotencyKey`] in persistent
    /// storage, then removes the temporary entry.  Returns the promoted ledger
    /// sequence on success.
    ///
    /// Fails with [`ContractError::IdempotencyKeyNotFound`] when the key is
    /// unknown or has already been evicted by the ledger's temporary TTL — the
    /// caller must surface this as a graceful "nothing to promote" error rather
    /// than panicking.
    pub fn promote_idempotency_key(env: &Env, key: &String) -> Result<u32, ContractError> {
        let seq = Self::get_idempotency_key(env, key)
            .ok_or(ContractError::IdempotencyKeyNotFound)?;
        let storage_key = StorageKey::PromotedIdempotencyKey(key.clone());
        env.storage().persistent().set(&storage_key, &seq);
        env.storage().persistent().extend_ttl(
            &storage_key,
            PROMOTED_IDEMPOTENCY_MIN_TTL_LEDGERS,
            PROMOTED_IDEMPOTENCY_MIN_TTL_LEDGERS,
        );
        env.storage()
            .temporary()
            .remove(&StorageKey::IdempotencyKey(key.clone()));
        Ok(seq)
    }

    /// Demote a previously promoted idempotency key back to the temporary tier.
    ///
    /// Used once a dispute resolves so persistent storage is not permanently
    /// bloated with every disputed key.  Returns `true` when a promoted entry
    /// was found and demoted, `false` when the key was not promoted (a no-op).
    pub fn demote_idempotency_key(env: &Env, key: &String) -> bool {
        let storage_key = StorageKey::PromotedIdempotencyKey(key.clone());
        let seq = match env
            .storage()
            .persistent()
            .get::<StorageKey, u32>(&storage_key)
        {
            Some(seq) => seq,
            None => return false,
        };
        env.storage().persistent().remove(&storage_key);
        let temp_key = StorageKey::IdempotencyKey(key.clone());
        env.storage().temporary().set(&temp_key, &seq);
        env.storage().temporary().extend_ttl(
            &temp_key,
            IDEMPOTENCY_TTL_LEDGERS,
            IDEMPOTENCY_TTL_LEDGERS,
        );
        true
    }

    // ── Relay-signer liveness ─────────────────────────────────────────────────

    /// Last heartbeat ledger timestamp recorded for `signer`, if any.
    pub fn get_last_heartbeat(env: &Env, signer: &Address) -> Option<u64> {
        env.storage()
            .persistent()
            .get(&StorageKey::LastHeartbeat(signer.clone()))
    }

    /// Record `ts` as `signer`'s last heartbeat.
    pub fn set_last_heartbeat(env: &Env, signer: &Address, ts: u64) {
        env.storage()
            .persistent()
            .set(&StorageKey::LastHeartbeat(signer.clone()), &ts);
    }

    /// Staleness window in seconds; `0` (default) disables quarantine.
    pub fn get_heartbeat_window(env: &Env) -> u64 {
        env.storage()
            .persistent()
            .get(&StorageKey::HeartbeatWindow)
            .unwrap_or(0)
    }

    /// Persist the staleness window in seconds.
    pub fn set_heartbeat_window(env: &Env, secs: u64) {
        env.storage()
            .persistent()
            .set(&StorageKey::HeartbeatWindow, &secs);
    }

    // ── Role scopes ───────────────────────────────────────────────────────────

    /// Scopes granted to `who` (empty when none).
    pub fn get_scopes(env: &Env, who: &Address) -> soroban_sdk::Vec<crate::types::RoleScope> {
        env.storage()
            .persistent()
            .get(&StorageKey::Scopes(who.clone()))
            .unwrap_or(soroban_sdk::Vec::new(env))
    }

    /// Persist the scope set for `who`.
    pub fn set_scopes(
        env: &Env,
        who: &Address,
        scopes: &soroban_sdk::Vec<crate::types::RoleScope>,
    ) {
        env.storage()
            .persistent()
            .set(&StorageKey::Scopes(who.clone()), scopes);
    }

    // ── Guardians ─────────────────────────────────────────────────────────────

    /// Current guardian set (empty when none configured).
    pub fn get_guardians(env: &Env) -> soroban_sdk::Vec<Address> {
        env.storage()
            .persistent()
            .get(&StorageKey::Guardians)
            .unwrap_or(soroban_sdk::Vec::new(env))
    }

    /// Persist the guardian set.
    pub fn set_guardians(env: &Env, guardians: &soroban_sdk::Vec<Address>) {
        env.storage()
            .persistent()
            .set(&StorageKey::Guardians, guardians);
    }

    // ── Emergency admin revocation ────────────────────────────────────────────

    /// Guardian quorum M (0 = unset).
    pub fn get_guardian_threshold(env: &Env) -> u32 {
        env.storage()
            .persistent()
            .get(&StorageKey::GuardianThreshold)
            .unwrap_or(0)
    }

    /// Persist the guardian quorum M.
    pub fn set_guardian_threshold(env: &Env, m: u32) {
        env.storage()
            .persistent()
            .set(&StorageKey::GuardianThreshold, &m);
    }

    /// Whether the admin was revoked via break-glass.
    pub fn is_admin_vacant(env: &Env) -> bool {
        env.storage().persistent().has(&StorageKey::AdminVacant)
    }

    /// Remove the admin and mark the role vacant.
    pub fn vacate_admin(env: &Env) {
        env.storage().persistent().remove(&StorageKey::Admin);
        env.storage().persistent().remove(&StorageKey::PendingAdmin);
        env.storage()
            .persistent()
            .set(&StorageKey::AdminVacant, &true);
    }
}
