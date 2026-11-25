# Agent factory: threat model

STRIDE, per element and per data flow in `design/dataflow.md`. The factory adds new ways
to change what agents do, so most threats are about privilege and integrity: who decides
what an agent may do, and whether what runs is what was evaluated and approved.

## Assets

- The owner's provider accounts, tokens and work data (descriptions, instructions, eval
  fixtures and outputs, Microsoft 365 content).
- The integrity of releases: what was evaluated, approved and signed is what runs.
- The owner's grants: shares, MCP offers, Work IQ grants and the allowlist.
- Approvals and the audit trail.
- The availability of the owner's workers and provider quotas.

## Trust boundaries

- The browser and the owner's agent (DF-1); editor agents and the owner's agent (DF-11).
- The owner's agent and the model providers (DF-2, DF-6).
- The owner's agent and its workers' OS sandbox (DF-4, DF-5).
- Workers and the MCP servers and Work IQ (DF-7).
- The owner's agent and other people's agents: approvers and peers (DF-8, DF-9, DF-10, DF-13).

## Threats

| ID | Element | STRIDE | Threat | Mitigation | Risk | Status |
| --- | --- | --- | --- | --- | --- | --- |
| TM-1 | DF-2, Drafter | Elevation of privilege | Instructions hidden in a description, template or catalog text make the model draft an agent with broad access, every tool or delegation | The drafting model has no tools and works read-only; a draft is data; capabilities are resolved against offers and never granted; least-privilege lint; capability increases need the owner's sign-off (AC-3, AC-5, AC-6, AC-16) | High | Planned |
| TM-2 | DF-10, Import | Tampering | A bundle is changed in transit or by its publisher after evaluation | Every artifact's hash in the signed manifest; signature must be the publisher's key; checked on import and on every load; local evaluation before activation (AC-15, AC-27) | High | Planned |
| TM-3 | DF-9, DF-10, Catalog | Spoofing | Someone publishes an agent as a person the peer trusts | Only allowlisted peers' authenticated agents publish; the manifest's signer must be the publishing peer's key; the catalog shows the key | Medium | Planned |
| TM-4 | DF-5, Graders and judge | Tampering | The agent under test writes output that steers the judge or the graders (score 5, ignore the rubric) | Deterministic expectations decide the gate; the judge sees the output inside the external-content envelope with the standing rule; the judge is another provider or model family; median of 3 (AC-10, AC-13) | High | Mitigated |
| TM-5 | DF-4, DF-6, Deployed worker | Elevation of privilege | A released agent does more than it was approved for (more access, other tools, delegation) | The lower of grant and folder access; only offered MCP servers and allowed tools; agent files' tools and model ignored; delegation needs the owner's grant; increases need the owner's sign-off (AC-7, AC-16, AC-18) | High | Planned |
| TM-6 | DF-4, DF-7, Agent output | Information disclosure | Hostile fixture, file or tool content makes the agent leak secrets or Microsoft 365 content in its output | Mandatory adversarial cases with canary secrets and injected instructions (AC-11); secret redaction; labels follow outputs; the envelope around external content | High | Planned |
| TM-7 | DF-9, Catalog | Information disclosure | Publishing leaks knowledge topics, eval outputs, folder paths or business data | Catalog entries carry name, version, description, access and an eval summary only; bundles never include knowledge or outputs (AC-26) | Medium | Planned |
| TM-8 | DF-8, Release history | Repudiation | An approver or the owner denies a release, deployment, rollback or retirement | Approvals signed by the approver's agent and kept in the manifest; audit entries for every lifecycle change (AC-17, AC-20) | Medium | Planned |
| TM-9 | DF-4, DF-13, Evaluation worker | Denial of service | Large evaluations or runaway agents exhaust task slots, peers' hourly limits or provider quotas | Estimates confirmed before evaluations run (AC-8); runs within task slots on a dedicated worker; per-agent budgets (AC-23); hourly limits | Medium | Planned |
| TM-10 | DF-11, Integration endpoint | Elevation of privilege | An editor agent with the integration token releases, deploys or approves an agent | Factory MCP tools can't release, deploy, roll back, retire or approve (AC-29); the token is separate from the web UI's; loopback only | High | Mitigated |
| TM-11 | DF-1, Web UI | Spoofing | A web page in the owner's browser calls the factory API | The web UI's token, Origin and Host checks, loopback only, as for the rest of the API | Medium | Mitigated |
| TM-12 | DF-2, Drafter and improver | Information disclosure | Descriptions or failing outputs sent to a provider contain secrets | Secrets redacted before sending; outputs capped; only the owner's own provider instance sees them | Medium | Planned |
| TM-13 | Improver | Tampering | An improvement changes the active release by itself | Proposals are new drafts; only a release, through the gate and sign-offs, changes the active version (AC-25) | Medium | Planned |
| TM-14 | DF-8, Sign-off | Spoofing | A forged answer approves a release | Answers only from the approver's authenticated agent, signed and matched to the request id | Medium | Planned |
| TM-15 | Registry store | Tampering | Malware or a mistake edits a release on disk | Hashes and signature checked on every load; a changed release can't deploy or be used in plans (AC-15) | Low | Planned |

## Residual risks

- TM-11 is mitigated by the web UI's existing protections; the factory adds no new origin.
- A judge model and the evaluated model may still share blind spots; judges never gate a
  release, and owners can require a person's sign-off for open-ended qualities.
- Eval suites prove what they test. An agent can pass its suite and still be wrong on inputs
  nobody wrote a case for; drift watches and improvement from failures narrow that gap.
