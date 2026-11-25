# Agent factory: spec

The spec of the agent factory described in [docs/agent-factory.md](../../agent-factory.md),
which answers the [LLM Agent Factory brief](../brief.md).
`proofkin spec check spec --dir docs/agent-factory` and `proofkin spec check api --dir
docs/agent-factory` validate it; the design in `design/` is traced to it.

## Problem

A Proofkin owner assembles an agent by hand today, from parts that don't know about each
other: an agent file for `agent=` (instructions, skills, MCP server names), the skills, a
digital worker with a role, model settings, MCP servers offered in shared folders, knowledge
topics, and plans whose checks prove the work. Nothing ties those parts together as one
versioned agent. Nothing shows that an agent behaves before it is used, a change to its
instructions or tools isn't reviewed as a whole, and there is no per-agent view of quality,
usage or cost. A proven agent can't be shared with a peer safely either.

The LLM Agent Factory brief asks for a platform that turns natural-language requirements
into governed, production-ready agents: specification, generation, model selection, tools,
memory, evaluation, registry, deployment, monitoring, optimization and retirement. Proofkin
already has the runtime (providers, sandboxed tasks, plans, digital workers, signed evidence,
audit, operations). It lacks the factory around it.

## Users

- Owner: a person running a Proofkin profile who creates, evaluates, releases, deploys and
  retires agents on their own provider accounts.
- Approver: a person whose sign-off a release needs (the owner, or a peer on the allowlist
  such as a security champion).
- Plan author: a person or agent (a boss) whose plans use factory agents in their steps.
- Peer: another owner on the allowlist, who may receive published agents or delegate tasks
  to deployed ones.
- Editor agent: an MCP or A2A client (VS Code, Claude Code, another agent) connected with the
  profile's integration token.

## User stories

- US-1: As an owner, I want to describe an agent in plain words and get a checked blueprint draft, so that I don't hand-write its configuration.
- US-2: As an owner, I want to start from a built-in template, so that common agents begin with sound instructions, capabilities and evals.
- US-3: As an owner, I want an agent's tools, access and data needs resolved against what I already offer, with least-privilege warnings, so that no agent gets more than I grant.
- US-4: As an owner, I want to evaluate a build against its eval suite, built-in adversarial cases included, on real workers and across models, so that I release only agents that prove they work, at a cost I accept.
- US-5: As an owner or approver, I want releases signed, versioned and approved, with a diff and a new approval for any capability increase, so that every change is deliberate and verifiable.
- US-6: As an owner, I want to deploy a release as a digital worker with a canary and rollback, so that running agents stay healthy.
- US-7: As a plan author, I want to use factory agents in plan steps by name or by exact version, so that steps get proven behavior.
- US-8: As an owner, I want per-agent metrics and budgets, so that I see quality, usage and cost, and stop runaway agents.
- US-9: As an owner, I want scheduled evaluations to catch drift, and the factory to propose improvements I approve, so that agents get better without changing by themselves.
- US-10: As a peer, I want to import an agent another owner published to me, verify it and evaluate it on my own accounts, so that good agents spread safely.
- US-11: As an owner, I want to deprecate and retire agents, so that old versions stop being used while their records stay.
- US-12: As an editor agent, I want factory tools to draft, check and evaluate agents, so that I can help build them while releases and grants stay with the owner.
- US-13: As an owner, I want an agent to read chosen knowledge and keep lessons only from its verified work, so that it improves with use without hidden state.
- US-14: As an owner, I want to keep an agent's blueprint, evals and fixtures in a repository, checked by built-in checks and released through a plan with a red-team review and sign-offs, so that agents are developed and reviewed like code.

## Acceptance criteria

