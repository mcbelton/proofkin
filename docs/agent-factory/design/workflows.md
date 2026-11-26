# Agent factory: workflows

Each workflow names the user stories it implements (`spec/SPEC.md`).

## Describe and draft an agent (US-1, US-2)

The owner describes the agent, optionally from a template. The owner's default model
writes the draft read-only, without tools; the factory checks it and sends problems back,
at most 3 tries. Nothing is granted.

```mermaid
sequenceDiagram
  actor O as Owner
  participant UI as Web UI or CLI
  participant F as Factory drafter
  participant T as Templates
  participant M as Owner's model
  participant V as Blueprint validator
  participant S as Registry store
  O->>UI: Describe the agent, pick a template (optional)
  UI->>F: createDraft(description, template, name)
  F->>T: The template's blueprint and starter evals
  F->>V: The catalog it may use (skills, topics, folders, MCP names, models)
  loop at most 3 tries
    F->>M: Write a blueprint (schema, catalog, template, description, problems so far)
    M-->>F: A blueprint in a fenced JSON block
    F->>V: Validate and lint
    V-->>F: Problems with fixes, or none
  end
  F->>S: Save the draft with its provenance (model, effort, template, description hash)
  F-->>UI: The draft and any problems left
  UI-->>O: Edit it as a form, fix what is left
```

## Check capabilities against what the owner offers (US-3)

```mermaid
sequenceDiagram
  actor O as Owner
  participant F as Factory
  participant C as Capability resolver
  participant SH as Shares and MCP offers
  participant TR as Allowlist and grants
  O->>F: checkDraft(folders)
  F->>C: The blueprint's access, MCP servers and tools, checks, Work IQ sources, delegation
  C->>SH: What each folder offers, and at which access
  C->>TR: Work IQ sign-in and grants, delegation grants
  alt a capability isn't offered
    C-->>F: Error naming it and the command that offers it
  else more than the requirements need
    C-->>F: Least-privilege warning naming the narrower choice
  end
  C-->>F: Capability increases over the active release
  F-->>O: Problems, warnings and increases. Nothing was offered or granted
```

## Evaluate a build across models (US-4)

```mermaid
sequenceDiagram
  actor O as Owner
  participant F as Factory
  participant B as Builder
  participant R as Eval runner
  participant W as Evaluation worker
  participant P as Candidate model
  participant G as Graders
  participant J as Judge model, another provider
  O->>F: startEvaluation(candidates, repetitions, confirm=false)
  F->>B: Build the draft (deterministic, hashed)
  B-->>F: Build hash, agent file, skills, suite with adversarial cases
  F-->>O: Estimate (cases, repetitions, candidates, tasks, minutes)
  O->>F: startEvaluation(confirm=true)
  loop each candidate, case and repetition, within the worker's task slots
    R->>W: Task with the build's instructions and the case, over iroh
    W->>P: The task, at the blueprint's access, sandboxed
    P-->>W: Output, tool calls, files changed
    W-->>R: Output and signed evidence (redacted)
    R->>G: Deterministic expectations
    opt the case has a judge rubric
      R->>J: Rubric and the output inside the external-content envelope
      J-->>R: Score 1 to 5, median of 3
    end
  end
  R->>F: Report per candidate (pass rates, adversarial, p95, attempts, multiplier, judge)
  F-->>O: The gate, the recommended candidate and the failing cases
```

## Release with sign-offs (US-5)

```mermaid
sequenceDiagram
  actor O as Owner
  participant F as Factory releases
  participant S as Registry store
  participant A as Approver's agent
  actor AP as Approver
  O->>F: requestRelease(evaluation, version, note)
  F->>S: The evaluation passed for exactly this build hash?
  F->>F: Manifest of hashes, signed with the owner's key
  F->>A: Sign-off request with the diff, eval summary and provenance
  A->>AP: Approve or reject, with a note
  AP-->>A: Approved
  A-->>F: Signed answer
  opt the release adds capabilities
    F->>O: Sign off the capability diff
    O-->>F: Approved
  end
  F->>S: Released (immutable), and the active version moves
  F-->>O: fin-analyst 1.2.0 released
```

## Deploy with a canary, roll back (US-6)

```mermaid
sequenceDiagram
  actor O as Owner
  participant D as Deployer
  participant WK as workers.rs and Supervisor
  participant W as Deployed worker
  participant R as Eval runner
  O->>D: deployAgent(version, folders, start, canary)
  D->>D: Verify the release's hashes and signature
  D->>WK: Create or update the worker, pinned to name@version, its folders and models
  WK->>W: Start it, refusing new tasks during the canary
  D->>R: Run the release's smoke cases on W
  alt the canary passes
    R-->>D: Passed
    D->>W: Take work
    D-->>O: Running 1.2.0
  else the canary fails
    R-->>D: Failed cases
    D->>WK: Pin the previous release, or stop the worker if there is none
    D-->>O: Rolled back to 1.1.0, with the failed cases
  end
  O->>D: rollbackAgent(reason) later
  D->>WK: Pin the previous release on every deployment, audited
```

## Use a factory agent in a plan (US-7)

```mermaid
sequenceDiagram
  actor B as Plan author
  participant PL as Plan check and runner
  participant RG as Registry
  participant W as Peer or deployed worker
  B->>PL: review @alice:app agent=sec-reviewer@1.2.0 Review the change
  PL->>RG: Resolve sec-reviewer at 1.2.0 (or the active release without a version)
  alt unknown version, or retired
    RG-->>PL: Refused, with the versions there are or the replacement
    PL-->>B: Plan check error
  else released
    RG-->>PL: The release's agent file and skills, verified
    PL->>W: The step, with the release's instructions first
    W-->>PL: Result and signed evidence naming sec-reviewer@1.2.0
  end
```

