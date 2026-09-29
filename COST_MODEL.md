# Cost Model — Per-Transaction XLM Estimate

> **Date:** 2025-Q2  
> **Target:** Mainnet (Soroban Protocol 22)  
> **Contract:** `synapse-core-contract` (Phase 1, on-chain transaction registry)

---

## 1. Scope

This document estimates the **per-transaction XLM cost** of the contract's core
lifecycle.  The estimate covers:

- Initial write of a `Transaction` record (persistent storage)
- Initial write of an idempotency key (temporary storage)
- Read + TTL-extension for each status transition
- Occasional off-chain polling reads

It does **not** cover the one-time `initialize` call, admin operations (pause,
transfer, upgrade), or the "dead entry" cost of expired idempotency keys that
the ledger evicts for free.

---

## 2. Storage Footprint

### 2.1 Persistent entry — `Transaction` record

Key: `StorageKey::Transaction(tx_id)` where `tx_id` is a UUID (36 chars).

| Field                  | Type             | XDR size | Notes                                        |
|------------------------|------------------|----------|----------------------------------------------|
| `id`                   | `String`         | 40 B     | UUID: 36 chars + 4-byte length prefix        |
| `stellar_account`      | `String`         | 60 B     | 56-char G-address + 4-byte length prefix     |
| `amount`               | `i128`           | 16 B     | Fixed-width                                  |
| `asset_code`           | `String`         | 16 B     | 12 chars + 4-byte prefix (capped by validator) |
| `asset_issuer`         | `String`         | 60 B     | 56-char G-address + 4-byte prefix            |
| `status`               | `TransactionStatus` | 4 B   | 4-byte enum discriminant                     |
| `created_at_ledger`    | `u32`            | 4 B      |                                              |
| `updated_at_ledger`    | `u32`            | 4 B      |                                              |
| `anchor_transaction_id`| `String`         | 40 B     | ~36 chars (opaque ID) + 4-byte prefix        |
| `callback_type`        | `CallbackType`   | 4 B      | 4-byte enum discriminant                     |
| `callback_status`      | `String`         | 24 B     | ~20 chars + 4-byte prefix                    |
| `stellar_tx_hash`      | `String`         | 4 B      | Empty string initially (4-byte prefix only)  |
| `failure_reason`       | `String`         | 4 B      | Empty string initially (4-byte prefix only)  |
| `tags`                 | `Vec<String>`    | 4 B+     | Empty vec initially; see section 2.1.1       |
| **Struct subtotal**    |                  | **280 B** |                                              |
| Storage key overhead   | `StorageKey` enum| 44 B     | 4-byte discriminant + 40-byte String         |
| XDR framing            |                  | 8 B      | Struct header / padding                      |
| **Total per entry**    |                  | **~332 B** | Rounded to **512 B** for fee calculation    |

> **Note:** The two empty-string fields (`stellar_tx_hash`, `failure_reason`)
> are replaced with populated values during the transaction lifecycle.  Their
> worst-case sizes are 68 B (64-char hex hash + prefix) and 24 B respectively.
> After final status, the entry grows to ~396 B.

#### 2.1.1 Tag storage (append-only, capped)

Tags are capped at 8 per transaction and 32 chars each. Each tag costs
`4 B length prefix + chars` (XDR pads to 4 B), on top of a 4 B vec header:

| Tags | Example content       | Added size |
|------|-----------------------|------------|
| 1    | one ~20-char tag      | ~24 B      |
| 5    | five ~20-char tags    | ~120 B     |
| 8 (max) | eight 32-char tags | ~288 B     |

Even at the cap the record stays within the 512 B fee-rounding bucket only
when other fields are near their typical sizes; budget up to the next bucket
for fully tagged records.

### 2.2 Temporary entry — idempotency key

| Component              | Type             | XDR size | Notes                                   |
|------------------------|------------------|----------|-----------------------------------------|
| Key: `StorageKey::IdempotencyKey(String)` | 44 B | 4-byte discrim + ~40-byte idempotency key string |
| Value                  | `u32`            | 4 B      | Ledger sequence when first stored       |
| **Total per entry**    |                  | **~48 B** | Rounded to **64 B** for fee calculation |

---

## 3. Soroban Fee Rates (Protocol 22)

