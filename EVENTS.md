# Event Schema — Public API Contract

> **Status:** Locked for Phase 2 / Phase 3 subscribers  
> **Source of truth:** [`src/events.rs`](./src/events.rs)  
> **Contract version:** `version()` → [`Cargo.toml`](./Cargo.toml) `package.version` (currently **0.1.0**)  
> **Audience:** Swap Engine (Phase 2), Cross-Chain Bridge (Phase 3), off-chain indexers

This document is the **stable public API** for on-chain events emitted by
`synapse-core-contract`. Topic names, data field names/types/order, and
multi-event emission order are part of the contract surface. Changing them is a
**breaking change** for teams that do not share this repo’s release cycle.

Semver policy for this schema lives in [§ Semver policy](#semver-policy) and is
summarised in the README [design-decisions](./README.md#event-schema-as-a-stable-public-api).
Schema revisions are logged separately in [`CHANGELOG.md`](./CHANGELOG.md#event-schema).

---

## 1. Wire format

Every event is published as:

```text
topics: [ Symbol("synapse"), Symbol("<event_name>") ]
data:   <typed #[contracttype] struct>   // XDR map; field order = Rust declaration order
```

| Index | Topic | Type | Notes |
|------:|-------|------|--------|
| 0 | `synapse` | `Symbol` | Fixed namespace for all Synapse Core events |
| 1 | `<event_name>` | `Symbol` | Short name (`symbol_short!`); see catalogue below |

Subscribers SHOULD filter on both topics. Relying only on payload shape is not
supported.

`ledger` in every payload is `env.ledger().sequence()` at emit time (`u32`).

---

## 2. Implementation status

| Event | Topic[1] | Emitter | Entry-point(s) | Status |
|-------|----------|---------|----------------|--------|
| [`EventInitialised`](#eventinitialised) | `init` | `EventEmitter::initialised` | `initialize` | **Live** |
| [`EventTransactionRegistered`](#eventtransactionregistered) | `reg` | `EventEmitter::transaction_registered` | `register_callback` (first write only) | **Live** |
| [`EventPauseToggled`](#eventpausetoggled) | `pause` | `EventEmitter::pause_toggled` | `pause`, `unpause` | **Live** |
| [`EventContractUpgraded`](#eventcontractupgraded) | `upgrade` | `EventEmitter::contract_upgraded` | `upgrade` | **Live** |
| [`EventStatusChanged`](#eventstatuschanged) | `status` | `EventEmitter::status_changed` | `start_processing`, `complete_transaction`, `fail_transaction` | **Live** |
| [`EventTransactionCompleted`](#eventtransactioncompleted) | `done` | `EventEmitter::transaction_completed` | `complete_transaction` | **Live** |
| [`EventTransactionFailed`](#eventtransactionfailed) | `fail` | `EventEmitter::transaction_failed` | `complete_transaction` (route configured) | 1. `status`<br>2. `done`<br>3. `fwd` (`EventForwardingIntent { tx_id, next_phase }`, additive) |
| `fail_transaction` | **Live** |
| [`EventAdminTransferProposed`](#eventadmintransferproposed) | `propose` | `EventEmitter::admin_transfer_proposed` | `propose_admin` | **Live** |
| [`EventAdminTransferred`](#eventadmintransferred) | `admin` | `EventEmitter::admin_transferred` | `accept_admin` | **Live** |
| [`EventRelaySignerRotated`](#eventrelaysignerrotated) | `relay` | `EventEmitter::relay_signer_rotated` | `set_relay_signer` | **Live** |
| [`EventSignerAttestationSet`](#eventsignerattestationset) | `attest` | `EventEmitter::signer_attestation_set` | `set_signer_attestation` | **Live** |
| [`EventAdminRenounced`](#eventadminrenounced) | `renounce` | `EventEmitter::admin_renounced` | `renounce_admin` | **Live** |
| [`EventGuardianSet`](#eventguardianset) | `guardian` | `EventEmitter::guardian_set` | `set_guardian` | **Live** |
| [`EventAutoPaused`](#eventautopaused) | `apause` | `EventEmitter::auto_paused` | `trip_auto_pause` | **Live** |
| [`EventAutoUnpaused`](#eventautounpaused) | `aunpause` | `EventEmitter::auto_unpaused` | `unpause_auto` (quorum met) | **Live** |

**Locked schema** means topics, struct fields, types, and field order are fixed
in this document and in `src/events.rs` even if the `publish` call is still
`todo!()`. Wiring the emitters must match this table exactly — no silent renames.

---

## 3. Event catalogue

Field tables list fields in **declaration / XDR order**. Do not reorder.

### EventInitialised

| | |
|--|--|
| **Topics** | `synapse`, `init` |
| **Struct** | `EventInitialised` |
| **Emitted by** | `initialize` |
| **When** | Once, after admin + relay signer are written |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `admin` | `Address` | Initial admin |
| `relay_signer` | `Address` | Initial trusted relay |
| `ledger` | `u32` | Ledger sequence at emit |

### EventTransactionRegistered

| | |
|--|--|
| **Topics** | `synapse`, `reg` |
| **Struct** | `EventTransactionRegistered` |
| **Emitted by** | `register_callback` |
| **When** | First successful persist of a transaction (**not** on idempotent replay) |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `tx_id` | `String` | On-chain / payload transaction id |
| `stellar_account` | `String` | Destination G-address |
| `amount` | `i128` | Amount (stroops / asset units as stored) |
| `asset_code` | `String` | SEP-11 asset code |
| `anchor_transaction_id` | `String` | Anchor Platform id |
| `ledger` | `u32` | Ledger sequence at emit |

### EventStatusChanged

| | |
|--|--|
| **Topics** | `synapse`, `status` |
| **Struct** | `EventStatusChanged` |
| **Emitted by** | `start_processing`, `complete_transaction`, `fail_transaction` |
| **When** | Every successful status-machine transition |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `tx_id` | `String` | Transaction id |
| `old_status` | `TransactionStatus` | Status before transition |
| `new_status` | `TransactionStatus` | Status after transition |
| `ledger` | `u32` | Ledger sequence at emit |

`TransactionStatus` variants (discriminant order as in `types.rs`):
`Pending`, `Processing`, `Completed`, `Failed`.

Phase 2 / Phase 3 SHOULD treat `new_status == Completed` as the cross-phase
signal (also see [`EventTransactionCompleted`](#eventtransactioncompleted)).

### EventTransactionCompleted

| | |
|--|--|
| **Topics** | `synapse`, `done` |
| **Struct** | `EventTransactionCompleted` |
| **Emitted by** | `complete_transaction` |
| **When** | Terminal success after on-chain settlement is recorded |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `tx_id` | `String` | Transaction id |
| `stellar_tx_hash` | `String` | Confirmed Stellar tx hash |
| `ledger` | `u32` | Ledger sequence at emit |

### EventTransactionFailed

| | |
|--|--|
| **Topics** | `synapse`, `fail` |
| **Struct** | `EventTransactionFailed` |
| **Emitted by** | `fail_transaction` |
| **When** | Terminal failure |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `tx_id` | `String` | Transaction id |
| `reason` | `String` | Short failure code |
| `ledger` | `u32` | Ledger sequence at emit |

### EventAdminTransferProposed

| | |
|--|--|
| **Topics** | `synapse`, `propose` |
| **Struct** | `EventAdminTransferProposed` |
| **Emitted by** | `propose_admin` |
| **When** | Current admin nominates a new admin. Not yet effective — see [`EventAdminTransferred`](#eventadmintransferred), emitted only once the nominee calls `accept_admin`. |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `current_admin` | `Address` | Admin making the proposal |
| `proposed_admin` | `Address` | Nominated address |
| `ledger` | `u32` | Ledger sequence at emit |

### EventAdminTransferred

| | |
|--|--|
| **Topics** | `synapse`, `admin` |
| **Struct** | `EventAdminTransferred` |
| **Emitted by** | `accept_admin` |
| **When** | Nominee accepts a pending admin transfer, completing it |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `old_admin` | `Address` | Previous admin |
| `new_admin` | `Address` | New admin |
| `ledger` | `u32` | Ledger sequence at emit |

### EventRelaySignerRotated

| | |
|--|--|
| **Topics** | `synapse`, `relay` |
| **Struct** | `EventRelaySignerRotated` |
| **Emitted by** | `set_relay_signer` |
| **When** | Trusted relay signer successfully rotated |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `old_signer` | `Address` | Previous relay signer |
| `new_signer` | `Address` | New relay signer |
| `ledger` | `u32` | Ledger sequence at emit |

### EventContractUpgraded

| | |
|--|--|
| **Topics** | `synapse`, `upgrade` |
| **Struct** | `EventContractUpgraded` |
| **Emitted by** | `upgrade` |
| **When** | After `update_current_contract_wasm` succeeds |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `admin` | `Address` | Admin that authorised the upgrade |
| `new_wasm_hash` | `BytesN<32>` | SHA-256 of the new WASM |
| `ledger` | `u32` | Ledger sequence at emit |
| `schema_version` | `u32` | On-chain schema version `expected_schema_version` was checked against (F-04). Additive trailing field, added after the initial 0.1.0 lock — Minor bump. |

Verified by snapshot-style test
`test_pause::test_upgrade_emits_contract_upgraded_event`
(topics `synapse` / `upgrade`).

### EventPauseToggled

| | |
|--|--|
| **Topics** | `synapse`, `pause` |
| **Struct** | `EventPauseToggled` |
| **Emitted by** | `pause`, `unpause` |
| **When** | Circuit breaker engaged or released (idempotent calls still emit) |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `paused` | `bool` | `true` = paused, `false` = unpaused |
| `admin` | `Address` | Admin that toggled |
| `ledger` | `u32` | Ledger sequence at emit |

### EventSignerAttestationSet

| | |
|--|--|
| **Topics** | `synapse`, `attest` |
| **Struct** | `EventSignerAttestationSet` |
| **Emitted by** | `set_signer_attestation` |
| **When** | Every attestation write, including no-op same-hash updates |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `signer` | `Address` | Relay signer that self-reported the fingerprint |
| `build_hash` | `BytesN<32>` | Self-reported build/commit fingerprint (not cryptographically verified on-chain) |
| `ledger` | `u32` | Ledger sequence at emit |

### EventAdminRenounced

| | |
|--|--|
| **Topics** | `synapse`, `renounce` |
| **Struct** | `EventAdminRenounced` |
| **Emitted by** | `renounce_admin` |
| **When** | Outgoing admin acknowledges step-down after a live successor accepted |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `former_admin` | `Address` | Outgoing admin |
| `successor` | `Address` | Live admin installed by prior `accept_admin` |
| `ledger` | `u32` | Ledger sequence at emit |

### EventGuardianSet

| | |
|--|--|
| **Topics** | `synapse`, `guardian` |
| **Struct** | `EventGuardianSet` |
| **Emitted by** | `set_guardian` |
| **When** | Guardian address set or rotated |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `guardian` | `Address` | New guardian |
| `ledger` | `u32` | Ledger sequence at emit |

### EventAutoPaused

| | |
|--|--|
| **Topics** | `synapse`, `apause` |
| **Struct** | `EventAutoPaused` |
| **Emitted by** | `trip_auto_pause` |
| **When** | Automatic circuit-breaker pause engaged |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `ledger` | `u32` | Ledger sequence at emit |

### EventAutoUnpaused

| | |
|--|--|
| **Topics** | `synapse`, `aunpause` |
| **Struct** | `EventAutoUnpaused` |
| **Emitted by** | `unpause_auto` (when 2-of-3 quorum is met) |
| **When** | Automatic pause released by a role pair |
| **Status** | Live |

| Field | Type | Meaning |
|-------|------|---------|
| `roles` | `UnpauseRoles` | Winning pair: `AdminGuardian` / `AdminRelay` / `GuardianRelay` |
| `ledger` | `u32` | Ledger sequence at emit |

---

## 4. Guaranteed emission order

When a single entry-point publishes **more than one** event, order is stable and
part of the API. Subscribers MAY rely on relative order within the same
invocation / transaction.

| Entry-point | Order (first → last) |
|-------------|----------------------|
| `initialize` | 1. `init` |
| `register_callback` (first write) | 1. `reg` |
| `register_callback` (idempotent hit) | *(no events)* |
| `start_processing` | 1. `status` (`Pending` → `Processing`) |
| `complete_transaction` | 1. `status` (`Processing` → `Completed`)<br>2. `done` |
| `fail_transaction` | 1. `status` (`Pending`\|`Processing` → `Failed`)<br>2. `fail` |
| `propose_admin` | 1. `propose` |
| `accept_admin` | 1. `admin` |
| `set_relay_signer` | 1. `relay` |
| `upgrade` | 1. `upgrade` |
| `pause` / `unpause` | 1. `pause` |
| `set_signer_attestation` | 1. `attest` |
| `renounce_admin` | 1. `renounce` |
| `set_guardian` | 1. `guardian` |
| `trip_auto_pause` | 1. `apause` |
| `unpause_auto` (quorum met) | 1. `aunpause` |
| `unpause_auto` (vote only) | *(no events)* |

**Rationale for `complete_transaction`:** Phase 2 indexers that listen only to
`done` still see completion; those that key off `status` with
`new_status == Completed` see the transition first, then the hash-bearing
`done` payload. Reordering would break dual-subscriber setups.

---

## 5. Semver policy

The string returned by `version()` is the contract package semver
(`Cargo.toml` → `package.version`). **Event-schema compatibility follows that
version**, independently of unrelated code churn.

| Change | Version bump | Advance notice |
|--------|--------------|----------------|
| Add a **new optional field at the end** of an existing event struct\* | **Minor** or **Patch** | Recommended |
| Add a **new event** (new topic[1] + struct) | **Minor** | Recommended |
| Document-only / non-behavioural clarifications | **Patch** | Not required |
| **Remove** a field | **Major** | **Required** — notify Phase 2 / Phase 3 |
| **Rename** a field or topic symbol | **Major** | **Required** |
| **Reorder** fields in a `#[contracttype]` struct | **Major** | **Required** |
| **Change** a field’s type | **Major** | **Required** |
| Change which events fire on a transition, or **emission order** | **Major** | **Required** |
| Change topic[0] away from `synapse` | **Major** | **Required** |

\*Soroban `#[contracttype]` structs are positional in XDR. “Additive at the end”
is the only additive pattern allowed without a major bump; inserting a field in
the middle is a **Major** (reorder). Prefer a **new event** over mid-struct
inserts when in doubt.

### Advance notice

For any **Major** event-schema change:

1. Open / update an issue tagged for subscriber teams **before** merging.
2. Record the planned break under [`CHANGELOG.md` → Event schema → Unreleased](./CHANGELOG.md#event-schema).
3. Bump `version()` major in the same release that ships the break.
4. Keep the old behaviour available until the noticed cutover date when
   operationally possible (dual-emit is allowed only within a documented
   migration window and itself requires changelog entries).

---

## 6. Verification checklist (maintainers)

Before merging any PR that touches `src/events.rs` or event emit sites in
`src/lib.rs`:

1. Diff this file against `EventEmitter::*` and the `#[contracttype]` structs.
2. Confirm topic symbols match `symbol_short!(...)` exactly (`init`, `reg`,
   `pause`, `upgrade`, `status`, `done`, `fail`, `admin`, `relay`, `propose`,
   `attest`, `renounce`, `guardian`, `apause`, `aunpause`).
3. Confirm multi-event order in §4 still matches the call sites.
4. Run snapshot-style tests (e.g. `test_pause::test_upgrade_emits_contract_upgraded_event`)
   and any new event tests; topics in assertions must match §3.
5. If the schema changed, update [`CHANGELOG.md`](./CHANGELOG.md#event-schema)
   and bump `version()` per §5.

---

## 7. References

- Implementation: [`src/events.rs`](./src/events.rs)
- Status enum: [`src/types.rs`](./src/types.rs) (`TransactionStatus`)
- Upgradability / admin trust: [`DECISIONS.md`](./DECISIONS.md)
- Version probe: `SynapseCoreContract::version`

## Addendum: `merged` event

`EventTransactionsMerged { canonical_tx_id, duplicate_tx_id, admin, reason, ledger }`,
topic `merged`, emitted once by `merge_duplicate_transactions` (break-glass admin action).
