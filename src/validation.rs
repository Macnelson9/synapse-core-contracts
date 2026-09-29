//! # Validation
//!
//! Stateless guards that reject malformed inputs before they touch storage.
//! Each function is pure (takes `&Env` only when needed for host string ops) and
//! maps directly to a [`ContractError`] variant.
//!
//! ## String-length caps
//!
//! Several `String` fields have maximum-length caps to prevent unbounded
//! storage-rent costs.  These caps are enforced here and referenced by the
//! [`cost model`](../COST_MODEL.md#6-string-length-cap-impact).
//!
//! ## Stellar G-address validation (SEP-23 strkey)
//!
//! **Decision (accepted): full base32-decode + CRC16-XModem checksum.**
//!
//! A cheap `len == 56 && starts_with('G')` check lets single-character typos
//! that still look well-shaped through, silently registering a transaction
//! against an address that corresponds to no real account.  That residual risk
//! is unacceptable for an on-chain registry that keys deposits to a destination.
//!
//! Cost of the full check inside this `#![no_std]`, `opt-level = "z"` contract
//! (measured on the release wasm artefact — see [`DECISIONS.md`](../DECISIONS.md)
//! § Strkey CRC16):
//!
//! | Metric | Approx. impact |
//! |--------|----------------|
//! | Algorithm | 56× base32 nibble + CRC16 over 33 bytes |
//! | Estimated CPU instructions | low thousands (≪ one storage write) |
//! | Release WASM size delta | on the order of 1–2 KiB |
//! | Fee impact | negligible vs ~0.01 XLM `register_callback` write cost |
//!
//! Implementation is hand-rolled (no external crate) to stay within the
//! no_std / wasm-size budget.  See fixtures in the unit tests below.

use soroban_sdk::{Address, Env, String};

use crate::types::{CallbackPayload, ContractError};

/// Maximum length (in bytes) for a `transaction_id` field (UUIDv4 is 36 chars).
const MAX_TX_ID_LEN: u32 = 64;
/// Maximum length for `anchor_transaction_id` (opaque AP ID, typically ≤ 36).
const MAX_ANCHOR_TX_ID_LEN: u32 = 64;
/// Maximum length for `callback_status` (short code, e.g. "pending_external").
const MAX_CALLBACK_STATUS_LEN: u32 = 32;
/// Maximum length for `stellar_tx_hash` (SHA-256 hex is 64 chars).
const MAX_STELLAR_TX_HASH_LEN: u32 = 72;
/// Maximum length for `failure_reason` (short human-readable code).
const MAX_FAILURE_REASON_LEN: u32 = 64;
/// Maximum length of a single transaction tag.
pub const MAX_TAG_LEN: u32 = 32;
/// Maximum number of tags per transaction.
pub const MAX_TAGS_PER_TX: u32 = 8;

/// Encoded ed25519 public-key strkey length (SEP-23).
const STRKEY_ENCODED_LEN: u32 = 56;
/// Decoded length: 1 version + 32 pubkey + 2 CRC16 = 35 bytes.
const STRKEY_DECODED_LEN: usize = 35;
/// Version byte for ed25519 public keys (`6 << 3`), base32-encodes to `'G'`.
const VERSION_ED25519_PUBLIC_KEY: u8 = 6 << 3;
/// CRC16-XModem polynomial \(x^{16} + x^{12} + x^{5} + 1\).
const CRC16_XMODEM_POLY: u16 = 0x1021;

/// Generic helper: reject a `String` if its byte length exceeds `max`.
fn enforce_max_length(field: &String, max: u32) -> Result<(), ContractError> {
    if field.len() > max {
        return Err(ContractError::StringTooLong);
    }
    Ok(())
}

/// Map a single RFC 4648 base32 alphabet character to its 5-bit value.
#[inline]
fn base32_value(c: u8) -> Result<u8, ()> {
    match c {
        b'A'..=b'Z' => Ok(c - b'A'),
        b'2'..=b'7' => Ok(c - b'2' + 26),
        _ => Err(()),
    }
}

