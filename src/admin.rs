//! # Admin
//!
//! Role-based access control helpers.  The contract has three privileged roles:
//!
//! | Role           | Storage key                | Capabilities                              |
//! |----------------|----------------------------|-------------------------------------------|
//! | `admin`        | `StorageKey::Admin`        | Propose/accept/renounce admin, rotate relay/guardian, pause/unpause, upgrade, rate-limit config |
//! | `relay_signer` | `StorageKey::RelaySigner`  | Register callbacks, drive status transitions, vote on auto-unpause, set attestation |
//! | `guardian`     | `StorageKey::Guardian`     | Vote on auto-unpause (and future guardian-pause paths) |
//!
//! Both roles are initialised once and can be rotated by the admin.
//!
//! ## Instance-storage size guardrails
//!
//! The admin/relay role records live in instance-tier storage, which is read on
//! (nearly) every call and therefore has the widest blast radius if a single
//! entry grows past Soroban's per-entry size limit.  The role records are fixed
//! size (a single `Address` each), so they cannot grow unboundedly; the helpers
//! below make that invariant explicit and reject any attempt to write an
//! oversized role record with a clear, actionable contract error *before* the
//! raw platform limit is ever reached.
//!
//! ## Admin rate limiting (#75)
//!
//! Privileged admin entry points share a **fixed-window** counter keyed by
//! ledger sequence (not a sliding window). Once `max_per_window` counted calls
//! land inside `[window_start, window_start + window_ledgers)`, further counted
//! calls return [`ContractError::AdminRateLimited`]. When
//! `ledger.sequence() >= window_start + window_ledgers` the counter resets
//! to 1 for the new window.
//!
//! ### Exemptions (never consume budget)
//!
//! - [`crate::SynapseCoreContract::unpause`] — recovery must not be rate-limited
//!   into helplessness after a legitimate pause.
//! - [`crate::SynapseCoreContract::unpause_auto`] — same rationale for auto-pause.
//! - [`crate::SynapseCoreContract::set_admin_rate_limit`] — otherwise a
//!   compromised key could lock the limit and strand operators.
//! - [`crate::SynapseCoreContract::renounce_admin`] — succession must remain
//!   reachable under pressure.
//!
//! Relay-signer and guardian actions are **out of scope** for this limiter.
//!
//! | Role           | Storage key                | Capabilities                              |
//! |----------------|----------------------------|-------------------------------------------|
//! | `admin`        | `StorageKey::Admin`        | Propose/accept/renounce admin, rotate relay/guardian, pause/unpause, upgrade, rate-limit config |
//! | `relay_signer` | `StorageKey::RelaySigner`  | Register callbacks, drive status transitions, vote on auto-unpause, set attestation |
//! | `guardian`     | `StorageKey::Guardian`     | Vote on auto-unpause (and future guardian-pause paths) |
//!

use soroban_sdk::{Address, Env};

use crate::storage::StorageClient;
use crate::types::{AdminRateLimitConfig, AdminRateLimitState, ContractError};

/// Ledgers a signer approval stays valid for (~8 minutes at ~5s/ledger).
pub const RELAY_APPROVAL_WINDOW_LEDGERS: u32 = 100;

/// Maximum serialised size, in bytes, permitted for a single instance-storage
/// role record (`admin` or `relay_signer`).
///
/// Soroban enforces a hard per-entry limit at the platform level; this cap is
/// deliberately

use soroban_sdk::{Address, Env};

use crate::storage::StorageClient;
use crate::types::{AdminRateLimitConfig, AdminRateLimitState, ContractError};

/// Ledgers a signer approval stays valid for (~8 minutes at ~5s/ledger).
pub const RELAY_APPROVAL_WINDOW_LEDGERS: u32 = 100;

/// Maximum serialised size, in bytes, permitted for a single instance-storage
/// role record (`admin` or `relay_signer`).
///
/// Soroban enforces a hard per-entry limit at the platform level; this cap is
/// deliberately well below it so that an over-sized write fails with a clean
/// contract-level rejection rather than a raw platform error mid-operation.
/// A role record is a single `Address`, so this bound is never approached in
/// practice — it exists to make the invariant explicit and auditable.
pub const MAX_ROLE_ENTRY_BYTES: u32 = 256;

pub struct AdminClient;

impl AdminClient {
    /// Require the calling transaction to be authorised by the current admin.
    ///
    /// Returns `Err(ContractError::Unauthorised)` when auth fails.
    /// Does **not** apply rate limiting — use [`Self::require_admin_rate_limited`]
    /// for counted privileged entry points.
    pub fn require_admin(env: &Env) -> Result<Address, ContractError> {
        let admin = StorageClient::get_admin(env)?;
        admin.require_auth();
        Ok(admin)
    }

