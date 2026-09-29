# Deployment Guide

> **Audience:** Relay operator / contract deployer for the Synapse Bridge ecosystem.
> 
> **Tracked Contract IDs:** All production deployments are tracked in [contract-ids.json](./contract-ids.json) with network-specific configuration including RPC endpoints and network passphrases.

---

## Network Configuration Reference

| Network    | RPC Endpoint                      | Network Passphrase                          |
|------------|-----------------------------------|---------------------------------------------|
| testnet    | https://soroban-testnet.stellar.org | Test SDF Network ; September 2015           |
| futurenet  | https://rpc-futurenet.stellar.org   | Test SDF Future Network ; October 2022      |
| mainnet    | https://soroban.stellar.org        | Public Global Stellar Network ; September 2015 |

---

## Prerequisites

- Rust 1.81+ with `wasm32-unknown-unknown` target
- `stellar-cli` ≥ 22.x
- Admin key(s) held by a **multisig or DAO** (see [DECISIONS.md](./DECISIONS.md) for rationale)
- Deployer key with enough XLM for network fees
- Relay signer key generated and secured by the off-chain relay team

```bash
rustup target add wasm32-unknown-unknown
cargo install --locked stellar-cli --features opt
```

---

## Pre-Deployment Checklist

**MUST complete these before any deployment:**
- [ ] Confirm intended admin public key (multisig address)
- [ ] Confirm intended relay_signer public key (off-chain relay service key)
- [ ] Verify the contract compiles and passes all tests: `make check`
- [ ] For a fresh deploy, confirm `health()` would return uninitialized (check source code that `is_initialised()` returns false before `initialize()` is called)
- [ ] For the target network, verify all keys exist and have sufficient funds
- [ ] Update [contract-ids.json](./contract-ids.json) with the target network's admin and relay_signer addresses before deployment

---

## Build

```bash
cargo build --target wasm32-unknown-unknown --release
```

The compiled WASM is at:
```
target/wasm32-unknown-unknown/release/synapse_core_contract.wasm
```

---

## Deploy (per network)

### Testnet
```bash
stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/synapse_core_contract.wasm \
  --source <DEPLOYER_SECRET> \
  --network testnet
```

### Futurenet
```bash
stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/synapse_core_contract.wasm \
  --source <DEPLOYER_SECRET> \
  --network futurenet
```

### Mainnet
```bash
stellar contract deploy \
  --wasm target/wasm32-unknown-unknown/release/synapse_core_contract.wasm \
  --source <DEPLOYER_SECRET> \
  --network mainnet
```

> **Security note:** The deployer key is **not** the admin key.  After
> deployment, the admin key (held by a multisig) must call `initialize()`.
> Never reuse the deployer secret for any other role.
>
> **Important:** After deployment succeeds, immediately record the returned contract ID, deployment timestamp (ISO 8601), and WASM hash in [contract-ids.json](./contract-ids.json) for the target network.

---

## Initialize (immediately after deploy)

### Testnet
```bash
stellar contract invoke \
  --id $(jq -r '.testnet.contract_id' contract-ids.json) \
  --source <ADMIN_SECRET> \
  --network testnet \
  -- \
  initialize \
  --admin $(jq -r '.testnet.admin_address' contract-ids.json) \
  --relay_signer $(jq -r '.testnet.relay_signer_address' contract-ids.json)
```

### Futurenet
```bash
stellar contract invoke \
  --id $(jq -r '.futurenet.contract_id' contract-ids.json) \
  --source <ADMIN_SECRET> \
  --network futurenet \
  -- \
  initialize \
  --admin $(jq -r '.futurenet.admin_address' contract-ids.json) \
  --relay_signer $(jq -r '.futurenet.relay_signer_address' contract-ids.json)
```

