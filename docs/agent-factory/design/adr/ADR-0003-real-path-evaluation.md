# ADR-0003: Evaluate on the real delegation path; checks prove, judges score

## Status

Proposed

## Context

The brief's evaluation engine measures accuracy, reliability, task completion,
hallucination, tool use, latency, tokens, cost and policy compliance, before and after
deployment. Model output varies from run to run, and a model asked to grade another
model's output can be manipulated by that output. Proofkin's rule so far: context is never
proof; only checks prove a claim, and their evidence is signed by the agent that ran them.

## Decision

- An evaluation runs each case as a **real delegated task** with the candidate build, on an
  evaluation worker (or a chosen peer), under the same access levels, OS sandbox, MCP offers,
  redaction and envelope as production. No separate harness.
- Each case runs **3 times** by default, per candidate model; the report keeps every run's
  verdict, duration, attempts and the worker's signed evidence.
- **Deterministic expectations** (contains, not-contains, regex, JSON schema, files changed,
  a folder check) decide the gate. Every requirement needs at least one.
- **Judge rubrics** are scored by a model of another provider or model family, with the
  output inside the external-content envelope, median of 3. Judge scores are shown and
  trended, and never decide the gate on their own.
- The **built-in adversarial cases** run for every build; the gate needs all of them.
- The owner sees an **estimate** (tasks and minutes) before anything runs.

## Alternatives considered

- **A mock harness calling models directly**: faster and cheaper, but it would not test the
  sandbox, tool allowlists, MCP offers or the worker's own behavior, which is where agents
  fail in practice.
- **LLM-as-judge as the gate**: covers open-ended quality, but is gameable and itself
  nondeterministic; it would make releases depend on unproven verdicts.
- **One run per case**: cheaper, but hides flakiness; a 3-run default shows variance at a
  known cost, and owners can lower it.

## Consequences

- Evaluations cost real model calls; estimates, budgets and the evaluation worker keep that
  visible and bounded.
- Releases rest on evidence Proofkin can verify offline, like plan runs.
- Open-ended qualities need either a deterministic proxy or a person's sign-off at release;
  judges inform, they don't decide.