| Component               | Rate (stroops) | Rate (XLM)        | Notes                                |
|-------------------------|----------------|--------------------|--------------------------------------|
| **Write entry**         | ~50,000        | 0.0050 XLM         | Host function + storage write cost   |
| **Read entry**          | ~5,000         | 0.0005 XLM         | Host function + storage read cost    |
| **Extend TTL call**     | ~10,000        | 0.0010 XLM         | Base fee for `extend_ttl`            |
| **Persistent rent**     | ~1 stroop/byte | per 100,000 ledgers| 100,000 ledgers ≈ 1 week at 5s       |
| **Temporary rent**      | ~0.5 stroop/byte| per 18,000 ledgers| 18,000 ledgers ≈ 24 hours at 5s      |
| **CPU/memory**          | ~10,000        | 0.0010 XLM         | Per-operation resource component     |

*(1 stroop = 0.0000001 XLM; 10,000,000 stroops = 1 XLM)*

---

## 4. Per-Transaction Cost Breakdown

### 4.1 `register_callback` — Initial write

| Operation               | Size   | Write fee | Rent component         | Total (XLM) |
|-------------------------|--------|-----------|------------------------|-------------|
| Write `Transaction`     | 512 B  | 0.0050    | 512 × 1 × 1e-7 = 0.0000512 | 0.00505 |
| Write idempotency key   | 64 B   | 0.0050    | 64 × 0.5 × 1e-7 = 0.0000032 | 0.00500 |
| CPU/memory              |        |           |                        | 0.0010      |
| **Total register**      |        |           |                        | **~0.0111 XLM** |

### 4.2 `start_processing` — Status transition

| Operation               | Size   | Read fee | Extend TTL + rent     | Total (XLM) |
|-------------------------|--------|----------|------------------------|-------------|
| Read `Transaction`      | 512 B  | 0.0005   | 0.0010 + 0.0000512    | 0.00155     |
| CPU/memory              |        |          |                        | 0.0010      |
| **Total start**         |        |          |                        | **~0.0026 XLM** |

### 4.3 `complete_transaction` / `fail_transaction` — Terminal transition

(Same as `start_processing` — one read + one TTL extension)

| Operation               |          | Total (XLM) |
|-------------------------|----------|-------------|
| Read + TTL extension    |          | 0.0026      |
| **Total terminal**      |          | **~0.0026 XLM** |

### 4.4 Off-chain polling (optional, ~3 reads)

| Operation               |          | Total (XLM) |
|-------------------------|----------|-------------|
| 3 × read + TTL extension | 3×0.00155 | 0.00465     |
| CPU/memory              | 3×0.0010  | 0.0030      |
| **Total polling**       |          | **~0.0077 XLM** |

---

## 5. Lifecycle Totals

### 5.1 Minimum lifecycle (no polling)

| Step                  | XLM         |
|-----------------------|-------------|
| `register_callback`   | 0.0111      |
| `start_processing`    | 0.0026      |
| `complete_transaction`| 0.0026      |
| **Total**             | **0.0163 XLM ≈ 0.016 XLM** |

### 5.2 Typical lifecycle (3 off-chain polls)

| Step                  | XLM         |
|-----------------------|-------------|
| `register_callback`   | 0.0111      |
| 3 × status transitions| 0.0078      |
| 3 × polling reads     | 0.0077      |
| **Total**             | **0.0266 XLM ≈ 0.027 XLM** |

### 5.3 Budget for relay operator

| Volume     | Monthly tx | Monthly cost (min) | Monthly cost (typical) |
|------------|------------|--------------------|------------------------|
| Low        | 1,000      | 16 XLM (~$0.20)    | 27 XLM (~$0.33)        |
| Medium     | 10,000     | 160 XLM (~$1.95)   | 270 XLM (~$3.30)       |
| High       | 100,000    | 1,600 XLM (~$19.50)| 2,700 XLM (~$32.95)    |
| Peak       | 1,000,000  | 16,000 XLM (~$195) | 27,000 XLM (~$329)     |

*(XLM price assumed at ~$0.122 per CoinMarketCap 2025-Q2 average)*

---

## 6. String-Length Cap Impact (Enforced)

**String-length caps have been implemented in [`validation.rs`](./src/validation.rs)**
as of this document's publication.  The caps are:

