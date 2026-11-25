# ADR-0001: Build the factory into Proofkin's local-first runtime

## Status

Proposed

## Context

The LLM Agent Factory brief suggests a hosted platform: Python and FastAPI, LangGraph or
CrewAI, Temporal, an API gateway, PostgreSQL with pgvector or Qdrant, Redis, Kafka or
RabbitMQ, Docker and Kubernetes, OpenTelemetry with Prometheus and Grafana, React, and
OAuth 2.0 with OIDC. It also says these are options, to be chosen by operational need.

Proofkin is one Rust binary per person. Each profile has its own provider accounts, key,
allowlist and files; agents meet over iroh, and work is proven by signed evidence. Much of
the factory's runtime exists already: providers and their sandboxes, plans with
dependencies and restarts (a workflow engine), digital workers (a runtime), shares and MCP
offers (a tool registry with permissions), knowledge (memory), evidence and runs
(evaluation records), audit and operations (observability).

## Decision

The factory is a module of the Proofkin binary, `src/factory/`, with its state in the
profile (`<profile>/factory/`), its interfaces on the existing CLI, web UI, MCP and A2A
endpoints, and its runtime the existing agents and digital workers. The brief's layers map
onto Proofkin's parts as listed in `docs/agent-factory.md` (Technology choices), and no
service, database, broker or cluster is added.

## Alternatives considered

- **The brief's hosted stack**: suits a multi-tenant service, but would move owners'
  credentials and work data to a server, add operations (databases, queues, clusters) no
  Proofkin user runs today, and duplicate the runtime Proofkin already has.
- **A separate factory process next to Proofkin**: keeps the binary smaller, but needs its
  own API, auth and state, and every factory call would cross a process boundary to reach
  workers, shares and evidence.
- **A plugin**: plugins bring skills, agents and MCP templates and run nothing, by design;
  the factory must evaluate, sign and deploy, so it can't be one.

## Consequences

- Owners get the factory with nothing to install or operate, and their data stays on their
  machines.
- The factory scales with each person's machines and peers, not as one shared service.
  Organization-wide catalogs are peer to peer (ADR-0007), not a central registry.
- Durable workflows use the plan runner's journal rather than Temporal; long evaluations
  must be journaled the same way.
- Observability starts with the audit, ops reports and the web UI; exporting metrics to
  OpenTelemetry is a later item.
