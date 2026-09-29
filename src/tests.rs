//! Contract tests for `synapse-core-contract`.
//!
//! Run with:
//! ```bash
//! cargo test
//! ```
//!
//! Gated under `#[cfg(test)]` and wired into the crate via `mod tests;` in
//! `lib.rs` (matching the [`crate::test_pause`] convention) rather than moved
//! to a `tests/` integration crate — the package only declares
//! `crate-type = ["cdylib"]`, so an external integration crate would have no
//! `rlib` to link against without changing the release build shape.
//!
//! Each `#[test]` corresponds to one contract entry-point and should cover:
//! * happy path
//! * authorisation failure
//! * invalid input
//! * duplicate / idempotency path
//! * invalid state-machine transition
//!
//! ## Auth testing strategy
//!
//! Most tests use [`setup`], which calls `env.mock_all_auths()` — good enough
//! when a test only needs *some* authorisation to be present. Tests that
//! must prove a specific address's authorisation is required (admin/relay
//! rotation, "auth'd the wrong address" bugs) build the environment manually
//! and use scoped `client.mock_auths(&[MockAuth { .. }])` per call instead;
//! mixing the two modes in one test is avoided since `mock_auths` sets
//! explicit authorization entries that supersede the recording-auth mode.
//!
//! ## `ContractError` coverage
//!
//! Every variant a handler can currently return is exercised below.
//! `ContractError::ContractPaused` is covered in [`crate::test_pause`], not
//! duplicated here. `DuplicateRequest` is exercised by
//! `test_register_callback_rejects_duplicate_transaction_id_after_idempotency_expiry`.
//! `NotRelaySigner` and `StorageError` are declared in [`crate::types`] but no
//! handler returns them today (`AdminClient::require_relay_signer` is unused
//! dead code) — nothing to test until a handler actually produces them.

#![cfg(test)]

use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events, Ledger, MockAuth, MockAuthInvoke},
    Address, Env, IntoVal, String, Symbol, TryFromVal,
};

use crate::types::{CallbackPayload, CallbackType, ContractError, StorageKey, TransactionStatus};
use crate::{SynapseCoreContract, SynapseCoreContractClient};

// ─── Test helpers ─────────────────────────────────────────────────────────────

/// Register + initialise the contract with all auths mocked (happy-path setup).
fn setup() -> (Env, SynapseCoreContractClient<'static>, Address, Address) {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    env.mock_all_auths();
    client.initialize(&admin, &relay);
    (env, client, admin, relay)
}

/// A real SEP-23 ed25519 public-key strkey (checksum-valid) — must pass full
/// CRC16 validation in [`crate::validation::Validator`].
fn g_address(env: &Env) -> String {
    String::from_str(
        env,
        "GA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVSGZ",
    )
}

fn payload_with(env: &Env, tx_id: &str, idem_key: &str) -> CallbackPayload {
    let account = g_address(env);
    CallbackPayload {
        transaction_id: String::from_str(env, tx_id),
        stellar_account: account.clone(),
        amount: 1_000,
        asset_code: String::from_str(env, "USDC"),
        asset_issuer: account,
        idempotency_key: String::from_str(env, idem_key),
        anchor_transaction_id: String::from_str(env, "anchor-1"),
        callback_type: CallbackType::Deposit,
        callback_status: String::from_str(env, "pending_external"),
    }
}

fn default_payload(env: &Env) -> CallbackPayload {
    payload_with(env, "tx-1", "idem-1")
}

/// Assert the last two topics of an event are `synapse`/`name`.
fn assert_topics(
    env: &Env,
    topics: &soroban_sdk::Vec<soroban_sdk::Val>,
    name: soroban_sdk::Symbol,
) {
    assert_eq!(topics.len(), 2);
    let t0 = Symbol::try_from_val(env, &topics.get_unchecked(0)).unwrap();
    let t1 = Symbol::try_from_val(env, &topics.get_unchecked(1)).unwrap();
    assert_eq!(t0, symbol_short!("synapse"));
    assert_eq!(t1, name);
}

// ─── initialize() ─────────────────────────────────────────────────────────────

#[test]
fn test_initialize_happy_path() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);

    assert!(!client.health());
    client.initialize(&admin, &relay);
    assert!(client.health());
}

#[test]
fn test_initialize_rejects_double_init() {
    let (_env, client, admin, relay) = setup();
    let result = client.try_initialize(&admin, &relay);
    assert_eq!(result, Err(Ok(ContractError::AlreadyInitialised)));
}