    /// Like [`Self::require_admin`], then consumes one slot of the admin
    /// rate-limit budget when a limit is configured.
    pub fn require_admin_rate_limited(env: &Env) -> Result<Address, ContractError> {
        let admin = Self::require_admin(env)?;
        Self::consume_admin_rate_limit(env)?;
        Ok(admin)
    }

    /// Consume one unit of the fixed-window admin rate-limit budget.
    ///
    /// No-op when no config has been set (limit disabled by default).
    pub fn consume_admin_rate_limit(env: &Env) -> Result<(), ContractError> {
        let Some(config) = StorageClient::get_admin_rate_limit_config(env) else {
            return Ok(());
        };
        let now = env.ledger().sequence();
        let mut state =
            StorageClient::get_admin_rate_limit_state(env).unwrap_or(AdminRateLimitState {
                window_start: now,
                count: 0,
            });

        // Fixed-window reset: once `now` reaches the exclusive end of the
        // current window, open a fresh window starting at `now`.
        if now >= state.window_start.saturating_add(config.window_ledgers) {
            state.window_start = now;
            state.count = 0;
        }

        if state.count >= config.max_per_window {
            return Err(ContractError::AdminRateLimited);
        }

        state.count = state.count.saturating_add(1);
        StorageClient::set_admin_rate_limit_state(env, &state);
        Ok(())
    }

    /// Validate and persist a new admin rate-limit configuration.
    pub fn set_rate_limit_config(
        env: &Env,
        max_per_window: u32,
        window_ledgers: u32,
    ) -> Result<AdminRateLimitConfig, ContractError> {
        if max_per_window == 0 || window_ledgers == 0 {
            return Err(ContractError::InvalidRateLimitConfig);
        }
        let config = AdminRateLimitConfig {
            max_per_window,
            window_ledgers,
        };
        StorageClient::set_admin_rate_limit_config(env, &config);
        // Reset counter so the new config takes effect immediately.
        StorageClient::set_admin_rate_limit_state(
            env,
            &AdminRateLimitState {
                window_start: env.ledger().sequence(),
                count: 0,
            },
        );
        Ok(config)
    }

    /// Check `nonce` against `addr`'s next expected nonce and increment it.
    ///
    /// Nonces are strictly sequential per address: only the exact next value
    /// is accepted (no gaps, no reuse). Any other value fails with
    /// [`ContractError::InvalidNonce`].
    pub fn consume_nonce(env: &Env, addr: &Address, nonce: u64) -> Result<(), ContractError> {
        let expected = StorageClient::get_nonce(env, addr);
        if nonce != expected {
            return Err(ContractError::InvalidNonce);
        }
        StorageClient::set_nonce(env, addr, expected + 1);
        Ok(())
    }

    /// Assert that `caller` is either the admin or the trusted relay signer.
    ///
    /// Used by status-transition methods which are callable by both roles.
    pub fn assert_is_relay_or_admin(env: &Env, caller: &Address) -> Result<(), ContractError> {
        let admin = StorageClient::get_admin(env)?;
        if caller == &admin {
            caller.require_auth();
            return Ok(());
        }
        let set = StorageClient::get_relay_signer_set(env)?;
        if !set.signers.contains(caller) {
            return Err(ContractError::Unauthorised);
        }
        Self::require_relay_quorum(env, Some(caller))
    }

    /// Like [`Self::assert_is_relay_or_admin`], but honours a per-transaction
    /// signer binding: when `assigned` is `Some`, the caller must be that
    /// signer (or the admin) rather than the global relay signer.
    pub fn assert_can_drive_tx(
        env: &Env,
        caller: &Address,
        assigned: &Option<Address>,
    ) -> Result<(), ContractError> {
        match assigned {
            None => Self::assert_is_relay_or_admin(env, caller),
            Some(signer) => {
                let admin = StorageClient::get_admin(env)?;
                if caller != &admin && caller != signer {
                    return Err(ContractError::Unauthorised);
                }
                caller.require_auth();
                Ok(())
            }
        }
    }

    /// Assert that `caller` is specifically the relay signer (not the admin).
    ///
    /// Used by `register_callback` — only the relay may ingest callbacks.
    pub fn require_relay_signer(env: &Env, caller: &Address) -> Result<(), ContractError> {
        let set = StorageClient::get_relay_signer_set(env)?;
        if !set.signers.contains(caller) {
            return Err(ContractError::NotRelaySigner);
        }
        Self::require_relay_quorum(env, Some(caller))
    }

