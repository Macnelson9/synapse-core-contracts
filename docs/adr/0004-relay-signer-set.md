# ADR-0004: N-of-M relay signer set

Status: Accepted. Supersedes the single-key aspect of ADR-0001 (mitigates THREAT_MODEL.md R-01).

## Decision
`relay_signer` becomes `RelaySignerSet { signers, threshold }`. Every relay-gated
entry point (`register_callback`, `start_processing`, `complete_transaction`,
`fail_transaction`) requires `threshold` distinct signers to authorise. The admin
path on status transitions is unchanged.

## Alternatives considered
- **Threshold signatures (off-chain aggregation)**: single on-chain key, but the
  contract cannot verify quorum or add/remove members on-chain; rejected.
- **Multi-invocation quorum (chosen)**: each non-caller signer calls
  `approve_relay_call` (short-lived, ~100-ledger, consumed on use); the gated call
  counts the caller plus fresh approvals. Uses only native Soroban auth.
  `register_callback` has no caller argument, so with N>1 it counts approvals only.

## Migration
Existing deployments hold only the legacy `RelaySigner` key. `relay_signer_set()`
lazily reads it as `threshold = 1, signers = [relay_signer]`; no migration
transaction is needed. `set_relay_signer` replaces the primary (first) signer.
`initialize` is unchanged.

## Off-chain requirement (relay-service team)
For threshold > 1 the relay service must have each co-signer call
`approve_relay_call` before the gated call in the same short window.