// ─── register_callback() ──────────────────────────────────────────────────────

#[test]
fn test_register_callback_creates_pending_transaction() {
    let (env, client, _admin, _relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);

    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.status, TransactionStatus::Pending);
    assert_eq!(tx.id, tx_id);
    assert_eq!(tx.amount, 1_000);
    assert_eq!(tx.stellar_tx_hash, String::from_str(&env, ""));
}

#[test]
fn test_register_callback_idempotency_returns_same_id() {
    let (env, client, _admin, _relay) = setup();
    let payload = default_payload(&env);
    let first = client.register_callback(&payload);
    let second = client.register_callback(&payload);
    assert_eq!(first, second);
}

#[test]
fn test_register_callback_rejects_before_initialise() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    env.mock_all_auths();

    let payload = default_payload(&env);
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::NotInitialised)));
}

#[test]
fn test_register_callback_rejects_non_relay_caller() {
    // `register_callback` takes no explicit `caller` argument: it reads the
    // trusted relay from storage and requires *that* address's auth. An
    // untrusted signer therefore fails at the auth layer (a host error), not
    // via a returned `ContractError`.
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    client.initialize(&admin, &relay);

    let attacker = Address::generate(&env);
    let payload = default_payload(&env);

    let result = client
        .mock_auths(&[MockAuth {
            address: &attacker,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "register_callback",
                args: (payload.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .try_register_callback(&payload);
    assert!(result.is_err());
}

#[test]
fn test_register_callback_rejects_invalid_account() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    payload.stellar_account = String::from_str(&env, "GSHORT");
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::InvalidStellarAccount)));
}

#[test]
fn test_register_callback_rejects_zero_amount() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    payload.amount = 0;
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::InvalidAmount)));
}

#[test]
fn test_register_callback_rejects_invalid_asset_code() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    payload.asset_code = String::from_str(&env, "usdc"); // lowercase: SEP-11 requires uppercase
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::InvalidAssetCode)));
}

#[test]
fn test_register_callback_rejects_empty_asset_code() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    payload.asset_code = String::from_str(&env, "");
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::InvalidAssetCode)));
}

#[test]
fn test_register_callback_accepts_asset_code_at_exact_cap() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    // SEP-11 allows up to 12 uppercase ASCII chars; exactly 12 must be accepted.
    payload.asset_code = String::from_str(&env, "ABCDEFGHIJKL");
    let tx_id = client.register_callback(&payload);
    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.asset_code, String::from_str(&env, "ABCDEFGHIJKL"));
}

#[test]
fn test_register_callback_rejects_asset_code_one_over_cap() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    // 13 uppercase ASCII chars exceeds the 12-char SEP-11 cap.
    payload.asset_code = String::from_str(&env, "ABCDEFGHIJKLM");
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::InvalidAssetCode)));
}

#[test]
fn test_register_callback_rejects_invalid_asset_issuer() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    payload.asset_issuer = String::from_str(&env, "GSHORT");
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::InvalidAssetIssuer)));
}

#[test]
fn test_register_callback_rejects_missing_idempotency_key() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    payload.idempotency_key = String::from_str(&env, "");
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::MissingIdempotencyKey)));
}

#[test]
fn test_register_callback_rejects_string_too_long() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    // MAX_CALLBACK_STATUS_LEN is 32; 40 chars exceeds the cap.
    payload.callback_status = String::from_str(&env, "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx");
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::StringTooLong)));
}

#[test]
fn test_register_callback_accepts_transaction_id_at_exact_cap() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    // MAX_TX_ID_LEN is 64; exactly 64 chars must be accepted.
    payload.transaction_id = String::from_str(&env, &"t".repeat(64));
    payload.idempotency_key = String::from_str(&env, "idem-boundary-tx-id");
    let tx_id = client.register_callback(&payload);
    assert_eq!(tx_id.len(), 64);
}

#[test]
fn test_register_callback_rejects_transaction_id_one_over_cap() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    // MAX_TX_ID_LEN is 64; 65 chars must be rejected.
    payload.transaction_id = String::from_str(&env, &"t".repeat(65));
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::StringTooLong)));
}

#[test]
fn test_register_callback_accepts_anchor_transaction_id_at_exact_cap() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    // MAX_ANCHOR_TX_ID_LEN is 64; exactly 64 chars must be accepted.
    payload.anchor_transaction_id = String::from_str(&env, &"a".repeat(64));
    let tx_id = client.register_callback(&payload);
    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.anchor_transaction_id.len(), 64);
}

