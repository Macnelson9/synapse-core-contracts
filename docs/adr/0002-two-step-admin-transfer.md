# ADR-0002 — Two-step admin transfer with self-nomination guard

> **Status:** Accepted
> **Date:** 2026-Q3
> **Deciders:** Synapse Bridge core team, contract author

---

## Context

The admin role is the root of trust for `synapse-core-contract`: it controls
`pause`/`unpause`, `upgrade`, `set_relay_signer`, and its own succession.
THREAT_MODEL.md's self-review (findings F-02, F-03) identified two related
gaps in the original `transfer_admin(new_admin)` entry point:

1. **F-03 — single-step transfer.** `transfer_admin` moved the admin role in
   one atomic call, authorised only by the *current* admin. A mistyped
   address, a copy-paste error, or a compromised-but-not-yet-detected admin
   key could hand control to an address nobody can act from — with no
   opportunity for the receiving party to confirm they actually control it
   before the old admin's access is gone.
2. **F-02 — no guard on the nominee address.** There was no on-chain check
   that `new_admin` was anything other than a syntactically valid `Address`.
   EVM-style contracts commonly guard against the zero address; Soroban has
   no equivalent universal "invalid address" sentinel, since both account
   and contract addresses are opaque hashes with no reserved null value.

Both findings point at the same underlying question: **how much should the
contract itself defend against a bad address ending up as admin, versus
relying entirely on off-chain process (careful copy-pasting, multisig
review) to get it right?**

---

## Options considered

### Option A — Two-step transfer (nominate → accept), plus self-nomination guard ✅ (chosen)

Split `transfer_admin` into `propose_admin(new_admin)` (current admin
nominates, stores a pending nominee) and `accept_admin(caller)` (only the
pending nominee, using its own `require_auth()`, can finalise). Additionally
reject nominating the contract's own address in `propose_admin`.

**Pros:**
- The nominee must prove key control by submitting its own authorised
  transaction before the old admin loses access — the single most common
  real-world failure mode (nominating an address nobody holds the key for)
  becomes self-correcting: if the nominee can't sign, the transfer simply
  never completes and the current admin stays in control.
- The one class of "invalid address" Soroban *can* check for on-chain — the
  contract's own address, which cannot practically call `accept_admin` back
  — is rejected up front, rather than only failing silently later.
- Symmetric with the existing `require_auth()`-based trust model already
  used everywhere else in the contract (`relay.require_auth()` in
  `register_callback`, `caller.require_auth()` in the status-transition
  methods) — no new primitive introduced.
- Low implementation cost: one new persistent storage key
  (`StorageKey::PendingAdmin`), one new event, no change to the multisig
  operational model already required for the admin key.

**Cons:**
- Two on-chain calls instead of one — a small operational overhead (two
  multisig-signed transactions instead of one) for every admin rotation,
  which is a rare operation.
- Does not, and cannot, detect a nominee address that is well-formed but
  whose private key was already lost or never existed — the contract has no
  way to distinguish that from an address whose key is simply not yet
  available at nomination time. That failure mode degrades gracefully
  under this design (the transfer just never completes, and the current
  admin remains in control) rather than being actively prevented.

### Option B — Keep single-step transfer, add only a self-nomination guard

Address just F-02 (reject the contract's own address) without addressing
F-03 (single-step finality).

**Pros:**
- Smaller change; no new storage key or event; no operational overhead.

**Cons:**
- Leaves the more consequential risk (F-03) open: a mistyped *externally
  owned* address — the overwhelmingly more likely mistake than nominating
  the contract's own address — would still permanently transfer control
  with no recovery path. The self-nomination guard alone only prevents one
  narrow, arguably less likely, failure mode.

### Option C — Full timelocked governance (schedule → delay → execute)

Require a nominated transfer to wait N ledgers before it can be accepted,
in addition to the two-step nominate/accept pattern.

**Pros:**
- Gives observers (off-chain monitoring, other multisig signers) a window
  to notice and react to an unexpected proposal before it can take effect.

**Cons:**
- Meaningfully more implementation surface (ledger-sequence tracking,
  cancellation semantics, a third error state) for a benefit that mostly
  duplicates what multisig review *before* signing the `propose_admin`
  transaction already provides — the multisig threshold is the actual
  timelock-equivalent control here. Noted as a candidate follow-up (see
  Consequences) rather than adopted now.

---

## Decision

**Adopt Option A.** `propose_admin` + `accept_admin` replaces the single-step
`transfer_admin`, and `propose_admin` rejects nominating the contract's own
address via `Validator::validate_admin_nominee`.

This directly closes F-03 (the higher-severity finding: no recovery path
from a bad transfer) and F-02 (the one on-chain-checkable invalid-address
case), using the same `require_auth()` trust pattern already established
everywhere else in the contract, at low implementation cost.

---

## Consequences

- **Positive:** A transfer to an address nobody controls the key for can
  no longer strand the admin role — it simply never completes, and
  `pending_admin()` lets anyone observe a proposal is outstanding.
  `EventAdminTransferProposed` gives off-chain monitoring visibility into a
  proposal *before* it takes effect, not just after.
- **Negative / accepted trade-offs:** Admin rotation now requires two
  separate multisig-signed transactions instead of one. A nominee whose key
  is genuinely lost (as opposed to simply not yet available) is
  indistinguishable on-chain from one who just hasn't accepted yet — the
  current admin must notice the transfer is stuck and `propose_admin` again
  with a different nominee, which overwrites the stale pending proposal.
- **Follow-up work:** A timelock between proposal and acceptance (Option C)
  is a reasonable Phase 2 enhancement if the operational review process
  around signing `propose_admin` transactions proves insufficient in
  practice — not adopted now for the same reasoning already accepted for
  `upgrade()` in [`DECISIONS.md` §7](../../DECISIONS.md#7-future-enhancements-out-of-scope-for-phase-1).

---

## References

- [`src/lib.rs`](../../src/lib.rs) — `propose_admin` / `accept_admin` entry points
- [`src/validation.rs`](../../src/validation.rs) — `validate_admin_nominee`
- [`src/storage.rs`](../../src/storage.rs) — `PendingAdmin` storage helpers
- [`src/tests.rs`](../../src/tests.rs) — two-step round trip, self-nomination
  rejection, wrong-caller rejection, and overwrite-nominee tests
- [`THREAT_MODEL.md`](../../THREAT_MODEL.md) — findings F-02, F-03
- [`EVENTS.md`](../../EVENTS.md) — `EventAdminTransferProposed`, `EventAdminTransferred`
- [`CHANGELOG.md`](../../CHANGELOG.md) — breaking-change notice for the
  removed `transfer_admin`

## Amendment: optional acceptance-window expiry

`propose_admin_with_expiry(new_admin, expiry)` attaches an optional ledger
timestamp (seconds) to a pending proposal. `accept_admin` fails with
`AdminProposalExpired` when `timestamp > expiry` (the boundary is inclusive:
accepting at exactly `expiry` succeeds). `propose_admin` creates a proposal with
no expiry, preserving the original semantics. `cancel_admin_proposal(caller)`
lets the current admin withdraw a pending proposal; it fails with
`NoPendingAdminTransfer` once nothing is pending (e.g. after acceptance).
