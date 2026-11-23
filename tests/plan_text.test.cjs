// Cases for the plan editor (ui/plan-text.js): its palette, the plan check's fixes and the
// builder's model. tests/ui.rs runs this with node.
// "|" marks the cursor before a click; the result shows the selection in [brackets].
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

// Loaded the way the page loads it: a classic script, in a context of its own.
const code = fs.readFileSync(path.join(__dirname, "..", "ui", "plan-text.js"), "utf8");
const planText = vm.runInNewContext(`${code}\n;planText`, {});
const defaults = { target: "@worker:repo", check: "test" };
const { ERRORS } = planText;

function show(plan, change) {
  if (change.error) return `error: ${change.error}`;
  const out = plan.slice(0, change.from) + change.insert + plan.slice(change.to);
  const [start, end] = change.select;
  return `${out.slice(0, start)}[${out.slice(start, end)}]${out.slice(end)}`;
}
const cursor = (text) => [text.replace("|", ""), text.indexOf("|")];
const part = (text, value) => {
  const [plan, pos] = cursor(text);
  return show(plan, planText.addPart(plan, pos, value, defaults));
};
const line = (text, what, given = defaults) => {
  const [plan, pos] = cursor(text);
  return show(plan, planText.addLine(plan, pos, what, given));
};
const fixed = (text, edits) => planText.applyFix(text, edits);

