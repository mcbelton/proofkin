# ADR-0005: A deployment is a digital worker pinned to a release

## Status

Proposed

## Context

The brief's runtime executes agents and its deployment makes them available through an
API. Proofkin's digital workers already are autonomous agents with their own key,
allowlist, folders, hourly limits and audit, running on their owner's accounts and
supervised by the owner's agent. Today a worker carries a role of at most 200 characters,
given to the model with every task.

## Decision

- Deploying a release creates (or updates) a digital worker whose profile **pins**
  `agent: name@version`. Every task it runs gets the release's instructions and skills ahead
  of the task, its model policy becomes the worker's model settings, and its folders are
  shared as the owner shares them.
- Peers see the worker as `[agent fin-analyst 1.2.0 of alice]` and delegate to it as to any
  worker; their grants, the folder's access and the release's budgets all apply.
- A **canary** runs the release's smoke cases on the new deployment before it takes work.
  If they fail, the deployment goes back to the previous release, or the worker stops when
  there is none, and the owner is told why.
- **Rollback** makes an earlier release active on every deployment of the agent within a
  minute; it is audited.

## Alternatives considered

- **A new agent runtime** separate from workers: duplicates keys, allowlists, supervision
  and audit, and peers would need to learn a new kind of agent.
- **Only `agent=` in plans, no deployment**: works for plans the owner writes, but peers
  couldn't delegate to an agent, and nothing would pin the version a peer gets.
- **Blue-green with two workers per agent**: zero-downtime switches, but doubles the
  workers and their keys; the canary on the one worker, with the previous release ready to
  restore, is enough for one person's agents.

## Consequences

- Deployed agents get everything workers have: isolation, allowlists, audit, live status.
- Older peers keep working: they see a worker with a role.
- During a canary the worker refuses new tasks with a clear message; plans' squad steps move
  to another member as they do today.
