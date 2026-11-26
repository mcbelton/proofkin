---
name: agent-evals
description: Write an agent's eval suite (agent/evals.json): a case proving each requirement with deterministic expectations, fixtures and smoke cases
---
# Agent eval suite

Write or update `agent/evals.json` for the agent in `agent/blueprint.json`. Checked by
`agent:evals`; its evaluation's signed report, kept in `agent/reports/`, by `agent:report`.

## Cases
Each case: `id` (`E1`...), the `requirement` it proves, the task as `input`, its `fixtures`
(files in `agent/fixtures/` it works on), its `expect`ations, and a `timeout` (e.g. `10m`).
- Every requirement has at least one case, and each case at least one deterministic
  expectation: `contains`, `not-contains`, `regex`, `json-schema`, `max-words`,
  `files-changed` (edit access) or a folder `check` (full access).
- A `judge` rubric (with `min` 1 to 5) adds a score from a model of another family; it never
  decides whether a run passes, so it is never the only expectation.
- Mark one or two quick, representative cases `"smoke": true`: a deployment's canary runs
  them before it takes work.
- Keep fixtures small and made up: no real customer data, credentials or secrets.
- The built-in attacks (hidden instructions, a canary secret, a file outside the folder, an
  off-topic request) are added to every evaluation: don't copy them.

## Suite settings
`repetitions` (runs per case and model, 3 by default) and `thresholds` (`pass_rate`,
`requirement_pass_rate`, `p95_secs`) only when the defaults don't fit.

When asked to summarize a report in `agent/reports/`, say whether it passed the gate, each
requirement's pass rate, the built-in attacks and the recommended model; change no file.
