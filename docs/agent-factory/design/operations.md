# Agent factory: operations

How the factory is released, watched and fixed, on owners' machines. There is no service
to operate; "operations" are what the owner's agent watches for each deployed agent, and
how Proofkin releases ship the factory.

## Rollout

- The factory ships in phases (see `docs/agent-factory.md`, Roadmap), each in a Proofkin
  release with its claims proven and a live check passed.
- The profile's factory folder is new; nothing existing is migrated. Agent files, workers
  and plans keep working as before.
- For an owner's agent, rollout is the deployment flow: canary first, then the release takes
  work; a peer only sees a new version after its canary passed.

## Rollback

- **A bad agent release:** `proofkin factory rollback <agent>` (or **Roll back** in the web
  UI) makes the previous release active on every deployment within a minute; a failed
  canary does it by itself.
- **A bad Proofkin release:** install the previous version. Factory files carry their
  format version; an older Proofkin leaves `factory/` alone, and deployed workers keep
  running as plain workers with their role.
- **A bad import:** `proofkin factory delete <agent>` deletes it with its imported bundles
  before it is active; nothing ran, so there is nothing to undo. Once active, retire it, or
  roll back to another version imported from its publisher.

## Monitoring

- SLO: 99% of factory API calls that don't wait for a model answer within 2 seconds.
- SLO: 99% of canaries reach a verdict within 15 minutes of a deployment.
- SLO: each deployed agent's drift watch passes its gate on at least 95% of scheduled runs
  over 28 days.
- Metrics, per agent and version, from the audit and evaluations: tasks, done, failed,
  refused (by reason: budget, access, canary), p50 and p95 duration, attempts, check pass
  rate, model mix, usage where reported, eval pass rate and judge score over time.
- Dashboard: the web UI's **Agent factory** card (each agent's state, version, deployments,
  canary, last evaluation and trend) and `proofkin factory metrics <agent>`; `proofkin ops
  report` adds a factory section.
- The audit records every lifecycle change: drafts, evaluations, releases, approvals,
  deployments, rollbacks, deprecations, retirements, publications and imports.

## Alerts

| Alert | Condition | Severity | Action |
| --- | --- | --- | --- |
| Canary failed | A deployment's smoke cases failed and it rolled back | Sev 2 | Runbook: canary failed |
| Drift | The drift watch failed its gate fail_after times in a row | Sev 2 | Runbook: drift |
| Budget reached | A deployed agent refused tasks for its daily budget | Sev 3 | Runbook: budget reached |
| Release integrity | A release failed verification when loaded | Sev 1 | Runbook: integrity |
| Evaluation stuck | An evaluation ran past its estimate by 50% | Sev 3 | Runbook: evaluation stuck |

Alerts reach the owner as ops alerts do today: in the web UI, in the terminal, and by chat
to the owner's agent.

## Runbook

### Canary failed

1. Open the deployment: which smoke cases failed, and with what output?
2. Compare with the evaluation report of the same release: if it passed there, the worker's
   folders, MCP offers or model settings differ; fix them and deploy again.
3. Otherwise evaluate the release again; if it fails, keep the rollback and improve it.

### Drift

1. Open the drift trend: which requirements fell, since when, and on which model?
2. If the provider changed the model, evaluate the other candidates and release the policy
   that passes.
3. Otherwise run **Improve** on the failing cases and release the proposal if it passes.

### Budget reached

1. Check the metrics: who sent the tasks, and were they expected?
2. Raise the budget for real demand, or lower the peer's hourly limit for unexpected use.

### Integrity

1. Run `proofkin factory verify <agent>` to see which file changed.
2. Don't deploy or use that release; roll back to a release that verifies.
3. Find out what changed the profile's files (malware scan, backups) before releasing again.

### Evaluation stuck

1. Check the evaluation worker's live status and its queue.
2. Cancel the evaluation, lower repetitions or candidates, and run it again.