### Mainnet
```bash
stellar contract invoke \
  --id $(jq -r '.mainnet.contract_id' contract-ids.json) \
  --source <ADMIN_SECRET> \
  --network mainnet \
  -- \
  initialize \
  --admin $(jq -r '.mainnet.admin_address' contract-ids.json) \
  --relay_signer $(jq -r '.mainnet.relay_signer_address' contract-ids.json)
```

This is a one-time call; it will fail with `AlreadyInitialised` if called
again.

---

## Post-Deployment Smoke Test

**MUST complete this end-to-end smoke test before considering the deployment successful:**

### 1. Verify initialization
```bash
# Replace <network> with your target network (testnet/futurenet/mainnet)
CONTRACT_ID=$(jq -r '.<network>.contract_id' contract-ids.json)
stellar contract invoke \
  --id $CONTRACT_ID \
  --source <ANY_KEY_WITH_XLM> \
  --network <network> \
  -- \
  health
```
Should return `true`, indicating the contract is properly initialized. A fresh, uninitialized contract would return `false`.

### 2. Verify admin and relay_signer on-chain

Cross-check the addresses `initialize()` actually stored against what
`contract-ids.json` claims — don't rely on the recorded value alone:
```bash
stellar contract invoke --id $CONTRACT_ID --source <ANY_KEY_WITH_XLM> --network <network> -- admin
stellar contract invoke --id $CONTRACT_ID --source <ANY_KEY_WITH_XLM> --network <network> -- relay_signer
```
Both must exactly match `contract-ids.json`'s `admin_address` and
`relay_signer_address` for the target network.

### 3. Test register_callback round-trip
Create a minimal test callback payload and call `register_callback` as the relay signer. The payload must include all required fields from the [`CallbackPayload`](./src/types.rs) struct:
```bash
# Example test payload (adjust fields as needed for your network)
stellar contract invoke \
  --id $CONTRACT_ID \
  --source <RELAY_SIGNER_SECRET> \
  --network <network> \
  -- \
  register_callback \
  --payload '{
    "transaction_id": "test-001",
    "stellar_account": "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3EMR25U3DQ3QQV37N6GWDFA3GUM",
    "amount": 1000000,
    "asset_code": "USDC",
    "asset_issuer": "GA5ZSEJYB37JRC5AVCIA5MOP4RHTM335X2KGX3IHOJAPP5RE34K4KZVN",
    "idempotency_key": "idempotency-test-001",
    "anchor_transaction_id": "anchor-test-001",
    "callback_type": "Deposit",
    "callback_status": "pending_external"
  }'
```

### 4. Read back the transaction
```bash
stellar contract invoke \
  --id $CONTRACT_ID \
  --source <ANY_KEY_WITH_XLM> \
  --network <network> \
  -- \
  get_transaction \
  --transaction_id "test-001"
```
Should return the transaction you just created with status `Pending`.

---

## Post-Deployment Checklist

- [ ] Admin key is a **multisig** (≥3-of-5 recommended, see [DECISIONS.md](./DECISIONS.md))
- [ ] Relay signer key is held by the off-chain relay service, **separate** from the admin
- [ ] Off-chain monitoring subscribes to the `(synapse, upgrade)` event to detect unexpected upgrades
- [ ] `stellar_tx_hash` field is capped at 72 B in [validation.rs](./src/validation.rs) 
      (otherwise storage cost is unbounded — see [COST_MODEL.md](./COST_MODEL.md))
- [ ] [contract-ids.json](./contract-ids.json) is fully updated with all deployment metadata
- [ ] Smoke test completed successfully
- [ ] Commit and push the updated `contract-ids.json` to the main branch

---

## Redeploy & Upgrade Strategy

**Decision:** We use **in-place upgrades** as the primary strategy for contract updates. This approach was chosen for its ability to maintain existing state while fixing bugs or adding features, avoiding the complexity of data migration between new contract instances. See [DECISIONS.md](./DECISIONS.md) §7 for full rationale.