const cases = [
  // Skills clicked in an empty plan start a step, then add up.
  ["skill in an empty plan", part("|", "skill=adr"), "step1 @worker:repo skill=adr [What to do]"],
  ["a second skill", part("step1 @worker:repo skill=adr |What to do", "skill=api-contract"),
    "step1 @worker:repo skill=adr,api-contract [What to do]"],
  ["the same skill again", part("step1 @worker:repo skill=adr What| to do", "skill=adr"),
    "step1 @worker:repo skill=adr [What to do]"],
  ["a line that starts with options", part("skill=adr skill=api-contract|", "skill=code-review"),
    "step1 @worker:repo skill=adr,api-contract,code-review [What to do]"],
  // Options go before what to do, wherever the cursor is.
  ["option after the task text", part("fix @alice:app verify=test Make it work|", "skill=implement-change"),
    "fix @alice:app verify=test skill=implement-change Make it work[]"],
  ["option after using", part("fix @alice:app using claude Make| it work", "verify=lint"),
    "fix @alice:app using claude verify=lint Make it work[]"],
  ["one-value option replaced", part("fix @alice:app verify=test attempts=2 Do| it", "attempts=3"),
    "fix @alice:app verify=test attempts=3 Do it[]"],
  ["topic placeholder selected", part("fix @alice:app Do it|", "save=topic"), "fix @alice:app save=[topic] Do it"],
  ["indentation kept", part("  fix @x d|o", "skill=s"), "  fix @x skill=s do[]"],
  // Who runs it replaces who did; a sign-off is by a person.
  ["who runs it replaced", part("fix @alice:app verify=test Do| it", "@bob:web"), "fix @bob:web verify=test Do it[]"],
  ["plain words become what to do", part("Fix the login bug|", "@bob:web"), "step1 @bob:web Fix the login bug[]"],
  ["sign-off by the peer", part("ok after fix approve @me claims=C-1 Approve| it", "@carol:app"),
    "ok after fix approve @carol claims=C-1 Approve it[]"],
  ["no squad sign-off", part("ok approve @me Approve| it", "#qa"), `error: ${ERRORS.squadSignoff}`],
  ["no checks on a sign-off", part("ok approve @me Approve| it", "verify=test"), `error: ${ERRORS.signoffOption}`],
  ["no Microsoft 365 search on a sign-off", part("ok approve @me Approve| it", "workiq=all"),
    `error: ${ERRORS.signoffOption}`],
  ["workiq sources go before what to do", part("a @x Do| it", "workiq=email"), "a @x workiq=email Do it[]"],
  ["workiq sources add up", part("a @x workiq=email Do| it", "workiq=files,teams"),
    "a @x workiq=email,files,teams Do it[]"],
  ["the same workiq source again", part("a @x workiq=email Do| it", "workiq=email"), "a @x workiq=email Do it[]"],
  ["workiq in a plan comes back as it was",
    planText.formatPlan(planText.parsePlan("a @x workiq=email,files verify=test Do it")).text,
    "a @x workiq=email,files verify=test Do it"],
  ["no agent on a sign-off", part("ok approve @me Approve| it", "agent=sec"), `error: ${ERRORS.signoffOption}`],
  // @workiq: the plan's own agent asks Microsoft 365; the answer is context, never proof.
  ["@workiq replaces who runs it", part("a @x Do| it", "@workiq"), "a @workiq Do it[]"],
  ["@workiq has no folder", part("a @x Do| it", "@workiq:app"), "a @workiq Do it[]"],
  ["a Work IQ agent id on @workiq", part("a @workiq Do| it", "agent=sales-agent:v1.2@acme"),
    "a @workiq agent=sales-agent:v1.2@acme Do it[]"],
  ["no checks on @workiq", part("a @workiq Do| it", "verify=test"), `error: ${ERRORS.workiqOption}`],
  ["no claims on @workiq", part("a @workiq Do| it", "claims=C-1"), `error: ${ERRORS.workiqOption}`],
  ["no skills on @workiq", part("a @workiq Do| it", "skill=adr"), `error: ${ERRORS.workiqOption}`],
  ["no MCP servers on @workiq", part("a @workiq Do| it", "mcp=docs"), `error: ${ERRORS.workiqOption}`],
  ["no search option on @workiq", part("a @workiq Do| it", "workiq=all"), `error: ${ERRORS.workiqOption}`],
  ["no @workiq for a step with checks", part("a @x verify=test Do| it", "@workiq"), `error: ${ERRORS.workiqOption}`],
  ["no @workiq sign-off", part("ok approve @me Approve| it", "@workiq"), `error: ${ERRORS.workiqKind}`],
  ["after and save fit @workiq", part("a @workiq save=notes Do| it", "after b"), "a after b @workiq save=notes Do it[]"],
  ["@workiq in a plan comes back as it was",
    planText.formatPlan(planText.parsePlan("a @workiq agent=sales:1 save=t What did we decide?")).text,
    "a @workiq agent=sales:1 save=t What did we decide?"],
  ["the builder says what a @workiq step can't have",
    planText.needs(planText.parsePlan("a @workiq claims=C-1 What did we decide?")).map((n) => n.message).join("|"),
    ERRORS.workiqOption],
  ["one agent per step", part("a @x agent=old Do| it", "agent=new"), "a @x agent=new Do it[]"],
  // after: once, in its place, never on the step itself.
  ["after in its place", part("a @x do\nb @x Do| it", "after a"), "a @x do\nb after a @x Do it[]"],
  ["after adds up", part("a @x do\nb after a @x Do| it", "after c"), "a @x do\nb after a,c @x Do it[]"],
  ["no waiting for itself", part("a @x Do| it", "after a"), `error: ${ERRORS.ownStep("a")}`],
  // Elsewhere a part starts a new step: on an empty line there, after a comment, and
  // after the plan's end from a claim (claims come first).
  ["empty first line", part("|\nfix @x do", "skill=s"), "step2 @worker:repo skill=s [What to do]\nfix @x do"],
  ["after a comment", part("// notes|\na @x do", "mcp=browser"), "// notes\nstep2 @worker:repo mcp=browser [What to do]\na @x do"],
  ["from a claim line", part("claim: C-1 x|\nclaim: C-2 y", "skill=adr"),
    "claim: C-1 x\nclaim: C-2 y\nstep1 @worker:repo skill=adr [What to do]"],
  // Whole lines: claims with the claims, steps after the cursor's line, sign-offs on
  // the step at the cursor.
  ["claim after the claims", line("claim: C-1 x\nfix @x d|o", "claim"),
    "claim: C-1 x\nclaim: C-2 [What must be true at the end]\nfix @x do"],
  ["first claim before the steps", line("// plan\nfix @x d|o", "claim"),
    "// plan\nclaim: C-1 [What must be true at the end]\nfix @x do"],
  ["claim in a plan of notes", line("// only a note|", "claim"), "// only a note\nclaim: C-1 [What must be true at the end]"],
  ["step after the cursor's line", line("fix @x d|o\nlast @x do", "step"),
    "fix @x do\nstep3 @worker:repo verify=test [What to do]\nlast @x do"],
  ["step proves a claim if there is one", line("claim: C-1 x\nfix @x d|o", "step"),
    "claim: C-1 x\nfix @x do\nstep2 @worker:repo verify=test claims=C-1 [What to do]"],
  ["no claim for a step without a check", line("claim: C-1 x\nfix @x d|o", "step", { target: "@worker:repo" }),
    "claim: C-1 x\nfix @x do\nstep2 @worker:repo [What to do]"],
  ["sign-off on the step at the cursor", line("fix @x d|o\nship @x do", "signoff"),
    "fix @x do\nreview after fix approve @me [Check the result of fix and approve it]\nship @x do"],
  ["sign-off in an empty plan", line("|", "signoff"), "review approve @me [Approve it]"],
  // The plan check's fixes (Fix in src/plan.rs), made on the text.
  ["the sign-off proves it alone", fixed(
    "claim: C-1 x\nstep1 @worker:repo claims=C-1 skill=adr,api-contract Write\nreview after step1 approve @me claims=C-1 Check",
    [{ do: "remove", step: "step1", option: "claims=C-1" }]),
  "claim: C-1 x\nstep1 @worker:repo skill=adr,api-contract Write\nreview after step1 approve @me claims=C-1 Check"],
  ["one claim of two removed", fixed("a @x claims=C-1,C-2 do", [{ do: "remove", step: "a", option: "claims=C-1" }]),
    "a @x claims=C-2 do"],
  ["a sign-off takes a claim over", fixed("a @x claims=C-1 do\nok after a approve @me claims=C-2 Approve",
    [{ do: "remove", step: "a", option: "claims=C-1" }, { do: "add", step: "ok", option: "claims=C-1" }]),
  "a @x do\nok after a approve @me claims=C-2,C-1 Approve"],
  ["a new sign-off", fixed("a @x claims=C-1 do\nb @x do", [{ do: "remove", step: "a", option: "claims=C-1" },
    { do: "add_line", after: "a", line: "review after a approve @me claims=C-1 Check the result of a and approve it" }]),
  "a @x do\nreview after a approve @me claims=C-1 Check the result of a and approve it\nb @x do"],
  ["a check added", fixed("  a @x claims=C-1 do", [{ do: "add", step: "a", option: "verify=test" }]),
    "  a @x claims=C-1 verify=test do"],
  ["a claim removed", fixed("claim: C-1 x\nclaim: C-2 y\na @x do", [{ do: "remove_claim", claim: "C-2" }]),
    "claim: C-1 x\na @x do"],
  ["the last line removed", fixed("a @x do\nclaim: C-2 y", [{ do: "remove_claim", claim: "C-2" }]), "a @x do"],
];