    /// Require `threshold`-of-`signers` authorisation (multi-invocation quorum).
    ///
    /// `caller` (if given) counts as one authorised signer via `require_auth`;
    /// every other signer must have registered a fresh approval through
    /// `approve_relay_call` within [`RELAY_APPROVAL_WINDOW_LEDGERS`]. With a
    /// single-signer set and no caller (legacy `register_callback`), that
    /// signer's own auth is required directly. Approvals are consumed.
    pub fn require_relay_quorum(env: &Env, caller: Option<&Address>) -> Result<(), ContractError> {
        let set = StorageClient::get_relay_signer_set(env)?;
        match caller {
            Some(c) => c.require_auth(),
            None if set.signers.len() == 1 => {
                set.signers.get(0).ok_or(ContractError::NotInitialised)?.require_auth();
                return Ok(());
            }
            None => {}
        }
        let now = env.ledger().sequence();
        let mut count = 0u32;
        for s in set.signers.iter() {
            let is_caller = caller == Some(&s);
            let approved = StorageClient::get_relay_approval(env, &s)
                .map(|l| now.saturating_sub(l) <= RELAY_APPROVAL_WINDOW_LEDGERS)
                .unwrap_or(false);
            if is_caller || approved {
                count += 1;
            }
        }
        if count < set.threshold {
            return Err(ContractError::QuorumNotMet);
        }
        for s in set.signers.iter() {
            StorageClient::clear_relay_approval(env, &s);
        }
        Ok(())
    }

    /// Reject `signer` with [`ContractError::SignerQuarantined`] when its last
    /// heartbeat is older than the configured window.
    ///
    /// A signer that has never heartbeated, or a window of `0`, is never
    /// quarantined (backward compatible). Staleness is strict: a signer is
    /// quarantined only when `now - last_heartbeat > window`.
    pub fn assert_not_quarantined(env: &Env, signer: &Address) -> Result<(), ContractError> {
        let window = StorageClient::get_heartbeat_window(env);
        if window == 0 {
            return Ok(());
        }
        if let Some(last) = StorageClient::get_last_heartbeat(env, signer) {
            if env.ledger().timestamp().saturating_sub(last) > window {
                return Err(ContractError::SignerQuarantined);
            }
        }
        Ok(())
    }

    /// Assert that `caller` holds `scope`, then require its auth.
    ///
    /// The admin and relay signer implicitly hold all scopes. Any other caller
    /// lacking the scope gets the same [`ContractError::Unauthorised`] as an
    /// unknown address, so error output does not reveal which addresses hold
    /// other scopes.
    pub fn require_scope(
        env: &Env,
        caller: &Address,
        scope: crate::types::RoleScope,
    ) -> Result<(), ContractError> {
        let admin = StorageClient::get_admin(env)?;
        let relay = StorageClient::get_relay_signer(env)?;
        if caller != &admin
            && caller != &relay
            && !StorageClient::get_scopes(env, caller).contains(scope)
        {
            return Err(ContractError::Unauthorised);
        }
        caller.require_auth();
        Ok(())
    }

    /// Assert that `caller` is the configured guardian.
    #[allow(dead_code)]
    pub fn require_guardian(env: &Env, caller: &Address) -> Result<(), ContractError> {
        let guardian = StorageClient::get_guardian(env).ok_or(ContractError::GuardianNotSet)?;
        if caller != &guardian {
            return Err(ContractError::Unauthorised);
        }
        caller.require_auth();
        Ok(())
    }

    /// Guard an instance-storage role record against exceeding
    /// [`MAX_ROLE_ENTRY_BYTES`].
    ///
    /// `structure` names the aggregate being written (e.g. `"admin"` or
    /// `"relay_signer"`) so the rejection error identifies exactly which
    /// structure and limit was hit.  Returns
    /// `Err(ContractError::InstanceEntryTooLarge)` when the serialised record
    /// would exceed the cap, failing safely well before Soroban's raw
    /// per-entry platform limit.
    pub fn assert_role_entry_fits(
        env: &Env,
        structure: &str,
        value: &Address,
    ) -> Result<(), ContractError> {
        let size = value.to_xdr(env).len();
        if size > MAX_ROLE_ENTRY_BYTES {
            let _ = structure;
            return Err(ContractError::InstanceEntryTooLarge);
        }
        Ok(())
    }
}