### Redeploy Process (Fresh Deploy)
If you need to deploy a **new, separate instance** of the contract (not upgrade an existing one):
1. Choose a new network if deploying alongside an existing instance, or coordinate a full network cutover
2. Follow the full Pre-Deployment → Build → Deploy → Initialize → Smoke Test process above
3. Update [contract-ids.json](./contract-ids.json) with the new contract ID (archive the old one for historical reference)
4. Communicate the new contract ID to all downstream teams (relay, Phase 2)

### In-Place Upgrade (Existing Contract)
If a bug fix or state-machine extension is required for an existing deployment:

```bash
# 1. Build the new WASM
cargo build --target wasm32-unknown-unknown --release

# 2. Upload the WASM and get its hash (replace <network> with your target network)
NETWORK=<network>
CONTRACT_ID=$(jq -r ".${NETWORK}.contract_id" contract-ids.json)
stellar contract upload \
  --wasm target/wasm32-unknown-unknown/release/synapse_core_contract.wasm \
  --source <ADMIN_SECRET> \
  --network $NETWORK

# 3. Confirm the on-chain schema version you're about to pass matches
stellar contract invoke --id $CONTRACT_ID --source <ANY_KEY_WITH_XLM> --network $NETWORK -- schema_version

# 4. Invoke upgrade (replace <NEW_WASM_HASH> with the hash from step 2, and
#    <EXPECTED_SCHEMA_VERSION> with the value read back in step 3 — a
#    mismatch is rejected before contract WASM is touched)
stellar contract invoke \
  --id $CONTRACT_ID \
  --source <ADMIN_SECRET> \
  --network $NETWORK \
  -- \
  upgrade \
  --new_wasm_hash <NEW_WASM_HASH> \
  --expected_schema_version <EXPECTED_SCHEMA_VERSION>

# 5. Update contract-ids.json with the new wasm_hash
# 6. Run the full post-deployment smoke test to verify the upgrade
```

---

## Cost Estimates

See **[COST_MODEL.md](./COST_MODEL.md)** for a full per-transaction XLM cost
breakdown, including:

- Persistent and temporary storage footprint
- Write, read, and TTL-extension fees
- Lifecycle totals (minimum and typical)
- Monthly budget estimates at various volumes
- Impact of string-length caps on worst-case costs

**TL;DR:** Approximately **0.016–0.027 XLM per transaction** at current
baseline fees, dominated by the initial persistent-write cost.

---

## Monitoring

| Metric | Source | Action |
|--------|--------|--------|
| `ContractUpgraded` event | Event subscription | Alert — unexpected upgrade may indicate key compromise |
| Balance of admin account | Horizon | Ensure sufficient XLM for rent + fees |
| Transaction volume | Off-chain relay logs | Re-budget if volume exceeds deployment estimate |
| String lengths | Contract validation | Ensure `validation.rs` caps are enforced on deploy |

---

## Admin Key Requirements

> **The admin key is the single most important secret in the Synapse Bridge
> ecosystem.**  See [DECISIONS.md](./DECISIONS.md) §6 for the full trust model.

- **MUST** be held by a multisig (minimum 3-of-5) or a DAO
- **SHOULD NOT** be the same key used for deployment or relay signing
- **SHOULD** be monitored for unexpected usage
- **SHOULD** have a key-rotation plan documented before mainnet launch
---

## Break-glass: merging duplicate transactions

`merge_duplicate_transactions(canonical_tx_id, duplicate_tx_id, caller, reason)`
is an admin-only recovery tool, **not** a routine operation. Use it only when
evidence shows a replay slipped through as a separate `transaction_id` (e.g.
before the F-07 durable guard existed). The duplicate is never deleted: it is
marked `Failed` with `failure_reason = "merged"`, gets a `MergedInto(canonical)`
marker (query with `get_merged_into`) and an `EventTransactionsMerged` (`merged`
topic) is emitted carrying `reason`. Settled (`Completed`) duplicates, self-merges
and already-merged records are rejected. Record the evidence off-chain alongside
the event.