#[test]
fn test_register_callback_rejects_anchor_transaction_id_one_over_cap() {
    let (env, client, _admin, _relay) = setup();
    let mut payload = default_payload(&env);
    // MAX_ANCHOR_TX_ID_LEN is 64; 65 chars must be rejected.
    payload.anchor_transaction_id = String::from_str(&env, &"a".repeat(65));
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::StringTooLong)));
}

// ─── start_processing() ───────────────────────────────────────────────────────

#[test]
fn test_start_processing_pending_to_processing() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);

    client.start_processing(&tx_id, &relay);
    assert_eq!(client.get_status(&tx_id), TransactionStatus::Processing);
}

#[test]
fn test_start_processing_rejects_non_pending() {
    let (env, client, admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    client.start_processing(&tx_id, &relay);
    client.complete_transaction(&tx_id, &String::from_str(&env, "hash-1"), &relay);

    // Tx is now Completed; starting it again violates the state machine.
    let result = client.try_start_processing(&tx_id, &admin);
    assert_eq!(result, Err(Ok(ContractError::InvalidStatusTransition)));
}

#[test]
fn test_start_processing_rejects_unrelated_caller() {
    let (env, client, _admin, _relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);

    // `mock_all_auths` approves anyone's `require_auth`, so this exercises
    // the caller-identity check itself, not the auth layer.
    let bystander = Address::generate(&env);
    let result = client.try_start_processing(&tx_id, &bystander);
    assert_eq!(result, Err(Ok(ContractError::Unauthorised)));
}

#[test]
fn test_start_processing_accepts_scoped_relay_auth() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    client.initialize(&admin, &relay);

    let payload = default_payload(&env);
    let tx_id = client
        .mock_auths(&[MockAuth {
            address: &relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "register_callback",
                args: (payload.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .register_callback(&payload);

    client
        .mock_auths(&[MockAuth {
            address: &relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "start_processing",
                args: (tx_id.clone(), relay.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .start_processing(&tx_id, &relay);

    assert_eq!(client.get_status(&tx_id), TransactionStatus::Processing);
}

#[test]
fn test_start_processing_rejects_when_wrong_address_authorised() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    client.initialize(&admin, &relay);

    let payload = default_payload(&env);
    let tx_id = client
        .mock_auths(&[MockAuth {
            address: &relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "register_callback",
                args: (payload.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .register_callback(&payload);

    // `caller` = relay (passes the admin/relay membership check), but the
    // ONLY authorisation supplied belongs to admin, not relay. If the
    // handler ever authorised the wrong address (e.g. always
    // `admin.require_auth()` regardless of `caller`), this would wrongly
    // succeed. It must fail: `caller.require_auth()` needs relay's own
    // signature.
    let result = client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "start_processing",
                args: (tx_id.clone(), relay.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .try_start_processing(&tx_id, &relay);
    assert!(result.is_err());
}

// ─── complete_transaction() ───────────────────────────────────────────────────

#[test]
fn test_complete_transaction_happy_path() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    client.start_processing(&tx_id, &relay);

    let hash = String::from_str(&env, "abc123stellarhash");
    client.complete_transaction(&tx_id, &hash, &relay);

    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.status, TransactionStatus::Completed);
    assert_eq!(tx.stellar_tx_hash, hash);
}

#[test]
fn test_complete_transaction_rejects_string_too_long_hash() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    client.start_processing(&tx_id, &relay);

    // MAX_STELLAR_TX_HASH_LEN is 72; 80 chars exceeds the cap.
    let oversized_hash = String::from_str(&env, &"a".repeat(80));
    let result = client.try_complete_transaction(&tx_id, &oversized_hash, &relay);
    assert_eq!(result, Err(Ok(ContractError::StringTooLong)));
    // Rejected input must not have mutated the transaction.
    assert_eq!(client.get_status(&tx_id), TransactionStatus::Processing);
}

#[test]
fn test_complete_transaction_rejects_pending() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    // Skipping start_processing: tx is still Pending.

    let hash = String::from_str(&env, "abc123stellarhash");
    let result = client.try_complete_transaction(&tx_id, &hash, &relay);
    assert_eq!(result, Err(Ok(ContractError::InvalidStatusTransition)));
}

// ─── fail_transaction() ───────────────────────────────────────────────────────

#[test]
fn test_fail_transaction_from_pending() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);

    let reason = String::from_str(&env, "horizon_timeout");
    client.fail_transaction(&tx_id, &reason, &relay);

    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.status, TransactionStatus::Failed);
    assert_eq!(tx.failure_reason, reason);
}

#[test]
fn test_fail_transaction_rejects_string_too_long_reason() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);

    // MAX_FAILURE_REASON_LEN is 64; 70 chars exceeds the cap.
    let oversized_reason = String::from_str(&env, &"x".repeat(70));
    let result = client.try_fail_transaction(&tx_id, &oversized_reason, &relay);
    assert_eq!(result, Err(Ok(ContractError::StringTooLong)));
    // Rejected input must not have mutated the transaction.
    assert_eq!(client.get_status(&tx_id), TransactionStatus::Pending);
}

#[test]
fn test_fail_transaction_from_processing() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    client.start_processing(&tx_id, &relay);

    let reason = String::from_str(&env, "invalid_account");
    client.fail_transaction(&tx_id, &reason, &relay);
    assert_eq!(client.get_status(&tx_id), TransactionStatus::Failed);
}

#[test]
fn test_fail_transaction_rejects_completed() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    client.start_processing(&tx_id, &relay);
    client.complete_transaction(&tx_id, &String::from_str(&env, "hash-1"), &relay);

    let result = client.try_fail_transaction(&tx_id, &String::from_str(&env, "too_late"), &relay);
    assert_eq!(result, Err(Ok(ContractError::InvalidStatusTransition)));
}

// ─── Read-only queries ─────────────────────────────────────────────────────────

#[test]
fn test_get_transaction_rejects_unknown_id() {
    let (env, client, _admin, _relay) = setup();
    let missing = String::from_str(&env, "does-not-exist");
    let result = client.try_get_transaction(&missing);
    assert!(matches!(
        result,
        Err(Ok(ContractError::TransactionNotFound))
    ));
}

#[test]
fn test_is_duplicate_reflects_idempotency_state() {
    let (env, client, _admin, _relay) = setup();
    let payload = default_payload(&env);
    assert!(!client.is_duplicate(&payload.idempotency_key));
    client.register_callback(&payload);
    assert!(client.is_duplicate(&payload.idempotency_key));
}

#[test]
fn test_admin_and_relay_signer_queries_return_current_addresses() {
    let (_env, client, admin, relay) = setup();
    assert_eq!(client.admin(), admin);
    assert_eq!(client.relay_signer(), relay);
}

#[test]
fn test_admin_query_rejects_before_initialise() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let result = client.try_admin();
    assert_eq!(result, Err(Ok(ContractError::NotInitialised)));
}