/// Decode an unpadded 56-character SEP-23 strkey into 35 raw bytes.
fn base32_decode_strkey(
    encoded: &[u8; STRKEY_ENCODED_LEN as usize],
) -> Result<[u8; STRKEY_DECODED_LEN], ()> {
    let mut out = [0u8; STRKEY_DECODED_LEN];
    let mut bit_buffer: u32 = 0;
    let mut bits_in_buffer: u32 = 0;
    let mut out_idx = 0usize;

    for &c in encoded {
        let val = base32_value(c)? as u32;
        bit_buffer = (bit_buffer << 5) | val;
        bits_in_buffer += 5;
        if bits_in_buffer >= 8 {
            bits_in_buffer -= 8;
            out[out_idx] = ((bit_buffer >> bits_in_buffer) & 0xff) as u8;
            out_idx += 1;
        }
    }

    // 56 × 5 bits = 280 bits = exactly 35 bytes; leftover must be zero.
    if out_idx != STRKEY_DECODED_LEN || bits_in_buffer != 0 {
        return Err(());
    }
    Ok(out)
}

/// CRC16-XModem (init 0, no final XOR) used by Stellar strkeys.
fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            if crc & 0x8000 != 0 {
                crc = (crc << 1) ^ CRC16_XMODEM_POLY;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

/// Verify a Stellar ed25519 public-key strkey (G-address) per SEP-23.
///
/// Steps: length → base32 decode → version byte → CRC16-XModem (little-endian).
fn verify_ed25519_public_key_strkey(encoded: &[u8; STRKEY_ENCODED_LEN as usize]) -> Result<(), ()> {
    let decoded = base32_decode_strkey(encoded)?;
    if decoded[0] != VERSION_ED25519_PUBLIC_KEY {
        return Err(());
    }
    let payload = &decoded[..STRKEY_DECODED_LEN - 2];
    let expected = u16::from_le_bytes([
        decoded[STRKEY_DECODED_LEN - 2],
        decoded[STRKEY_DECODED_LEN - 1],
    ]);
    if crc16_xmodem(payload) != expected {
        return Err(());
    }
    Ok(())
}

pub struct Validator;

impl Validator {
    /// Validate an incoming [`CallbackPayload`] before writing to ledger.
    ///
    /// Runs every sub-check and returns the first error encountered.
    pub fn validate_payload(env: &Env, payload: &CallbackPayload) -> Result<(), ContractError> {
        Self::validate_stellar_account(env, &payload.stellar_account)?;
        Self::validate_amount(payload.amount)?;
        Self::validate_amount_ceiling(env, &payload.asset_issuer, payload.amount)?;
        Self::validate_asset_code(env, &payload.asset_code)?;
        Self::validate_asset_issuer(env, &payload.asset_issuer)?;
        Self::validate_idempotency_key(env, &payload.idempotency_key)?;
        Self::validate_transaction_id(&payload.transaction_id)?;
        Self::validate_anchor_transaction_id(&payload.anchor_transaction_id)?;
        Self::validate_callback_status(&payload.callback_status)?;
        Ok(())
    }

    /// Reject amounts above the per-anchor ceiling (anchor = `asset_issuer`);
    /// anchors without an explicit entry use the contract-wide default.
    /// Exactly-at-ceiling is allowed.
    pub fn validate_amount_ceiling(
        env: &Env,
        anchor: &String,
        amount: i128,
    ) -> Result<(), ContractError> {
        if amount > crate::storage::StorageClient::get_amount_ceiling(env, anchor) {
            return Err(ContractError::AmountCeilingExceeded);
        }
        Ok(())
    }

    /// Stellar G-address: SEP-23 ed25519 public-key strkey with CRC16 checksum.
    ///
    /// Rejects wrong length/prefix, invalid base32, wrong version byte, and
    /// checksum mismatches (including well-shaped single-character typos).
    pub fn validate_stellar_account(_env: &Env, account: &String) -> Result<(), ContractError> {
        if account.len() != STRKEY_ENCODED_LEN {
            return Err(ContractError::InvalidStellarAccount);
        }
        let mut buf = [0u8; STRKEY_ENCODED_LEN as usize];
        account.copy_into_slice(&mut buf);
        verify_ed25519_public_key_strkey(&buf).map_err(|_| ContractError::InvalidStellarAccount)
    }

    /// Amount must be strictly positive (> 0 stroops).
    pub fn validate_amount(amount: i128) -> Result<(), ContractError> {
        if amount <= 0 {
            return Err(ContractError::InvalidAmount);
        }
        Ok(())
    }

    /// Asset code: 1–12 ASCII uppercase characters (SEP-11).
    pub fn validate_asset_code(_env: &Env, code: &String) -> Result<(), ContractError> {
        let len = code.len() as usize;
        if len == 0 || len > 12 {
            return Err(ContractError::InvalidAssetCode);
        }
        let mut buf = [0u8; 12];
        code.copy_into_slice(&mut buf[..len]);
        for &byte in &buf[..len] {
            if !byte.is_ascii_uppercase() {
                return Err(ContractError::InvalidAssetCode);
            }
        }
        Ok(())
    }

    /// Issuer: must be a valid G-address (same rule as stellar_account).
    pub fn validate_asset_issuer(env: &Env, issuer: &String) -> Result<(), ContractError> {
        Self::validate_stellar_account(env, issuer).map_err(|_| ContractError::InvalidAssetIssuer)
    }

    /// Idempotency key: non-empty (off-chain enforces UUID format).
    pub fn validate_idempotency_key(_env: &Env, key: &String) -> Result<(), ContractError> {
        if key.is_empty() {
            return Err(ContractError::MissingIdempotencyKey);
        }
        Ok(())
    }

    /// Transaction ID: UUID format expected; max length enforced for rent cost
    /// control.
    pub fn validate_transaction_id(id: &String) -> Result<(), ContractError> {
        enforce_max_length(id, MAX_TX_ID_LEN)
    }

    /// Anchor transaction ID: max length enforced for rent cost control.
    pub fn validate_anchor_transaction_id(id: &String) -> Result<(), ContractError> {
        enforce_max_length(id, MAX_ANCHOR_TX_ID_LEN)
    }

    /// Callback status: max length enforced for rent cost control.
    pub fn validate_callback_status(status: &String) -> Result<(), ContractError> {
        enforce_max_length(status, MAX_CALLBACK_STATUS_LEN)
    }

    /// Stellar transaction hash: max length enforced for rent cost control.
    pub fn validate_stellar_tx_hash(hash: &String) -> Result<(), ContractError> {
        enforce_max_length(hash, MAX_STELLAR_TX_HASH_LEN)
    }

    /// Failure reason: max length enforced for rent cost control.
    pub fn validate_failure_reason(reason: &String) -> Result<(), ContractError> {
        enforce_max_length(reason, MAX_FAILURE_REASON_LEN)
    }

    /// Tag: non-empty, length-capped, and the tx must have room for one more.
    pub fn validate_tag(tag: &String, existing_count: u32) -> Result<(), ContractError> {
        if tag.len() == 0 {
            return Err(ContractError::EmptyTag);
        }
        enforce_max_length(tag, MAX_TAG_LEN)?;
        if existing_count >= MAX_TAGS_PER_TX {
            return Err(ContractError::TooManyTags);
        }
        Ok(())
    }

    /// Partial settlement: require `0 < settled < original`.
    ///
    /// Equal amounts must use `complete_transaction`; greater amounts are a
    /// different bug class and are rejected too.
    pub fn validate_settled_amount(settled: i128, original: i128) -> Result<(), ContractError> {
        if settled <= 0 || settled >= original {
            return Err(ContractError::InvalidSettledAmount);
        }
        Ok(())
    }

    /// Reject nominating the contract's own address as the next admin.
    ///
    /// Soroban has no "zero address" sentinel the way EVM chains do — any
    /// well-formed G-address is indistinguishable on-chain from one whose
    /// private key has been lost, so that class of mistake cannot be guarded
    /// against here. What *is* checkable is the contract's own address: it
    /// cannot practically sign a transaction to call `accept_admin`, so
    /// nominating it would permanently brick every admin-gated operation
    /// (THREAT_MODEL.md finding F-02).
    pub fn validate_admin_nominee(env: &Env, nominee: &Address) -> Result<(), ContractError> {
        if *nominee == env.current_contract_address() {
            return Err(ContractError::InvalidAdminNominee);
        }
        Ok(())
    }
}

// ─── Unit tests & fixtures ────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use soroban_sdk::Env;

    // Fixtures — real SEP-23 shapes, not synthetic "G" + filler.
    //
    // VALID: canonical ed25519 public-key example from SEP-23.
    const FIXTURE_VALID: &str = "GA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVSGZ";
    // CHECKSUM-INVALID: same length + `G` prefix, one character corrupted so
    // CRC16 fails (would pass a cheap prefix/length check).
    const FIXTURE_CHECKSUM_INVALID: &str =
        "GA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVSGY";
    // MALFORMED: wrong length (genuinely not a strkey shape).
    const FIXTURE_MALFORMED_SHORT: &str = "GSHORT";
    // MALFORMED: invalid base32 alphabet character (`0` is not in RFC 4648).
    const FIXTURE_MALFORMED_ALPHABET: &str =
        "GAAAAAAAAAAAAAAA0AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    // MALFORMED: correct length but wrong version prefix (`C` = contract).
    const FIXTURE_WRONG_PREFIX: &str = "CA7QYNF7SOWQ3GLR2BGMZEHXAVIRZA4KVWLTJJFC7MGXUA74P7UJVSGZ";

    #[test]
    fn fixture_valid_address_passes() {
        let env = Env::default();
        let account = String::from_str(&env, FIXTURE_VALID);
        assert_eq!(Validator::validate_stellar_account(&env, &account), Ok(()));
    }

    #[test]
    fn fixture_checksum_invalid_is_rejected() {
        let env = Env::default();
        // Sanity: still looks like a cheap-check G-address.
        assert_eq!(FIXTURE_CHECKSUM_INVALID.len(), 56);
        assert!(FIXTURE_CHECKSUM_INVALID.starts_with('G'));

        let account = String::from_str(&env, FIXTURE_CHECKSUM_INVALID);
        assert_eq!(
            Validator::validate_stellar_account(&env, &account),
            Err(ContractError::InvalidStellarAccount)
        );
    }

    #[test]
    fn fixture_malformed_short_is_rejected() {
        let env = Env::default();
        let account = String::from_str(&env, FIXTURE_MALFORMED_SHORT);
        assert_eq!(
            Validator::validate_stellar_account(&env, &account),
            Err(ContractError::InvalidStellarAccount)
        );
    }

    #[test]
    fn fixture_malformed_alphabet_is_rejected() {
        let env = Env::default();
        assert_eq!(FIXTURE_MALFORMED_ALPHABET.len(), 56);
        let account = String::from_str(&env, FIXTURE_MALFORMED_ALPHABET);
        assert_eq!(
            Validator::validate_stellar_account(&env, &account),
            Err(ContractError::InvalidStellarAccount)
        );
    }

    #[test]
    fn fixture_wrong_prefix_is_rejected() {
        let env = Env::default();
        assert_eq!(FIXTURE_WRONG_PREFIX.len(), 56);
        let account = String::from_str(&env, FIXTURE_WRONG_PREFIX);
        assert_eq!(
            Validator::validate_stellar_account(&env, &account),
            Err(ContractError::InvalidStellarAccount)
        );
    }

    #[test]
    fn issuer_maps_checksum_failure_to_invalid_asset_issuer() {
        let env = Env::default();
        let issuer = String::from_str(&env, FIXTURE_CHECKSUM_INVALID);
        assert_eq!(
            Validator::validate_asset_issuer(&env, &issuer),
            Err(ContractError::InvalidAssetIssuer)
        );
    }

    #[test]
    fn crc16_matches_known_sep23_vector() {
        let mut buf = [0u8; 56];
        buf.copy_from_slice(FIXTURE_VALID.as_bytes());
        assert!(verify_ed25519_public_key_strkey(&buf).is_ok());
    }
}