- AC-1 (US-1): Given a signed-in provider, when the owner describes an agent, then the factory returns a draft blueprint written by the owner's default model that passes validation, with the provider, model and effort that wrote it recorded as its provenance.
- AC-2 (US-1): Given the model's draft has problems, when the factory checks it, then the draft goes back to the model with the problems, at most 3 tries in all; after that the owner gets the last draft with each problem and its fix, and it can't be built until they are fixed.
- AC-3 (US-1): Drafting must never grant anything: the drafting model works read-only with no tools, and a draft changes no share, MCP offer, allowlist entry or model setting.
- AC-4 (US-2): Given the built-in templates, when the owner lists them, then each shows its purpose, its most access and its number of eval cases, and a blueprint made from any template passes validation unchanged.
- AC-5 (US-3): Given a blueprint asks for an MCP server, check or Work IQ source that its deployment folders don't offer, when it is checked, then the problem names the capability and the command that offers it, and nothing is offered automatically.
- AC-6 (US-3): Given a blueprint asks for more than its requirements need (full access with no check, every tool of a server, every Work IQ source), when it is checked, then it gets a least-privilege warning naming the narrower choice.
- AC-7 (US-3): A factory agent must never delegate to other agents or peers unless its blueprint asks to and the owner allows it on the worker's allowlist, and an agent file's tools and model fields must never grant anything.
- AC-8 (US-4): Given a build and its eval suite, when the owner starts an evaluation, then the factory first shows the estimate (cases, repetitions, candidates, tasks and minutes) and runs nothing until the owner confirms it.
- AC-9 (US-4): Given a confirmed evaluation, when it runs, then each case runs as a real delegated task on the owner's own worker or a chosen peer, under the same access, sandbox and MCP rules as any task, 3 times by default.
- AC-10 (US-4): Given an eval suite, when it is checked, then every requirement must have at least one case with a deterministic expectation (contains, not-contains, regex, JSON schema, files changed or a folder check).
- AC-11 (US-4): Given any build, when it is evaluated, then the built-in adversarial cases (prompt injection, secret exfiltration, path escape and out-of-scope requests) run too, and the gate needs every one of them to pass.
- AC-12 (US-4): Given candidates on several models, when the evaluation ends, then the report shows for each its pass rate per requirement, p95 duration, attempts and cost multiplier, and recommends the cheapest candidate that meets every threshold.
- AC-13 (US-4): Given a case with a judge rubric, when it is graded, then a model of another provider or model family scores it with the output inside the external-content envelope, and the score must never decide the gate on its own.
- AC-14 (US-5): Given a build whose evaluation passed the gate, when the owner releases it, then the release gets a semantic version and a manifest of content hashes (blueprint, agent, skills, evals, report, provenance) signed with the owner's identity key.
- AC-15 (US-5): Given a stored release, when anyone runs factory verify, then every hash and signature is checked offline, and a changed file makes verify fail and the release unusable.
- AC-16 (US-5): Given a release that adds capabilities to the active one, when it is requested, then it waits for the owner's sign-off on the capability diff, even after every other approver accepted it.
- AC-17 (US-5): Given approvers in the blueprint, when a release is requested, then each gets a sign-off request with the diff, the eval summary and the provenance; the release happens only after all accept, and a rejection keeps the active release.
- AC-18 (US-6): Given a release, when the owner deploys it, then a digital worker runs it with the release's instructions and skills on every task, its model policy and its folders, and peers see the agent's name and version.
- AC-19 (US-6): Given a new deployment, when the canary's smoke cases fail, then the deployment goes back to the previous release (or the worker stops if there is none), and the owner sees why.
- AC-20 (US-6): Given an active release, when the owner rolls back, then the previous release is active on every deployment within a minute, and the rollback is audited.
- AC-21 (US-7): Given a released factory agent, when a plan step says agent=name, then the step uses the active release; with agent=name@version it uses that release, and the plan check refuses an unknown version.
- AC-22 (US-8): Given tasks run by a deployed agent, when the owner opens its metrics, then they see per version: tasks, done, failed, refused, p50 and p95 duration, attempts, check pass rate, model mix and usage where the provider reports it, and never prompt text.
- AC-23 (US-8): Given a deployed agent's budget (tasks per day, attempts, timeout), when a task would exceed it, then the worker refuses it before any model call with a one-line reason, and the refusal is audited.
- AC-24 (US-9): Given a drift watch, when a scheduled evaluation's pass rate falls below the gate the configured number of times in a row, then an incident opens and the owner is alerted with the failing cases.
- AC-25 (US-9): Given failing cases or failed tasks, when the owner asks for an improvement, then the factory proposes a new draft version with a diff and evaluates it on the same suite plus regression cases, and the active release must never change without a release.
- AC-26 (US-10): Given an owner publishes a release to a peer, when the peer lists its catalog, then it sees only the releases granted to it (name, version, description, most access and eval summary), never knowledge topics, eval outputs or paths.
- AC-27 (US-10): Given a peer imports a published release, when the bundle arrives, then its signature and hashes are verified, nothing runs, and it can't be activated until it passes an evaluation on the peer's own accounts, unless the peer explicitly accepts the publisher's report.
- AC-28 (US-11): Given a deprecated agent, when a plan uses it, then the plan check warns and names its replacement; given a retired agent, then the plan check refuses it, its deployments stop, and its releases and reports are kept for audit.
- AC-29 (US-12): Given an editor agent with the integration token, when it calls factory tools, then it can list, draft, check and evaluate agents, and every release, deploy, rollback, retire or capability approval is refused with the owner's command to run instead.
- AC-30 (US-13): Given a deployed agent whose blueprint names context topics, when it runs a task, then those topics are in the task's prompt (at most 24,000 characters), and a topic with Microsoft 365 labels marks the task's output with them.
- AC-31 (US-13): Given a deployed agent with a lessons topic, when one of its tasks is verified by its checks, then a lesson is added to the topic with the task's labels; an unverified result must never add a lesson, and lessons older than the blueprint's retention_days are pruned.
- AC-32 (US-14): Given a folder made by factory init, when a plan step runs verify=agent:blueprint,agent:evals, then the checks pass for a valid blueprint and suite, and fail naming each problem and its fix otherwise.
- AC-33 (US-14): Given the agent-factory playbook, when it runs, then the agent is evaluated (agent:report), reviewed by a red-team squad, signed off, and released from the folder's commit, and the release's provenance names that commit.