#[test]
fn test_admin_and_relay_signer_queries_reflect_rotation() {
    let (_env, client, _admin, _relay) = setup();
    let new_admin = Address::generate(&_env);
    let new_relay = Address::generate(&_env);

    client.propose_admin(&new_admin);
    client.accept_admin(&new_admin);
    assert_eq!(client.admin(), new_admin);

    client.set_relay_signer(&new_relay);
    assert_eq!(client.relay_signer(), new_relay);
}

// ─── Admin ────────────────────────────────────────────────────────────────────

#[test]
fn test_admin_transfer_two_step_happy_path() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    client.initialize(&admin, &relay);

    let new_admin = Address::generate(&env);

    // Step 1: current admin proposes. Not yet effective.
    client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "propose_admin",
                args: (new_admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .propose_admin(&new_admin);
    assert_eq!(client.admin(), admin);
    assert_eq!(client.pending_admin(), Some(new_admin.clone()));

    // Step 2: only the nominee's own auth can finalise it.
    client
        .mock_auths(&[MockAuth {
            address: &new_admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "accept_admin",
                args: (new_admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .accept_admin(&new_admin);
    assert_eq!(client.admin(), new_admin);
    assert_eq!(client.pending_admin(), None);

    let new_relay = Address::generate(&env);

    // The OLD admin can no longer authorise privileged ops.
    let old_admin_attempt = client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_relay_signer",
                args: (new_relay.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .try_set_relay_signer(&new_relay);
    assert!(old_admin_attempt.is_err());

    // The NEW admin can.
    client
        .mock_auths(&[MockAuth {
            address: &new_admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_relay_signer",
                args: (new_relay.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .set_relay_signer(&new_relay);
}

#[test]
fn test_propose_admin_rejects_non_admin() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    client.initialize(&admin, &relay);

    let attacker = Address::generate(&env);
    let new_admin = Address::generate(&env);

    let result = client
        .mock_auths(&[MockAuth {
            address: &attacker,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "propose_admin",
                args: (new_admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .try_propose_admin(&new_admin);
    assert!(result.is_err());
}

#[test]
fn test_propose_admin_rejects_self_nomination() {
    // F-02: nominating the contract's own address would permanently brick
    // every admin-gated operation, since a plain contract address cannot
    // practically call accept_admin back.
    let (_env, client, _admin, _relay) = setup();
    let result = client.try_propose_admin(&client.address);
    assert_eq!(result, Err(Ok(ContractError::InvalidAdminNominee)));
}

#[test]
fn test_accept_admin_rejects_when_none_pending() {
    let (_env, client, _admin, _relay) = setup();
    let someone = Address::generate(&_env);
    let result = client.try_accept_admin(&someone);
    assert_eq!(result, Err(Ok(ContractError::NoPendingAdminTransfer)));
}

#[test]
fn test_accept_admin_rejects_wrong_caller() {
    // caller is a plausible-looking argument, but only the pending nominee's
    // own auth may finalise the transfer — mirrors the same
    // "authorised the wrong address" concern already covered for relay
    // rotation in start_processing's tests.
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    client.initialize(&admin, &relay);

    let new_admin = Address::generate(&env);
    let bystander = Address::generate(&env);

    client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "propose_admin",
                args: (new_admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .propose_admin(&new_admin);

    let result = client
        .mock_auths(&[MockAuth {
            address: &bystander,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "accept_admin",
                args: (bystander.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .try_accept_admin(&bystander);
    assert_eq!(result, Err(Ok(ContractError::Unauthorised)));
    assert_eq!(client.admin(), admin);
}

#[test]
fn test_propose_admin_overwrites_prior_pending_nominee() {
    let (env, client, admin, _relay) = setup();
    let first_nominee = Address::generate(&env);
    let second_nominee = Address::generate(&env);

    client.propose_admin(&first_nominee);
    assert_eq!(client.pending_admin(), Some(first_nominee.clone()));

    client.propose_admin(&second_nominee);
    assert_eq!(client.pending_admin(), Some(second_nominee.clone()));

    // The superseded first nominee can no longer accept.
    let result = client.try_accept_admin(&first_nominee);
    assert_eq!(result, Err(Ok(ContractError::Unauthorised)));
    assert_eq!(client.admin(), admin);
}

#[test]
fn test_set_relay_signer_happy_path() {
    let (env, client, _admin, _relay) = setup();
    let new_relay = Address::generate(&env);
    client.set_relay_signer(&new_relay);

    // Smoke-test: the contract stays usable post-rotation. Precise identity
    // enforcement (old signer locked out, new signer required) is covered by
    // `test_relay_rotation_mid_lifecycle_enforces_new_signer` below, since
    // `mock_all_auths` here would approve any caller and couldn't catch an
    // "authorised the wrong address" bug.
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    assert_eq!(
        client.get_transaction(&tx_id).status,
        TransactionStatus::Pending
    );
}

#[test]
fn test_set_relay_signer_emits_relay_signer_rotated_event() {
    let (env, client, _admin, _relay) = setup();
    let new_relay = Address::generate(&env);
    client.set_relay_signer(&new_relay);

    let events = env.events().all();
    let last = events.get_unchecked(events.len() - 1);
    assert_topics(&env, &last.1, symbol_short!("relay"));
}

#[test]
fn test_relay_rotation_mid_lifecycle_enforces_new_signer() {
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    client.initialize(&admin, &relay);

    let payload = default_payload(&env);
    let tx_id = client
        .mock_auths(&[MockAuth {
            address: &relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "register_callback",
                args: (payload.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .register_callback(&payload);

    client
        .mock_auths(&[MockAuth {
            address: &relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "start_processing",
                args: (tx_id.clone(), relay.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .start_processing(&tx_id, &relay);

    // Admin rotates the relay signer mid-lifecycle (tx is now Processing).
    let new_relay = Address::generate(&env);
    client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_relay_signer",
                args: (new_relay.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .set_relay_signer(&new_relay);

    let hash = String::from_str(&env, "hash-post-rotation");

    // The OLD relay can no longer complete the in-flight transaction.
    let old_relay_attempt = client
        .mock_auths(&[MockAuth {
            address: &relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "complete_transaction",
                args: (tx_id.clone(), hash.clone(), relay.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .try_complete_transaction(&tx_id, &hash, &relay);
    assert!(old_relay_attempt.is_err());

    // The NEW relay can finish it.
    client
        .mock_auths(&[MockAuth {
            address: &new_relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "complete_transaction",
                args: (tx_id.clone(), hash.clone(), new_relay.clone()).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .complete_transaction(&tx_id, &hash, &new_relay);

    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.status, TransactionStatus::Completed);
    assert_eq!(tx.stellar_tx_hash, hash);
}

// ─── Events ───────────────────────────────────────────────────────────────────

#[test]
fn test_register_emits_transaction_registered_event() {
    let (env, client, _admin, _relay) = setup();
    let payload = default_payload(&env);
    client.register_callback(&payload);

    let events = env.events().all();
    let last = events.get_unchecked(events.len() - 1);
    assert_topics(&env, &last.1, symbol_short!("reg"));
}

#[test]
fn test_complete_emits_status_changed_and_completed_events() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    client.start_processing(&tx_id, &relay);

    let hash = String::from_str(&env, "hash-events");
    client.complete_transaction(&tx_id, &hash, &relay);

    // `env.events().all()` reflects only the most recent top-level
    // invocation, so this is exactly what `complete_transaction` published.
    // Per EVENTS.md §4: complete_transaction emits `status` then `done`.
    let events = env.events().all();
    assert_eq!(events.len(), 2);
    let status_evt = events.get_unchecked(0);
    let done_evt = events.get_unchecked(1);
    assert_topics(&env, &status_evt.1, symbol_short!("status"));
    assert_topics(&env, &done_evt.1, symbol_short!("done"));
}

// ─── Idempotency & TTL ─────────────────────────────────────────────────────────

#[test]
fn test_idempotent_replay_emits_no_event() {
    let (env, client, _admin, _relay) = setup();
    let payload = default_payload(&env);
    let first_id = client.register_callback(&payload);

    // `env.events().all()` reflects only the most recent top-level
    // invocation, so any events left over here belong solely to this second,
    // idempotent call.
    let second_id = client.register_callback(&payload);
    let events = env.events().all();

    assert_eq!(first_id, second_id);
    assert_eq!(
        events.len(),
        0,
        "idempotent replay must not emit a `reg` event"
    );
}

#[test]
fn test_idempotency_key_expires_after_ttl() {
    let (env, client, _admin, _relay) = setup();
    let payload = default_payload(&env);
    client.register_callback(&payload);
    assert!(client.is_duplicate(&payload.idempotency_key));

    // Admin/relay-signer/instance entries never have their TTL extended by
    // any handler, so the local test environment's default
    // `min_persistent_entry_ttl` (4_096 ledgers) would otherwise archive the
    // whole contract instance long before we reach the 18_000-ledger
    // idempotency window. Extend those out of the way first so this test
    // isolates idempotency-key expiry specifically.
    env.as_contract(&client.address, || {
        env.storage().instance().extend_ttl(100_000, 100_000);
        env.storage()
            .persistent()
            .extend_ttl(&StorageKey::Admin, 100_000, 100_000);
        env.storage()
            .persistent()
            .extend_ttl(&StorageKey::RelaySigner, 100_000, 100_000);
    });

    // Jump past the ~24h / 18_000-ledger idempotency TTL.
    env.ledger().with_mut(|li| li.sequence_number += 18_001);

    assert!(!client.is_duplicate(&payload.idempotency_key));

    // The idempotency key itself is no longer live, but the transaction_id
    // guard (F-07) is a second, independent line of defence: replaying the
    // exact same payload must still be rejected, not silently re-registered.
    let events_before = env.events().all().len();
    let result = client.try_register_callback(&payload);
    assert_eq!(result, Err(Ok(ContractError::DuplicateRequest)));
    let events_after = env.events().all().len();
    assert_eq!(
        events_after, events_before,
        "a rejected duplicate must not emit a `reg` event"
    );
}

#[test]
fn test_register_callback_rejects_duplicate_transaction_id_after_idempotency_expiry() {
    // F-07: a late replay with a *different* idempotency_key but the same
    // transaction_id must not be deduped by the (now-expired) idempotency
    // key and fall through to overwriting the existing record — it must be
    // rejected by the transaction_id guard instead.
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    client.start_processing(&tx_id, &relay);
    let hash = String::from_str(&env, "hash-before-replay");
    client.complete_transaction(&tx_id, &hash, &relay);

    env.as_contract(&client.address, || {
        env.storage().instance().extend_ttl(100_000, 100_000);
        env.storage()
            .persistent()
            .extend_ttl(&StorageKey::Admin, 100_000, 100_000);
        env.storage()
            .persistent()
            .extend_ttl(&StorageKey::RelaySigner, 100_000, 100_000);
    });
    env.ledger().with_mut(|li| li.sequence_number += 18_001);

    let mut replay = payload_with(&env, "tx-1", "idem-1-replay");
    replay.transaction_id = String::from_str(&env, "tx-1");
    let result = client.try_register_callback(&replay);
    assert_eq!(result, Err(Ok(ContractError::DuplicateRequest)));

    // The original Completed record — including its audit-trail fields —
    // must be untouched by the rejected replay.
    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.status, TransactionStatus::Completed);
    assert_eq!(tx.stellar_tx_hash, hash);
}

// ─── Full lifecycle ────────────────────────────────────────────────────────────

#[test]
fn test_full_lifecycle_pending_to_processing_to_completed() {
    let (env, client, _admin, relay) = setup();
    let payload = default_payload(&env);
    let tx_id = client.register_callback(&payload);
    assert_eq!(client.get_status(&tx_id), TransactionStatus::Pending);

    client.start_processing(&tx_id, &relay);
    assert_eq!(client.get_status(&tx_id), TransactionStatus::Processing);

    let hash = String::from_str(&env, "final-hash");
    client.complete_transaction(&tx_id, &hash, &relay);

    let tx = client.get_transaction(&tx_id);
    assert_eq!(tx.status, TransactionStatus::Completed);
    assert_eq!(tx.stellar_tx_hash, hash);
    assert_eq!(tx.failure_reason, String::from_str(&env, ""));
    assert!(tx.updated_at_ledger >= tx.created_at_ledger);
}

// ─── cancel_transaction ───────────────────────────────────────────────────────

#[test]
fn test_cancel_transaction_by_relay_and_admin() {
    let (env, client, admin, relay) = setup();
    let a = client.register_callback(&payload_with(&env, "tx-c1", "k-c1"));
    let b = client.register_callback(&payload_with(&env, "tx-c2", "k-c2"));
    client.start_processing(&b, &relay);
    let reason = String::from_str(&env, "duplicate");
    client.cancel_transaction(&a, &reason, &relay);
    client.cancel_transaction(&b, &reason, &admin);
    assert_eq!(client.get_status(&a), TransactionStatus::Cancelled);
    assert_eq!(client.get_status(&b), TransactionStatus::Cancelled);
}

#[test]
fn test_cancel_transaction_rejects_stranger_and_illegal_states() {
    let (env, client, _admin, relay) = setup();
    let reason = String::from_str(&env, "r");
    let tx_id = client.register_callback(&payload_with(&env, "tx-c3", "k-c3"));
    let stranger = Address::generate(&env);
    assert!(client.try_cancel_transaction(&tx_id, &reason, &stranger).is_err());

    client.cancel_transaction(&tx_id, &reason, &relay);
    assert_eq!(
        client.try_cancel_transaction(&tx_id, &reason, &relay),
        Err(Ok(ContractError::AlreadyCancelled))
    );

    let done = client.register_callback(&payload_with(&env, "tx-c4", "k-c4"));
    client.start_processing(&done, &relay);
    client.complete_transaction(&done, &String::from_str(&env, "hash-1"), &relay);
    assert_eq!(
        client.try_cancel_transaction(&done, &reason, &relay),
        Err(Ok(ContractError::CannotCancel))
    );

    let failed = client.register_callback(&payload_with(&env, "tx-c5", "k-c5"));
    client.fail_transaction(&failed, &reason, &relay);
    assert_eq!(
        client.try_cancel_transaction(&failed, &reason, &relay),
        Err(Ok(ContractError::CannotCancel))
    );

    let missing = String::from_str(&env, "nope");
    assert_eq!(
        client.try_cancel_transaction(&missing, &reason, &relay),
        Err(Ok(ContractError::TransactionNotFound))
    );
}

// ─── retry_transaction ────────────────────────────────────────────────────────

#[test]
fn test_retry_transaction_cycle_and_limit() {
    let (env, client, _admin, relay) = setup();
    let reason = String::from_str(&env, "horizon_timeout");
    let tx_id = client.register_callback(&payload_with(&env, "tx-r1", "k-r1"));
    for n in 1..=3u32 {
        client.start_processing(&tx_id, &relay);
        client.fail_transaction(&tx_id, &reason, &relay);
        client.retry_transaction(&tx_id, &relay);
        let tx = client.get_transaction(&tx_id);
        assert_eq!(tx.status, TransactionStatus::Pending);
        assert_eq!(tx.retry_count, n);
    }
    client.fail_transaction(&tx_id, &reason, &relay);
    assert_eq!(
        client.try_retry_transaction(&tx_id, &relay),
        Err(Ok(ContractError::RetryLimitExceeded))
    );
}

#[test]
fn test_retry_transaction_rejects_non_failed() {
    let (env, client, _admin, relay) = setup();
    let tx_id = client.register_callback(&payload_with(&env, "tx-r2", "k-r2"));
    assert_eq!(
        client.try_retry_transaction(&tx_id, &relay),
        Err(Ok(ContractError::InvalidStatusTransition))
    );
}

// ─── get_transactions_by_status ───────────────────────────────────────────────

#[test]
fn test_get_transactions_by_status_pagination_and_transitions() {
    let (env, client, _admin, relay) = setup();
    let ids = ["tx-p1", "tx-p2", "tx-p3"];
    for (i, id) in ids.iter().enumerate() {
        let key = ["k-p1", "k-p2", "k-p3"][i];
        client.register_callback(&payload_with(&env, id, key));
    }
    let p = TransactionStatus::Pending;
    let page1 = client.get_transactions_by_status(&p, &0, &2);
    let page2 = client.get_transactions_by_status(&p, &2, &2);
    assert_eq!(page1.len(), 2);
    assert_eq!(page2.len(), 1);
    assert_eq!(client.get_transactions_by_status(&p, &3, &2).len(), 0);

    client.start_processing(&String::from_str(&env, "tx-p1"), &relay);
    assert_eq!(client.get_transactions_by_status(&p, &0, &10).len(), 2);
    let proc = client.get_transactions_by_status(&TransactionStatus::Processing, &0, &10);
    assert_eq!(proc.len(), 1);
}

#[test]
fn test_get_transactions_by_status_rejects_bad_limit() {
    let (_env, client, _admin, _relay) = setup();
    let p = TransactionStatus::Pending;
    assert_eq!(
        client.try_get_transactions_by_status(&p, &0, &0),
        Err(Ok(ContractError::InvalidPageLimit))
    );
    assert_eq!(
        client.try_get_transactions_by_status(&p, &0, &51),
        Err(Ok(ContractError::InvalidPageLimit))
    );
}

// ─── batch_register_callback ──────────────────────────────────────────────────

#[test]
fn test_batch_register_callback_happy_path() {
    let (env, client, _admin, relay) = setup();
    let mut v = soroban_sdk::Vec::new(&env);
    v.push_back(payload_with(&env, "tx-b1", "k-b1"));
    v.push_back(payload_with(&env, "tx-b2", "k-b2"));
    assert_eq!(client.batch_register_callback(&v, &relay), 2);
    assert_eq!(
        client.get_status(&String::from_str(&env, "tx-b2")),
        TransactionStatus::Pending
    );
}

#[test]
fn test_batch_register_callback_is_atomic_and_bounded() {
    let (env, client, _admin, relay) = setup();
    let mut bad = payload_with(&env, "tx-b4", "k-b4");
    bad.amount = 0;
    let mut v = soroban_sdk::Vec::new(&env);
    v.push_back(payload_with(&env, "tx-b3", "k-b3"));
    v.push_back(bad);
    assert!(client.try_batch_register_callback(&v, &relay).is_err());
    assert!(client.try_get_status(&String::from_str(&env, "tx-b3")).is_err());

    // Duplicate of an on-chain id aborts the whole batch.
    client.register_callback(&payload_with(&env, "tx-b5", "k-b5"));
    let mut d = soroban_sdk::Vec::new(&env);
    d.push_back(payload_with(&env, "tx-b6", "k-b6"));
    d.push_back(payload_with(&env, "tx-b5", "k-b7"));
    assert_eq!(
        client.try_batch_register_callback(&d, &relay),
        Err(Ok(ContractError::DuplicateRequest))
    );
    assert!(client.try_get_status(&String::from_str(&env, "tx-b6")).is_err());

    // Empty batch rejected.
    let empty = soroban_sdk::Vec::new(&env);
    assert_eq!(
        client.try_batch_register_callback(&empty, &relay),
        Err(Ok(ContractError::InvalidBatchSize))
    );
}