| Field                       | Cap (bytes) | Rationale                                    |
|-----------------------------|-------------|----------------------------------------------|
| `transaction_id`            | 64          | UUIDv4 is 36 chars; generous cushion         |
| `anchor_transaction_id`     | 64          | Opaque AP ID, typically ≤ 36 chars           |
| `callback_status`           | 32          | Short status code, e.g. `pending_external`   |
| `stellar_tx_hash`           | 72          | SHA-256 hex is 64 chars; + prefix overhead   |
| `failure_reason`            | 64          | Short code, e.g. `horizon_timeout`           |

**Without these caps** a malicious relay or bug could store megabyte-sized
strings, driving rent cost to tens of XLM per entry.  The caps close this
attack vector.

### 6.1 Strkey CRC16 validation cost

`stellar_account` / `asset_issuer` are validated with a hand-rolled SEP-23
base32 decode + CRC16-XModem check (see [`DECISIONS.md`](./DECISIONS.md)
§ Strkey CRC16).  Per-call CPU is low thousands of instructions — negligible
beside the ~0.01 XLM storage write for `register_callback`.  Release WASM grew
by **418 bytes** (19 820 → 20 238) under `opt-level = "z"`; fee impact is
dominated by ledger writes, not validation.

With the enforced caps, the **worst-case persistent entry** is:

| Field (worst case)          | Size     |
|-----------------------------|----------|
| `id`                        | 68 B     |
| `stellar_account`           | 60 B     |
| `amount`                    | 16 B     |
| `asset_code`                | 16 B     |
| `asset_issuer`              | 60 B     |
| `status`                    | 4 B      |
| `created_at_ledger`         | 4 B      |
| `updated_at_ledger`         | 4 B      |
| `anchor_transaction_id`     | 68 B     |
| `callback_type`             | 4 B      |
| `callback_status`           | 36 B     |
| `stellar_tx_hash` (capped)  | 76 B     |
| `failure_reason` (capped)   | 68 B     |
| Key + framing               | 52 B     |
| **Total**                   | **~536 B** |

This is near-identical to the unbounded estimate of 512 B because the fixed-
width fields (addresses, amounts, enums) dominate the footprint.  The caps
primarily protect against pathological inputs, not typical usage.

**The cost model in §4–5 reflects this enforced worst case and is final.**

---

## 7. Key Assumptions

1. **Ledger time:** 5 seconds per ledger (Stellar mainnet nominal).
2. **TTL values:** `TRANSACTION_MIN_TTL_LEDGERS = 100,000` (~1 week) and
   `IDEMPOTENCY_TTL_LEDGERS = 18,000` (~24 hours).  These are the constants
   in [`storage.rs`](./src/storage.rs).
3. **Read pattern:** 3 status transitions × 1 read each + 3 off-chain polling
   reads × 1 read each = 6 total reads × TTL extension.
4. **Fee rates:** Based on Soroban Protocol 22 resource model.  These are
    subject to change via Stellar network governance votes (CAPs).
5. **XLM price:** $0.122 (2025-Q2 average).  Actual cost varies with market
    price and network congestion surcharges.
6. **No storage inflation:** The estimate assumes fees remain at baseline;
    during congestion, Soroban uses a fee-auction model that can raise prices
    10× or more.

---

## 8. Summary

| Metric                    | Value              |
|---------------------------|--------------------|
| Cost per tx (min lifecycle)| **0.016 XLM**      |
| Cost per tx (typical)     | **0.027 XLM**      |
| Monthly cost at 10K tx    | **160–270 XLM**    |
| Primary cost driver       | Persistent write + rent for `Transaction` record |

The cost is dominated by the **initial write of the Transaction record**
(~40% of lifecycle cost).  Each subsequent read + TTL extension is relatively
cheap.  The relay operator should budget for the **typical lifecycle** to
account for off-chain monitoring reads.

**String-length caps have been implemented** in [`validation.rs`](./src/validation.rs)
(§6).  Without them, a malicious relay could drive costs to tens of XLM per
entry via megabyte-sized strings.  The caps close this vector with negligible
impact on typical usage.


## Batch registration (`batch_register_callback`)

Batch size is capped at `MAX_BATCH_SIZE` (20). Worst case is 20 payloads with
max-length string fields: each payload costs one persistent write for the
transaction (~512 B fee-rounded, see section 2), one temporary write for the
idempotency key, one status-index update, and one event, plus a single
`EventBatchProcessed`. Validation runs over the whole batch before any write,
including an O(n^2) in-batch duplicate check (at most 190 comparisons). The
cap is deliberately conservative to stay well under the per-transaction
resource limits; raise it only after benchmarking.
