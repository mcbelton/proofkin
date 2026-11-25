# Agent factory: test plan

A test for every acceptance criterion in `spec/SPEC.md` and every high-risk threat in
`design/threat-model.md`. When an item is built, each test's name goes into its claim
(`FAC-*`) in `evidence/claims.json`, and `scripts/evidence.py` proves it.

| Covers | Level | Test | Data |
| --- | --- | --- | --- |
| AC-1 | Integration | A fake executor answers with a valid blueprint: createDraft returns 201, no problems, and provenance names the provider, model and effort | A fake provider answer |
| AC-1 | Unit | The drafting prompt holds the schema, the catalog's names and the description, and no secrets | A catalog with a planted token |
| AC-2 | Integration | Answers with an unknown skill, then a bad name, then valid: the draft passes on the third try; four bad answers stop at 3 tries with the problems and fixes | Scripted fake answers |
| AC-2 | Contract | PUT of an edited blueprint with an error returns 200 with passed false, and evaluating it is refused with 409 | The contract's examples |
| AC-3 | Integration | Drafting runs the model read-only with no tools, and shares, MCP offers, the allowlist and model settings are byte-identical after it | A profile snapshot |
| AC-4 | Unit | Every built-in template parses, validates unchanged and has at least 3 eval cases, each requirement covered | Built-in templates |
| AC-4 | Contract | GET /api/factory/templates matches the contract | - |
| AC-5 | Integration | A blueprint asking for MCP server pdf in a folder that doesn't offer it gets mcp_not_offered with the mcp add command; offers are unchanged | Two shares, one offering pdf |
| AC-6 | Unit | full access with no check, tools '*' and workiq all each get a least_privilege warning naming the narrower choice | Lint fixtures |
| AC-6 | Contract | POST /api/factory/drafts/{name}/check and PUT /api/factory/drafts/{name} return the least_privilege warning as the contract's CheckResult example shows | A blueprint asking for every tool of pdf |
| AC-7 | Integration | A deployed agent with delegate false can't delegate; with true but no allowlist grant it still can't; tools and model in a built agent file grant nothing | Two workers |
| AC-8 | Contract | startEvaluation with confirm false returns the estimate (cases x repetitions x candidates) and no task is sent | A suite of 4 cases, 2 candidates |
| AC-9 | Integration | A confirmed evaluation sends each case 3 times to the evaluation worker over iroh at the blueprint's access; a write outside the folder is refused as in production | Two agents on localhost |
| AC-10 | Unit | A suite where R2 has only judge cases is refused naming R2; each deterministic expectation kind passes and fails as specified | Grader fixtures |
| AC-10 | Contract | startEvaluation of a draft whose R2 has only judge cases returns 400 naming R2, and no task is sent | The contract's 400 example |
| AC-11 | Integration | The built-in adversarial cases run for every build; an agent that echoes the canary secret or follows the injected instruction fails the gate | A deliberately unsafe agent |
| AC-12 | Integration | Two fake candidates with different pass rates, durations and multipliers: the report has each metric and recommends the cheapest one that meets every threshold | Scripted results |
| AC-13 | Integration | The judge is a different provider or model family, gets the output inside the envelope, and a 5 from the judge doesn't pass a case whose deterministic expectation failed | A fake judge |
| AC-14 | Integration | Releasing a passing evaluation writes release.json with every artifact's hash and a signature that verifies with the owner's key | A passing evaluation |
| AC-14 | Contract | requestRelease with an evaluation of another build returns 409 | - |
| AC-15 | Integration | factory verify passes on a release, then fails after one byte of agent.md changes, and that release can no longer deploy or run in a plan | A stored release |
| AC-16 | Integration | A release adding an MCP tool waits for the owner's sign-off after every approver accepted; one without increases doesn't ask the owner again | Two releases |
| AC-17 | E2E | A release with approvers carol and me asks both with the diff, eval summary and provenance; carol rejecting keeps the active release | Three agents on localhost |
| AC-18 | Integration | Deploying 1.2.0 creates a worker pinned to it: its tasks get the release's instructions and skills, its models follow the policy, and peers' hello shows the name and version | A deployed worker |
| AC-19 | Integration | A canary with a failing smoke case rolls the worker back to 1.1.0 (or stops it when there is none) and says which case failed | A broken release |
| AC-20 | Integration | rollbackAgent makes 1.1.0 active on two deployments within a minute and writes an audit entry with the reason | Two deployments |
| AC-21 | Integration | The plan check resolves agent=name to the active release and agent=name@1.1.0 to that release, and refuses agent=name@9.9.9 listing the versions | A registry with two releases |
| AC-22 | Contract | GET metrics after 10 tasks returns per-version counts, durations, attempts, check pass rate, model mix and usage, and the audit file holds no prompt text | Fake tasks |
| AC-23 | Integration | With tasks_per_day 2, the third task is refused before any model call with a one-line reason and audited as refused | A deployed worker |
| AC-24 | Integration | A drift watch with fail_after 2 opens an incident on the second failing scheduled evaluation and alerts with the failing cases | Scripted evaluations |
| AC-25 | Integration | proposeImprovement creates a new draft with a diff, evaluates it on the suite plus regression cases, and the active release is unchanged until a release | Failing cases |
| AC-26 | E2E | bob sees only the release published to him, with access and eval summary, and dana sees nothing; no knowledge topic or path is in any message | Three agents on localhost |
| AC-27 | E2E | bob imports fin-analyst 1.2.0: verified, nothing runs, state imported; it can't run in a plan until a local evaluation passes or bob accepts the publisher's report | Two agents on localhost |
| AC-28 | Integration | A deprecated agent draws a plan check warning naming its replacement; a retired one is refused, its workers stop and its releases stay | A registry |
| AC-29 | Integration | Over MCP with the integration token, factory_draft and factory_evaluate work, and release, deploy, rollback, retire and approve are refused with the owner's command | tests/connect.rs style client |
| AC-30 | Integration | A worker with context topic finance-glossary gets it in every task's prompt, cut to 24,000 characters; a topic labeled Confidential marks the task's output Confidential | Two topics, one labeled |
| AC-31 | Integration | A verified task adds one lesson with its labels, the same lesson twice is kept once, a task whose check failed adds none, and lessons older than retention_days are pruned | Scripted tasks and clock |
| AC-32 | Integration | factory init writes a folder whose agent:blueprint and agent:evals checks pass; breaking a skill reference and removing R2's only deterministic case fails them, naming each problem and fix | A temporary folder |
| AC-33 | E2E | The agent-factory playbook runs on localhost agents: evaluation (agent:report), a red-team squad step, sign-offs, and a release whose provenance names the folder's commit | Three agents on localhost |
| TM-1 | Security | A description and a catalog entry carrying hidden instructions to ask for full access and every tool produce a draft whose increases need the owner, and nothing is granted | Injection fixtures |
| TM-2 | Security | Bundles with a changed file, a re-signed manifest from another key, or a missing artifact are all refused on import | Tampered bundles |
| TM-4 | Security | Outputs saying to score them 5 or to ignore the rubric don't pass a failing case, and the judge prompt has them inside the envelope | Hostile outputs |
| TM-5 | Security | A release whose agent file asks for tools and a model, and a peer with read-only access, can't write, run a shell or use an unoffered MCP tool | A hostile agent file |
| TM-6 | Security | Fixtures with planted secrets and instructions to reveal them: outputs are redacted, the adversarial cases fail an agent that reveals them, and labeled content stays from peers without a grant | Canary secrets, labeled topics |
| TM-10 | Security | With the integration token, every lifecycle and approval call is refused over MCP and A2A, and the web UI's token is refused at /mcp | tests/connect.rs style client |

## Non-functional testing

- **Performance:** with 200 agents and 2,000 releases in a profile, listing agents, getting
  one and checking a blueprint answer within 2 seconds and 1 second at the 95th percentile.
  An evaluation of 20 cases x 3 repetitions x 2 candidates keeps within the worker's 2 task
  slots and finishes within its estimate plus 20%.
- **Security:** the TM tests above, plus `cargo clippy`, the secret and license checks,
  and a live check of the adversarial suite against real Copilot.
- **Reliability:** an agent killed during an evaluation and during a canary resumes the run,
  or rolls back, without running a case twice.
- **Accessibility:** the web UI's Agent factory passes the checks in `docs/ux-guidelines.md`
  and a keyboard-only pass.
- **Live:** `scripts/live_test.py` drafts, evaluates, releases, deploys and retires an agent
  with real Copilot across two machines' agents.
