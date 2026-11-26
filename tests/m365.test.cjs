// Cases for how the page words Microsoft 365 content (ui/m365.js): labels without the
// internal `unknown`, a run's and an import's counts, who answers a @workiq step, where a
// knowledge topic came from, and a route's error as a sentence. tests/ui.rs runs this with
// node.
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const code = fs.readFileSync(path.join(__dirname, "..", "ui", "m365.js"), "utf8");
const { m365Label, m365Labels, workiqCounts, workiqWho, knowledgeOrigin, asSentence } =
  vm.runInThisContext(`${code}\n;({ m365Label, m365Labels, workiqCounts, workiqWho, knowledgeOrigin, asSentence })`);

// No label information, never "unknown"; label names as they are.
assert.strictEqual(m365Label("unknown"), "no label information");
assert.strictEqual(m365Label("Confidential\\Internal Only"), "Confidential\\Internal Only");
assert.strictEqual(m365Label("unlabeled"), "unlabeled");
assert.strictEqual(m365Labels(["General", "unknown"]), "General, no label information");

// A run's and an import's counts: an answer without citations has no label information.
assert.strictEqual(workiqCounts(1, ["unknown"], 0), "1 result kept (no label information), none withheld");
assert.strictEqual(workiqCounts(3, ["General", "unlabeled"], 2), "3 results kept (General, unlabeled), 2 withheld");
assert.strictEqual(workiqCounts(0, [], 1), "0 results kept, 1 withheld");
assert.strictEqual(workiqCounts(2, undefined, 0), "2 results kept, none withheld");

// A @workiq step's task: Microsoft 365 Copilot through Work IQ, or the agent it asks, never
// a provider; agent ids may have colons.
assert.strictEqual(workiqWho("workiq"), "Microsoft 365 Copilot (Work IQ)");
assert.strictEqual(workiqWho("@workiq"), "Microsoft 365 Copilot (Work IQ)");
assert.strictEqual(workiqWho("workiq:sales:1"), "Microsoft 365 agent sales:1 (Work IQ)");
assert.strictEqual(workiqWho(undefined), "Microsoft 365 Copilot (Work IQ)");

// A topic imported from Microsoft 365 shows its labels, and who it may go to.
const both = knowledgeOrigin(["Confidential\\Internal Only", "unknown"]);
assert.strictEqual(both.text, "Microsoft 365 · Confidential\\Internal Only, no label information");
assert.match(both.title, /without label information never goes to a peer, so neither does this topic/);
const general = knowledgeOrigin(["General"]);
assert.strictEqual(general.text, "Microsoft 365 · General");
assert.strictEqual(general.title, "Imported from Microsoft 365: it goes only to peers whose Microsoft 365 grant covers these labels");
// A mark without labels is content without a label (src/knowledge.rs labels_of_mark).
assert.strictEqual(knowledgeOrigin([]).text, "Microsoft 365 · unlabeled");
// Other topics have no mark.
assert.strictEqual(knowledgeOrigin(undefined), null);
assert.strictEqual(knowledgeOrigin(null), null);

// A route's error, worded to follow a colon, shows as a sentence.
const why = "nothing was imported, and the topic 'launch' was not written. Work IQ found nothing for the query.";
assert.strictEqual(asSentence(why),
  "Nothing was imported, and the topic 'launch' was not written. Work IQ found nothing for the query.");
assert.strictEqual(asSentence("Work IQ is not signed in"), "Work IQ is not signed in");
assert.strictEqual(asSentence(""), "");

console.log("ok: microsoft 365 wording");
