# ADR-0006: Memory is knowledge topics and verified lessons

## Status

Proposed

## Context

The brief lists working, conversation, episodic, semantic, vector and persistent memory.
Proofkin has: the step's context (earlier steps' results, cut to size), knowledge topics
(`context=` reads them, `save=` writes them from verified results only), troubleshooting
guides, run records with signed evidence, and the profile's files. Knowledge made from
Microsoft 365 content keeps its sensitivity labels. Vector search needs embeddings, which
means another model call per write and read, an index to keep, and content leaving the
machine to an embedding service.

## Decision

A blueprint's memory says which **knowledge topics** the agent reads (semantic memory),
which topic its **lessons** go to, and how long lessons are kept. Lessons are saved only
from verified results (episodic memory, as `save=` does), with their labels, deduplicated
and capped in size. Working memory is the task's context as today; persistent memory is
the profile. Publishing a release never includes knowledge. Vector memory is not in the
first version; when topics outgrow the 24,000-character context, the next step is local
lexical retrieval of the relevant sections, with no new dependency or service.

## Alternatives considered

- **A vector database** (pgvector, Qdrant): good semantic recall, but a service to run and
  an embedding call for every read and write, with content leaving the machine.
- **Local embeddings**: no service, but a large model to ship and keep current per
  platform.
- **Conversation memory per agent**: Proofkin tasks are self-contained by design, so each
  is reproducible from its prompt; keeping chats would make results depend on hidden state.

## Consequences

- Agents learn only from verified work, and what they learned is readable Markdown the
  owner can edit or delete.
- Very large knowledge bases are cut to the context limit until lexical retrieval exists.
- Memory stays private to its owner, and labels keep Microsoft 365 content from peers that
  may not have it.
