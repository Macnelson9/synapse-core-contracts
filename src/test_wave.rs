//! Tests for issues #75–#78: admin rate limiting, renounce_admin,
//! signer attestation, and multi-role auto-unpause.

#![cfg(test)]

use soroban_sdk::{
    symbol_short,
    testutils::{Address as _, Events, Ledger, MockAuth, MockAuthInvoke},
    Address, BytesN, Env, IntoVal, Symbol, TryFromVal,
};

use crate::types::ContractError;
use crate::{SynapseCoreContract, SynapseCoreContractClient};

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

fn assert_topics(env: &Env, topics: &soroban_sdk::Vec<soroban_sdk::Val>, name: Symbol) {
    assert_eq!(topics.len(), 2);
    let t0 = Symbol::try_from_val(env, &topics.get_unchecked(0)).unwrap();
    let t1 = Symbol::try_from_val(env, &topics.get_unchecked(1)).unwrap();
    assert_eq!(t0, symbol_short!("synapse"));
    assert_eq!(t1, name);
}

// ─── #75 Admin rate limiting ──────────────────────────────────────────────────

#[test]
fn test_admin_rate_limit_allows_one_below_max() {
    let (env, client, _admin, _relay) = setup();
    client.set_admin_rate_limit(&2, &100);

    // Two counted calls succeed (exactly at the limit).
    client.pause();
    client.set_relay_signer(&Address::generate(&env));
}

#[test]
fn test_admin_rate_limit_hits_exactly_at_max() {
    let (env, client, _admin, _relay) = setup();
    client.set_admin_rate_limit(&2, &100);

    client.pause();
    client.set_relay_signer(&Address::generate(&env));

    // Third counted call in the same window is rejected.
    let result = client.try_set_relay_signer(&Address::generate(&env));
    assert_eq!(result, Err(Ok(ContractError::AdminRateLimited)));
}

#[test]
fn test_admin_rate_limit_window_reset_re_enables_calls() {
    let (env, client, _admin, _relay) = setup();
    client.set_admin_rate_limit(&1, &10);

    client.pause();
    assert_eq!(
        client.try_set_relay_signer(&Address::generate(&env)),
        Err(Ok(ContractError::AdminRateLimited))
    );

    // Advance past the fixed window boundary (window_start + 10).
    env.ledger().with_mut(|li| {
        li.sequence_number += 10;
    });

    // Fresh window — counted calls work again.
    client.set_relay_signer(&Address::generate(&env));
}

#[test]
fn test_admin_rate_limit_exempts_unpause() {
    let (_env, client, _admin, _relay) = setup();
    client.set_admin_rate_limit(&1, &100);
    client.pause(); // consumes the only slot
                    // unpause is exempt — must still succeed for recovery.
    client.unpause();
    assert!(!client.is_paused());
}

#[test]
fn test_admin_rate_limit_rejects_zero_config() {
    let (_env, client, _admin, _relay) = setup();
    assert_eq!(
        client.try_set_admin_rate_limit(&0, &10),
        Err(Ok(ContractError::InvalidRateLimitConfig))
    );
    assert_eq!(
        client.try_set_admin_rate_limit(&5, &0),
        Err(Ok(ContractError::InvalidRateLimitConfig))
    );
}

// ─── #76 renounce_admin ───────────────────────────────────────────────────────

#[test]
fn test_renounce_admin_fails_without_prior_accept() {
    let (env, client, admin, _relay) = setup();
    let result = client.try_renounce_admin(&admin);
    assert_eq!(result, Err(Ok(ContractError::NoAcceptedSuccessor)));

    // Even with a pending nominee (not yet accepted), renounce is unreachable.
    let nominee = Address::generate(&env);
    client.propose_admin(&nominee);
    assert_eq!(
        client.try_renounce_admin(&admin),
        Err(Ok(ContractError::NoAcceptedSuccessor))
    );
}

#[test]
fn test_renounce_admin_succeeds_after_accept() {
    let (env, client, admin, _relay) = setup();
    let successor = Address::generate(&env);

    client.propose_admin(&successor);
    client.accept_admin(&successor);
    assert_eq!(client.admin(), successor);

    // Live admin cannot burn the only seat.
    assert_eq!(
        client.try_renounce_admin(&successor),
        Err(Ok(ContractError::NoAcceptedSuccessor))
    );

    // Outgoing admin acknowledges step-down once successor is live.
    client.renounce_admin(&admin);
    assert_eq!(client.admin(), successor);
}

#[test]
fn test_renounce_admin_rejects_unrelated_caller() {
    let (env, client, _admin, _relay) = setup();
    let successor = Address::generate(&env);
    let stranger = Address::generate(&env);

    client.propose_admin(&successor);
    client.accept_admin(&successor);

    assert_eq!(
        client.try_renounce_admin(&stranger),
        Err(Ok(ContractError::Unauthorised))
    );
}

// ─── #77 Signer attestation ───────────────────────────────────────────────────

#[test]
fn test_signer_attestation_round_trip_and_event() {
    let (env, client, _admin, relay) = setup();
    let hash = BytesN::from_array(&env, &[0x11u8; 32]);

    client.set_signer_attestation(&hash, &relay);
    // events().all() reflects only the most recent top-level invocation.
    let events = env.events().all();
    assert_eq!(events.len(), 1);
    assert_topics(&env, &events.get_unchecked(0).1, symbol_short!("attest"));

    assert_eq!(client.get_signer_attestation(&relay), Some(hash.clone()));

    // No-op update to the same hash still emits (monitoring must see it).
    client.set_signer_attestation(&hash, &relay);
    let events = env.events().all();
    assert_eq!(events.len(), 1);
    assert_topics(&env, &events.get_unchecked(0).1, symbol_short!("attest"));
}

