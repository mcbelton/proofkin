# ADR-0002: Blueprints are declarative data, built into ordinary agent files

## Status

Proposed

## Context

The brief's specification engine turns requirements into a structured agent definition,
and its generator turns that into prompts, tools, memory and workflow configuration.
Proofkin already runs agents from agent files (`agent=name`: instructions, skills, MCP
server names) and leaves out their `tools` and `model` fields, because what a task may do is
the folder owner's decision. Plugins already show that importing definitions can be safe
when it runs nothing.

## Decision

An agent is defined by a **blueprint**: JSON in the `proofkin.agent-blueprint/v1` format
(purpose, requirements, instructions, skills, capabilities, model policy, memory, evals,
budgets, deployment, approvers). A model may write a draft; the owner edits it as data. The
**builder** turns a blueprint into ordinary artifacts, deterministically: an agent file, the
skills it brings (namespaced `<agent>.<skill>`), and an optional team plan, plus a build hash
over all of them. A factory agent is then usable wherever agent files are, as a third
source after the profile's own agents.

## Alternatives considered

- **Generated code** (a program per agent, as agent frameworks do): flexible, but code must
  be reviewed and sandboxed as code, and couldn't be shared with peers safely.
- **Let the model write the agent file directly, without a blueprint**: simpler, but leaves
  requirements, capabilities, evals and budgets implicit, so nothing could be checked or
  diffed before release.
- **YAML blueprints**: friendlier to edit, but JSON matches the API, the web UI's editor and
  the rest of the profile's files; the UI edits it as a form.

## Consequences

- Blueprints can be validated, linted, diffed and hashed; capability changes are visible.
- Builds are reproducible, so a release names exactly what was evaluated.
- Existing plans, the plan check and peers keep working: a built agent is an agent file.
- The format is versioned; a future v2 needs a migration and both readers for a while.