## Non-functional requirements

- Security: the owner grants, never the factory: capabilities are requests resolved against
  the owner's shares, MCP offers and grants, and any increase needs the owner's sign-off.
  Releases are content-addressed and signed with the profile's ed25519 identity key.
  Imports run nothing. Adversarial evals are mandatory. The integration token can't release,
  deploy, roll back, retire or approve. Every factory model call and eval task runs under
  Proofkin's existing rules: access levels, OS sandbox, credential isolation, secret
  redaction and the external-content envelope.
- Privacy: descriptions, blueprints, eval inputs and outputs stay in the owner's profile.
  The audit keeps no prompt or output text. Microsoft 365 sensitivity labels follow memory
  and outputs as they do for plans. Eval outputs are kept 30 days by default; reports keep
  hashes, verdicts and metrics after that. Publishing never includes knowledge topics or eval
  outputs. People's names appear only in approvals and grants.
- Reliability: evaluations and deployments survive an agent restart (journaled like plan
  runs); a failed canary rolls back by itself; releases are immutable.
- Performance: factory API calls that don't wait for a model answer within 2 seconds at the
  95th percentile; checking a blueprint takes under 1 second; evaluations respect each
  serving agent's task slots.
- Cost: every evaluation shows its estimate before it runs; each deployed agent has budgets;
  usage is shown where providers report it.
- Compatibility: a factory agent is an ordinary agent file and digital worker, so older peers
  see a worker with a role; no new infrastructure (one Rust binary, profile files, iroh).
- Accessibility: the web UI meets WCAG 2.2 AA as `docs/ux-guidelines.md` requires.

## Out of scope

- A hosted, multi-tenant factory service, and the brief's suggested databases, message
  brokers, workflow engines and Kubernetes (see ADR-0001).
- Vector databases and embeddings in the first version (see ADR-0006).
- Fine-tuning or training models.
- Agents that change their own instructions, tools or models while they run.
- Billing for provider usage beyond what providers report to the agent.

## Open questions

- Should the default gate be a 90% pass rate overall, or 2 of 3 repetitions per requirement?
- How is an agent with its own team plan (a multi-agent workflow) evaluated: as one case, or
  step by step?
- Should imported agents keep the publisher's version numbers, or get a local suffix?