// The plan builder's model: a plan's text comes back as it was; renames and removals
// keep it consistent; and what still has to be filled in, by card and field.
const playbook = [
  "// Existing project: reverse-engineer it.",
  "// Vars: peer, folder",
  "",
  "claim: KB-1 The architecture is documented",
  "claim: KB-2 Build and test are documented",
  "",
  "architecture @worker:repo skill=reverse-engineer save=architecture Reverse-engineer this codebase.",
  "build @worker:repo skill=build-run-discovery verify=test claims=KB-2 save=build-and-run Find out how to build it.",
  "confirm after architecture,build approve @me claims=KB-1 Check the knowledge.",
].join("\n");
const model = () => planText.parsePlan(playbook);
const text = (plan) => planText.formatPlan(plan).text;
const step = (plan, id) => plan.steps.find((s) => s.id === id);
const edit = (change) => {
  const plan = model();
  change(plan);
  return text(plan);
};
const needs = (plan) => planText.needs(plan).map((n) => `${n.kind}${n.index}.${n.field}: ${n.message}`).join(" | ");
const broken = planText.parsePlan("skill=adr skill=api-contract");
const placeholders = planText.parsePlan(
  "claim: C-1 What must be true at the end\nstep1 @worker:repo skill=adr claims=C-1 What to do\nok approve @me",
);
cases.push(
  ["a plan comes back as it was", text(model()), playbook],
  ["which line is which", JSON.stringify(planText.formatPlan(model()).items.slice(2, 5)),
    JSON.stringify([null, { claim: "KB-1" }, { claim: "KB-2" }])],
  ["a half-written line is a step with parts missing", text(broken), "step1 skill=adr,api-contract"],
  ["what it still needs", needs(broken), "step0.target: Choose who does it | step0.todo: Say what this step should do"],
  ["placeholders count as missing", needs(placeholders),
    "claim0.text: Say what must be true | step0.todo: Say what this step should do"
    + " | step0.claims: Without a check, this step can't prove a claim: pick a check, or let a sign-off prove it"
    + " | step1.todo: Say what to check before signing off"],
  ["a rename reaches the steps that wait for it", edit((p) => planText.renameStep(p, "build", "build-docs")).split("\n").at(-1),
    "confirm after architecture,build-docs approve @me claims=KB-1 Check the knowledge."],
  ["a removed step is no longer waited for", edit((p) => planText.removeStep(p, "architecture")).split("\n").at(-1),
    "confirm after build approve @me claims=KB-1 Check the knowledge."],
  ["a renamed claim is proven under its new id", edit((p) => planText.renameClaim(p, "KB-2", "KB-9")).split("\n").at(-2),
    "build @worker:repo skill=build-run-discovery verify=test claims=KB-9 save=build-and-run Find out how to build it."],
  ["a removed claim is proven by no step", edit((p) => planText.removeClaim(p, "KB-1")).split("\n").at(-1),
    "confirm after architecture,build approve @me Check the knowledge."],
  ["options change where they stand", edit((p) => planText.setValues(step(p, "build"), "skill", ["a", "b"])).split("\n").at(-2),
    "build @worker:repo skill=a,b verify=test claims=KB-2 save=build-and-run Find out how to build it."],
  ["an emptied option goes", edit((p) => planText.setValues(step(p, "build"), "verify", [])).split("\n").at(-2),
    "build @worker:repo skill=build-run-discovery claims=KB-2 save=build-and-run Find out how to build it."],
  ["no waiting in circles", [...planText.dependents(model(), "architecture")].join(","), "confirm"],
  // A team step: the lead's team plan does and proves it.
  ["a team step comes back as it was", text(planText.parsePlan("claim: C-1 x\nbuild after spec team @alice:app levels=2 timeout=2h claims=C-1 Build it")),
    "claim: C-1 x\n\nbuild after spec team @alice:app levels=2 timeout=2h claims=C-1 Build it"],
  ["a team step proves claims without checks of its own",
    needs(planText.parsePlan("claim: C-1 x\nbuild team @alice:app claims=C-1 Build it")), ""],
  ["no checks on a team step", part("build team @alice:app Build| it", "verify=test"), `error: ${ERRORS.teamOption}`],
  ["a team step's timeout", part("build team @alice:app Build| it", "timeout=30m"), "build team @alice:app timeout=30m Build it[]"],
  // model= names the model a step runs on; a sign-off, team step or @workiq step has none.
  ["a step's model", part("a @x Do| it", "model=gpt-5.5"), "a @x model=gpt-5.5 Do it[]"],
  ["a step's model replaces its model", part("a @x model=gpt-5.5 Do| it", "model=claude-sonnet-5.5"), "a @x model=claude-sonnet-5.5 Do it[]"],
  ["no model on a sign-off", part("ok approve @me Approve| it", "model=gpt-5.5"), `error: ${ERRORS.signoffOption}`],
  ["no model on a team step", part("build team @alice:app Build| it", "model=gpt-5.5"), `error: ${ERRORS.teamOption}`],
  ["no model on @workiq", part("a @workiq Do| it", "model=gpt-5.5"), `error: ${ERRORS.workiqOption}`],
  ["a step's model comes back as it was", text(planText.parsePlan("a @x using claude model=claude-opus-5.5 Do it")),
    "a @x using claude model=claude-opus-5.5 Do it"],
  // effort= says how hard the model reasons, with the same limits as model=.
  ["a step's effort", part("a @x model=gpt-5.5 Do| it", "effort=max"), "a @x model=gpt-5.5 effort=max Do it[]"],
  ["a step's effort replaces its effort", part("a @x effort=low Do| it", "effort=max"), "a @x effort=max Do it[]"],
  ["no effort on a sign-off", part("ok approve @me Approve| it", "effort=max"), `error: ${ERRORS.signoffOption}`],
  ["no effort on a team step", part("build team @alice:app Build| it", "effort=max"), `error: ${ERRORS.teamOption}`],
  ["a step's effort comes back as it was", text(planText.parsePlan("a @x model=claude-opus-5.5 effort=max Do it")),
    "a @x model=claude-opus-5.5 effort=max Do it"],
  // How the run is tracked on GitHub: one track: line, kept as written; issue= is a step's.
  ["a tracked plan comes back as it was", text(planText.parsePlan("// The feature\ntrack: #12\nclaim: C-1 x\na @x issue=#15 Do it")),
    "// The feature\ntrack: #12\n\nclaim: C-1 x\n\na @x issue=#15 Do it"],
  ["track: is no step", needs(planText.parsePlan("track: Ship it\na @x Do it")), ""],
  ["tracking a plan", line("a @x Do| it", "track"), "track: #[1]\na @x Do it"],
  ["a plan tracked already", line("track: #4\na @x Do| it", "track"), "track: [#4]\na @x Do it"],
  ["a step's one issue", part("a @x issue=#3 Do| it", "issue=#5"), "a @x issue=#5 Do it[]"],
  // A step's task text goes on in the lines indented under it (src/plan.rs).
  ["task text over several lines comes back as it was",
    text(planText.parsePlan("a @x Do these:\n  - one\n    - nested\n\n  second\nb @x Next")),
    "a @x Do these:\n  - one\n    - nested\n\n  second\nb @x Next"],
  ["a step whose text starts on the next line", text(planText.parsePlan("a @x\n  Alpha\n  - b")), "a @x\n  Alpha\n  - b"],
  ["the task text of a step is all of it",
    planText.taskOf(planText.parsePlan("a @x One\n    - two\n      - three").steps[0]), "One\n- two\n  - three"],
  ["an option-looking word in a continuation line is text",
    JSON.stringify(planText.parsePlan("a @x Do\n  verify=test more").steps[0].options), "[]"],
  ["which lines are a step's text",
    planText.lineKinds("a @x One\n  more\n\n  x\n// c\nb @x Y").map((l) => l.kind).join(","),
    "step,text,text,text,comment,step"],
  ["a part clicked in a continuation line goes on the step's line",
    part("a @x Do these:\n  - o|ne", "verify=test"), "a @x verify=test Do these:[]\n  - one"],
  ["a line added after a step goes after its text",
    fixed("a @x One\n  more\nb @x Y", [{ do: "add_line", step: "a", line: "// hi" }]), "a @x One\n  more\n// hi\nb @x Y"],
  ["extra lines of a step are written indented", (() => {
    const plan = planText.parsePlan("a @x");
    planText.setTask(plan.steps[0], "One\n- two\n\n  - three");
    return text(plan);
  })(), "a @x One\n  - two\n\n    - three"],
  // How a run is tracked: read from its track: line, and set by the choice beside Run plan.
  ["no track: line is a new issue", JSON.stringify(planText.trackOf("a @x Do it")), JSON.stringify({ kind: "new", title: "" })],
  ["track: #12 and 12 work on issue 12", JSON.stringify([planText.trackOf("track: #12\na @x Do it"), planText.trackOf("track: 12")]),
    JSON.stringify([{ kind: "issue", number: 12 }, { kind: "issue", number: 12 }])],
  ["track: OFF leaves it untracked", planText.trackOf("a @x Do it\ntrack: OFF").kind, "off"],
  ["track: a title is a new issue with it", JSON.stringify(planText.trackOf("track: Ship it\na @x Do it")),
    JSON.stringify({ kind: "new", title: "Ship it" })],
  ["a track: line in a step's text isn't the plan's", planText.trackOf("a @x Do it\n  track: off").kind, "new"],
  ["setting it adds the line first", planText.setTrack("a @x Do it", "off"), "track: off\na @x Do it"],
  ["setting it again replaces the line", planText.setTrack("// c\ntrack: off\na @x Do it", "#4"), "// c\ntrack: #4\na @x Do it"],
  ["no value takes it out", planText.setTrack("track: #4\na @x Do it", null), "a @x Do it"],
  ["no value takes out a last line too", planText.setTrack("a @x Do it\ntrack: off", null), "a @x Do it"],
  // The line under the choice says what running the plan does on GitHub.
  ...(() => {
    const repo = { repo: "acme/shop", repo_url: "https://github.com/acme/shop", login: "alice", visibility: "public",
      board: { title: "Shop", url: "https://github.com/users/alice/projects/4" } };
    const say = (track, found, tracker = repo) => {
      const { parts, warn } = planText.trackNote(track, tracker, found);
      return `${warn ? "! " : ""}${parts.map((p) => (typeof p === "string" ? p : `[${p.text}](${p.href})`)).join("")}`;
    };
    const issue = (open) => ({ issue: { number: 12, title: "Customers export orders", url: "https://github.com/acme/shop/issues/12", open } });
    const after = " They move on the [Shop board](https://github.com/users/alice/projects/4) as the steps run, and each result is commented on its issue."
      + " The repository is public: anyone can read them.";
    return [
      ["a new issue: where, who, the board and who can read them", say(planText.trackOf("a @x Do it")),
        `Running it opens an issue in [acme/shop](https://github.com/acme/shop/issues), with a sub-issue per step, assigned to alice.${after}`],
      ["a new issue with a title names it", say(planText.trackOf("track: Ship it\na @x Do it")).split(" in ")[0],
        "Running it opens the issue “Ship it”"],
      ["an existing issue without its number asks for it", say({ kind: "issue", number: null }),
        "Type the number of an issue in [acme/shop](https://github.com/acme/shop/issues): the run works on it, and its steps become sub-issues of it."],
      ["its number is looked up", say({ kind: "issue", number: 12 }), "Looking up #12 in [acme/shop](https://github.com/acme/shop/issues)…"],
      ["an open issue is named, with a link", say({ kind: "issue", number: 12 }, issue(true)),
        "Running it works on [#12 Customers export orders](https://github.com/acme/shop/issues/12): its steps become sub-issues of it"
        + ` (or take up the open ones an earlier run left), assigned to alice.${after}`],
      ["a closed one is reopened: a warning", say({ kind: "issue", number: 12 }, issue(false)).split(", and")[0],
        "! [#12 Customers export orders](https://github.com/acme/shop/issues/12) is closed: running it reopens it"],
      ["no such issue: the run wouldn't be tracked", say({ kind: "issue", number: 99 }, { missing: true }),
        "! [acme/shop](https://github.com/acme/shop/issues) has no issue #99: the plan would run without tracking. Check the number, or choose New issue."],
      ["a lookup that failed says why", say({ kind: "issue", number: 12 }, { error: "GitHub is down" }), "! Couldn't look up #12: GitHub is down"],
      ["not tracked: nothing goes to GitHub", say({ kind: "off" }), "Nothing goes to GitHub: the run is kept under Evidence runs only."],
      ["a private repository without a board says neither", say({ kind: "new", title: "" }, null,
        { repo: "acme/shop", host: "github.com", login: "alice", visibility: "private" }),
        "Running it opens an issue in [acme/shop](https://github.com/acme/shop/issues), with a sub-issue per step, assigned to alice."
        + " Each step's result is commented on its issue."],
    ];
  })(),
);

const failed = cases.filter(([, got, want]) => got !== want);
for (const [name, got, want] of failed) console.error(`${name}\n  want: ${JSON.stringify(want)}\n  got:  ${JSON.stringify(got)}`);
if (failed.length) {
  console.error(`${failed.length} of ${cases.length} plan editor cases failed`);
  process.exit(1);
}
console.log(`ok: ${cases.length} plan editor cases`);
