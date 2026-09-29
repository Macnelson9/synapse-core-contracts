# Architecture Decision Records

This directory contains the Architecture Decision Records (ADRs) for
`synapse-core-contracts`. Each file captures one non-obvious decision:
the context that made it necessary, the options that were considered, the
choice made, and the consequences to live with.

ADRs are append-only. A superseded decision gets a new ADR that links back
to the old one; the old file is never modified or deleted.

---

## Index

| # | Title | Status | Date |
|---|-------|--------|------|
| [0001](./0001-relay-signer-trust-model.md) | Relay-signer trust model | Accepted | 2025-Q1 |
| [0002](./0002-two-step-admin-transfer.md) | Two-step admin transfer with self-nomination guard | Accepted | 2026-Q3 |
| [0003](./0003-upgrade-schema-version-guard.md) | Upgrade schema-version guard | Accepted | 2026-Q3 |
| [0004](./0004-guardian-emergency-admin-revocation.md) | Guardian-quorum emergency admin revocation | Proposed | 2026-Q3 |

---

## When to write an ADR

Write an ADR whenever a decision:

- affects the **trust model** or access-control boundaries,
- makes a **storage or data-format choice** that is expensive to reverse,
- chooses between **two or more non-trivial options** with real trade-offs,
- is likely to be **questioned or re-litigated** during code review or by a
  future contributor, or
- would otherwise live only in a PR thread that becomes hard to find once
  merged.

You do not need an ADR for naming decisions, minor refactors, or any choice
where there is an obvious "only sane option".

---

## Template

Copy the block below into a new file named `NNNN-short-title.md`, where
`NNNN` is the next unused four-digit number.

```markdown
# ADR-NNNN — Title

> **Status:** Proposed | Accepted | Superseded by [ADR-MMMM](./MMMM-title.md)
> **Date:** YYYY-QN (or YYYY-MM-DD)
> **Deciders:** (list names or roles, e.g. "relay operator, contract author")

---

## Context

What situation, constraint, or question forced this decision?
Include relevant external facts (protocol behaviour, security model,
operational constraints) that a future reader might not know.

## Options considered

### Option A — (name)

Short description.

**Pros:**
- …

**Cons:**
- …

### Option B — (name)

Short description.

**Pros:**
- …

**Cons:**
- …

## Decision

State the chosen option and the primary reason.

## Consequences

- **Positive:** …
- **Negative / accepted trade-offs:** …
- **Follow-up work:** (list any TODOs this decision defers)

## References

- Link to relevant code, docs, or external resources.
```

---

## Relationship to DECISIONS.md

[`DECISIONS.md`](../../DECISIONS.md) at the repo root holds the first
major architectural decision (in-place contract upgradability) in a
long-form narrative format that predates this ADR log. That document is
authoritative for its topic and is not being migrated. Future decisions
use this `docs/adr/` directory and the template above.
