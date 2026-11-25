# ADR-0004: Releases are immutable, content-addressed and signed

## Status

Proposed

## Context

The brief asks for an agent registry with versions, prompt versions, rollback, and human
approval for sensitive actions. In Proofkin, trust is per key: each profile has an ed25519
identity, results are signed, and `runs verify` re-checks evidence offline. An agent's
risk changes most when its capabilities grow: more access, more tools, Work IQ sources, or
the right to delegate.

## Decision

- A **release** is created only from a build whose evaluation passed the gate for exactly
  that build hash. It gets a semantic version (never reused) and a manifest, `release.json`,
  listing the SHA-256 of the blueprint, agent file, skills, team plan, eval suite, reports and
  provenance, signed with the owner's identity key.
- A release is **immutable**. Changing anything is a new build, a new evaluation and a new
  release.
- Every release asks the blueprint's **approvers** for a sign-off, with the diff, the eval
  summary and the provenance. A release that **adds capabilities** to the active one also
  needs the **owner's** sign-off on that diff, whoever else approved.
- `proofkin factory verify` checks every hash and signature offline; the factory checks them
  again whenever it loads a release, so a changed file makes the release unusable.

## Alternatives considered

- **Mutable agents with history** (edit in place, keep a log): simpler, but what ran last
  week can't be proven, and rollbacks restore something nobody evaluated.
- **Git as the registry**: commits are content-addressed, but not every owner keeps agents in
  a repository, and signatures would depend on each person's git signing setup. The
  agents-as-code path still records the commit a release came from.
- **Approvals only for major versions**: fewer sign-offs, but version numbers are chosen by
  people; capability increases are measured, so they are what triggers the owner's sign-off.

## Consequences

- Every running agent can be traced to one evaluated, approved, signed release.
- Rollback is choosing another release; no state is reconstructed.
- Releases accumulate; retirement keeps them for audit, and purging is an explicit step.