#[test]
fn test_signer_attestation_rejects_non_relay() {
    let (env, client, admin, _relay) = setup();
    let hash = BytesN::from_array(&env, &[0x22u8; 32]);
    assert_eq!(
        client.try_set_signer_attestation(&hash, &admin),
        Err(Ok(ContractError::NotRelaySigner))
    );
}

#[test]
fn test_signer_attestation_rejects_zero_hash() {
    let (env, client, _admin, relay) = setup();
    let zero = BytesN::from_array(&env, &[0u8; 32]);
    assert_eq!(
        client.try_set_signer_attestation(&zero, &relay),
        Err(Ok(ContractError::EmptyAttestation))
    );
}

// ─── #78 Multi-role auto-unpause ──────────────────────────────────────────────

fn setup_with_guardian() -> (
    Env,
    SynapseCoreContractClient<'static>,
    Address,
    Address,
    Address,
) {
    let (env, client, admin, relay) = setup();
    let guardian = Address::generate(&env);
    client.set_guardian(&guardian);
    (env, client, admin, relay, guardian)
}

#[test]
fn test_auto_unpause_requires_two_of_three() {
    let (_env, client, admin, _relay, _guardian) = setup_with_guardian();
    client.trip_auto_pause(&admin);
    assert!(client.is_paused());
    assert!(client.is_auto_paused());

    // Single vote records but does not clear the pause.
    client.unpause_auto(&admin);
    assert!(client.is_paused());
    assert!(client.is_auto_paused());
}

#[test]
fn test_auto_unpause_admin_plus_guardian() {
    let (env, client, admin, _relay, guardian) = setup_with_guardian();
    client.trip_auto_pause(&admin);

    client.unpause_auto(&admin);
    assert!(client.is_paused());
    client.unpause_auto(&guardian);
    // Check event before any further client calls wipe the log.
    let events = env.events().all();
    assert_eq!(events.len(), 1);
    assert_topics(&env, &events.get_unchecked(0).1, symbol_short!("aunpause"));

    assert!(!client.is_paused());
    assert!(!client.is_auto_paused());
}

#[test]
fn test_auto_unpause_admin_plus_relay() {
    let (_env, client, admin, relay, _guardian) = setup_with_guardian();
    client.trip_auto_pause(&admin);

    client.unpause_auto(&admin);
    assert!(client.is_paused());
    client.unpause_auto(&relay);
    assert!(!client.is_paused());
}

#[test]
fn test_auto_unpause_guardian_plus_relay() {
    let (_env, client, admin, relay, guardian) = setup_with_guardian();
    client.trip_auto_pause(&admin);

    client.unpause_auto(&guardian);
    assert!(client.is_paused());
    client.unpause_auto(&relay);
    assert!(!client.is_paused());
}

#[test]
fn test_manual_unpause_blocked_during_auto_pause() {
    let (_env, client, admin, _relay, _guardian) = setup_with_guardian();
    client.trip_auto_pause(&admin);
    assert_eq!(
        client.try_unpause(),
        Err(Ok(ContractError::RequiresMultiRoleUnpause))
    );
}

#[test]
fn test_manual_pause_still_admin_only_unpause() {
    let (_env, client, _admin, _relay, _guardian) = setup_with_guardian();
    client.pause();
    assert!(!client.is_auto_paused());
    client.unpause();
    assert!(!client.is_paused());
}

#[test]
fn test_unpause_auto_rejects_when_not_auto_paused() {
    let (_env, client, admin, _relay, _guardian) = setup_with_guardian();
    client.pause(); // manual
    assert_eq!(
        client.try_unpause_auto(&admin),
        Err(Ok(ContractError::NotAutoPaused))
    );
}

#[test]
fn test_auto_unpause_pairwise_auth_enforced() {
    // Prove each pairwise combination under scoped MockAuth (not mock_all).
    let env = Env::default();
    let contract_id = env.register(SynapseCoreContract, ());
    let client = SynapseCoreContractClient::new(&env, &contract_id);
    let admin = Address::generate(&env);
    let relay = Address::generate(&env);
    let guardian = Address::generate(&env);
    client.initialize(&admin, &relay);

    client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "set_guardian",
                args: (guardian.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .set_guardian(&guardian);

    client
        .mock_auths(&[MockAuth {
            address: &admin,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "trip_auto_pause",
                args: (admin.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .trip_auto_pause(&admin);

    // Guardian vote alone is not enough — pause stays engaged.
    client
        .mock_auths(&[MockAuth {
            address: &guardian,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "unpause_auto",
                args: (guardian.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .unpause_auto(&guardian);
    assert!(client.is_paused());

    // Relay completes guardian+relay pair.
    client
        .mock_auths(&[MockAuth {
            address: &relay,
            invoke: &MockAuthInvoke {
                contract: &contract_id,
                fn_name: "unpause_auto",
                args: (relay.clone(),).into_val(&env),
                sub_invokes: &[],
            },
        }])
        .unpause_auto(&relay);
    assert!(!client.is_paused());
}

#[test]
fn test_set_guardian_emits_event() {
    let (env, client, _admin, _relay) = setup();
    let guardian = Address::generate(&env);
    client.set_guardian(&guardian);

    let events = env.events().all();
    assert_eq!(events.len(), 1);
    assert_topics(&env, &events.get_unchecked(0).1, symbol_short!("guardian"));

    assert_eq!(client.guardian(), Some(guardian));
}
