# ADR-0004: Guardian-quorum emergency admin revocation

Status: Proposed (needs maintainer sign-off on threshold and vacant-state operations)

## Context
ADR-0002's two-step transfer assumes a cooperative admin. A compromised admin
key needs a break-glass path. Guardians (see `guardian_pause`) already exist.

## Decision
- `set_guardians` / `set_guardian_threshold` (admin) configure guardians and M.
- `revoke_admin_emergency(approvers)` requires M distinct guardians' auth.
  One short of M is rejected with `QuorumNotMet`.
- On success: admin and pending admin are removed, the contract is paused, an
  `EventAdminRevokedEmergency` event is emitted.
- "Admin vacant" state: all admin-gated entry points fail with `AdminVacant`.
  Still callable: `guardian_pause`, read-only queries, `heartbeat`.
  `register_callback` is blocked by the pause; status transitions use
  `get_admin` and fail closed.

## Consequences
- Accepted trust assumption: colluding guardians can revoke a genuine admin.
- Bootstrap of a new admin is a follow-up issue (suggested: guardian-quorum
  `bootstrap_admin(new_admin)` gated on the vacant flag, with a timelock).
