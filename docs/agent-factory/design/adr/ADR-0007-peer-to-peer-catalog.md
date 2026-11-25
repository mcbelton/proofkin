# ADR-0007: Agents are shared peer to peer as signed releases

## Status

Proposed

## Context

The brief's long-term vision is an ecosystem of reusable agents. Proofkin has no central
server: peers are people on each other's allowlists, and every message is authenticated by
key. Plugins already show how to take in someone else's definitions safely: importing
copies files and runs nothing, and anything that would run is listed and left out.

## Decision

- An owner **publishes** chosen releases to chosen peers. Granted peers see a **catalog**
  of them (name, version, description, most access and an eval summary) in the agent's
  hello and whenever it changes. Knowledge topics, eval outputs and paths are never sent.
- A peer **imports** a release by asking for its bundle over iroh. The bundle's signature
  must be the publisher's key and every hash must match; importing runs nothing.
- An imported agent can't be activated until it passes an **evaluation on the importer's own
  accounts**, unless the importer explicitly accepts the publisher's report.
- Peers can also simply **delegate** to the publisher's deployed agent, without importing it.

## Alternatives considered

- **A central marketplace**: easy discovery, but a server to run and trust, and a single
  place where everyone's agents could be tampered with.
- **Sharing through git repositories or plugins**: works today for skills and agent files,
  but carries no evaluation, signature or capability summary.
- **Trusting the publisher's evaluation by default**: faster, but models, accounts and
  folders differ between people; the agent must prove itself where it will run.

## Consequences

- Good agents spread between people who trust each other, with proof attached.
- Each importer pays for one evaluation before use, unless they choose to accept the
  publisher's report.
- Catalog messages are new protocol messages; older agents ignore them.