## Metrics, budgets and drift (US-8, US-9)

```mermaid
sequenceDiagram
  participant PE as Peer
  participant W as Deployed worker
  participant AU as Audit
  participant SC as Scheduler
  participant R as Eval runner
  participant OP as Ops incidents
  actor O as Owner
  PE->>W: A task
  W->>W: Within today's budget, attempts and timeout?
  alt over budget
    W-->>PE: Refused, with the budget it would exceed
    W->>AU: Refused, agent and version, no text
  else within budget
    W-->>PE: Result
    W->>AU: Done, agent and version, model, duration, attempts, usage
  end
  O->>AU: Metrics per version
  SC->>R: Scheduled evaluation of the active release
  R-->>SC: Pass rate below the gate
  SC->>OP: Second failure in a row, open an incident
  OP-->>O: Alert with the failing cases
  SC->>R: The next scheduled evaluation
  R-->>SC: Passed its gate
  SC->>OP: Close the incident
```

## Improve an agent (US-9)

```mermaid
sequenceDiagram
  actor O as Owner
  participant I as Improver
  participant M as Owner's model
  participant R as Eval runner
  participant F as Factory releases
  O->>I: proposeImprovement(cases, note)
  I->>M: Blueprint, failing cases with outputs (redacted), failed task outcomes, the note
  M-->>I: A new draft version
  I->>R: Evaluate it on the suite plus regression cases from the failures
  R-->>I: Report
  I-->>O: The diff and the report, side by side with the active release
  O->>F: Release it, or discard it
  Note over I,F: The active release never changes without a release
```

## Publish and import (US-10)

```mermaid
sequenceDiagram
  actor O as Publisher
  participant PA as Publisher's agent
  participant QA as Peer's agent
  actor Q as Peer
  participant R as Peer's eval runner
  O->>PA: publishAgent(peers, versions)
  PA->>QA: Catalog entries granted to this peer (name, version, description, access, eval summary)
  Q->>QA: listCatalog, then importAgent(alice, fin-analyst, 1.2.0)
  QA->>PA: Ask for the release bundle
  PA-->>QA: Blueprint, agent file, skills, evals, report, manifest, signature
  QA->>QA: Signature is alice's key, every hash matches, nothing runs
  QA->>R: Evaluate on the peer's own accounts
  R-->>QA: Passed
  QA->>QA: Release 1.2.0 signed by the peer, its publisher's version
  QA-->>Q: fin-analyst 1.2.0 imported and active
```

## Deprecate and retire (US-11)

```mermaid
sequenceDiagram
  actor O as Owner
  participant F as Factory
  participant WK as Deployed workers
  participant PL as Plan check
  O->>F: setLifecycle(deprecated, replacement fin-analyst-2)
  PL->>F: A plan uses fin-analyst
  F-->>PL: Warning naming fin-analyst-2
  O->>F: setLifecycle(retired, reason)
  F->>WK: Stop every deployment
  F->>F: Unpublish, keep releases and reports for audit
  PL->>F: A plan uses fin-analyst
  F-->>PL: Refused, retired, use fin-analyst-2
```

## Editor agents (US-12)

```mermaid
sequenceDiagram
  participant E as Editor agent
  participant C as MCP endpoint with the integration token
  participant F as Factory
  E->>C: tools/call factory_draft, factory_check, factory_evaluate, factory_status
  C->>F: Same operations as the web UI
  F-->>C: Drafts, problems, estimates, reports
  C-->>E: Results
  E->>C: tools/call that would release, deploy, roll back, retire, approve, publish, import, watch or improve
  C-->>E: Refused, with the owner's command to run instead
```

## Memory: context and lessons (US-13)

```mermaid
sequenceDiagram
  participant D as Deployer
  participant KB as Owner's knowledge
  participant W as Deployed worker
  participant P as Model
  participant L as Worker's lessons topic
  D->>KB: The blueprint's context topics, with their marks
  D->>W: Copy them into the worker's knowledge at deployment
  W->>P: A task, with the release's instructions, then the context topics (at most 24,000 characters)
  P-->>W: Output, marked with the topics' labels
  W->>W: Did the task's checks pass?
  alt verified
    W->>L: Add a lesson with the task's labels, deduplicated
  else not verified
    W-->>W: No lesson
  end
  W->>L: Prune lessons older than retention_days
```

## The agent lifecycle

```mermaid
stateDiagram-v2
  [*] --> Draft: describe or template
  [*] --> Imported: import a peer's release
  Imported --> Active: evaluated here, or its report accepted
  Draft --> Draft: edit, check
  Draft --> Evaluated: build and evaluate
  Evaluated --> Draft: gate failed, edit
  Evaluated --> PendingApproval: request release
  PendingApproval --> Draft: rejected
  PendingApproval --> Active: approved and signed
  Active --> Active: new release, rollback
  Active --> Deprecated: deprecate with a replacement
  Deprecated --> Active: reactivate
  Deprecated --> Retired: retire
  Active --> Retired: retire
  Retired --> [*]
```

## Agents as code: the pipeline as a plan (US-14)

The `agent-factory` playbook runs the same lifecycle with peers, in a folder under git:
blueprint and evals as files, built-in `agent:*` checks, a red-team squad and sign-offs.

```mermaid
flowchart LR
  bp["blueprint<br/>verify=agent:blueprint"] --> ev["evals<br/>verify=agent:evals"]
  ev --> run["evaluate<br/>verify=agent:report"]
  run --> red["red-team squad<br/>adversarial review"]
  run --> sec["security sign-off<br/>capabilities, threats"]
  red --> sec
  sec --> rel["owner sign-off<br/>release from the folder's commit"]
```
