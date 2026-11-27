// Cases for the model lists (ui/models.js) the Run task form and the plan builder show:
// what a worker lets peers pick (the models it allows, else all its account offers), what
// several allow together, and the reasoning levels a model takes. tests/ui.rs runs this
// with node.
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const code = fs.readFileSync(path.join(__dirname, "..", "ui", "models.js"), "utf8");
const { modelMenu, effortChoices, fitEffort, modelOptionText, effortRange, modelsByVendor, modelsSummary, squadMember } =
  vm.runInThisContext(`${code}\n;({ modelMenu, effortChoices, fitEffort, modelOptionText, effortRange, modelsByVendor, modelsSummary, squadMember })`);

const opus = { id: "claude-opus-5.5", name: "Claude Opus 5.5", vendor: "Anthropic", multiplier: "3", efforts: ["low", "medium", "high", "max"] };
const gpt = { id: "gpt-5.5", name: "GPT-5.5", vendor: "OpenAI", multiplier: "1", efforts: ["low", "medium", "high", "xhigh"] };
// It takes no reasoning level (its offer says none).
const mini = { id: "gpt-5-mini", name: "GPT-5 mini", vendor: "OpenAI", multiplier: "0" };

// A worker that allows any model offers all its account lists, its defaults selected.
const worker = { models: { copilot: { default: "claude-opus-5.5", effort: "max" } }, offers: { copilot: [opus, gpt, mini] } };
let menu = modelMenu([worker], "copilot");
assert.deepStrictEqual(menu.models.map((m) => m.id), ["claude-opus-5.5", "gpt-5.5", "gpt-5-mini"]);
assert.strictEqual(menu.defaultModel, "claude-opus-5.5");
assert.strictEqual(menu.defaultEffort, "max");
assert.strictEqual(menu.other, false, "it lists them: none by name");
assert.deepStrictEqual(menu.efforts, ["minimal", "low", "medium", "high", "xhigh", "max"]);
// Reasoning: the levels the model supports, the default as the model takes it.
assert.deepStrictEqual(effortChoices(menu, ""), { levels: ["low", "medium", "high", "max"], fitted: "max", fixed: false });
assert.deepStrictEqual(effortChoices(menu, "gpt-5.5"), { levels: ["low", "medium", "high", "xhigh"], fitted: "xhigh", fixed: false });
assert.deepStrictEqual(effortChoices(menu, "GPT-5-MINI"), { levels: [], fitted: null, fixed: true });
// A model it doesn't list (named by hand): every level it allows.
assert.strictEqual(effortChoices(menu, "o9").levels.length, 6);

// A worker that limits them offers only those (one its account doesn't list, by name)
// and the levels it allows; with no defaults set: the first model, the least level.
const picky = { models: { copilot: { allowed: ["gpt-5.5", "o9"], efforts: ["low", "medium"] } }, offers: { copilot: [gpt] } };
menu = modelMenu([picky], "copilot");
assert.deepStrictEqual(menu.models.map((m) => [m.id, m.efforts]), [["gpt-5.5", gpt.efforts], ["o9", null]]);
assert.strictEqual(menu.defaultModel, "gpt-5.5");
assert.strictEqual(menu.defaultEffort, "low");
assert.deepStrictEqual(menu.efforts, ["low", "medium"]);
assert.deepStrictEqual(effortChoices(menu, "gpt-5.5").levels, ["low", "medium"]);
assert.strictEqual(menu.other, false);

// An older peer, or Claude and Codex (no list): a model may be named.
menu = modelMenu([{ models: {} }], "claude");
assert.deepStrictEqual([menu.models, menu.other, menu.defaultModel], [[], true, null]);
assert.strictEqual(modelMenu([], "copilot").other, true, "no peer to ask");

// Several (Any peer, a squad's members): what they allow together, and no defaults.
menu = modelMenu([worker, picky], "copilot");
assert.deepStrictEqual(menu.models.map((m) => m.id), ["claude-opus-5.5", "gpt-5.5", "gpt-5-mini", "o9"]);
assert.deepStrictEqual([menu.defaultModel, menu.defaultEffort], [null, null]);
assert.strictEqual(menu.efforts.length, 6);

// Run task on this agent and Describe: defaults first, then vendor groups, then a name.
const { modelPickerOptions, OTHER_MODEL } = vm.runInThisContext("({ modelPickerOptions, OTHER_MODEL })");
const sonnet = { id: "claude-sonnet-5.5", vendor: "Anthropic", efforts: ["low", "medium", "high"] };
const local = { ...worker, offers: { copilot: [gpt, opus, sonnet, mini] } };
const pick = (agent, provider = "copilot", model = "", effort = "", name = "") =>
  modelPickerOptions([agent], provider, { model, effort, name }, true);
const defaults = pick(local);
assert.deepStrictEqual(defaults.models.map((o) => o.options ? [o.label, ...o.options.map((m) => m.value)] : [o.value, o.text]), [
  ["", "claude-opus-5.5 (default)"],
  ["OpenAI", "gpt-5.5", "gpt-5-mini"],
  ["Anthropic", "claude-sonnet-5.5"],
  [OTHER_MODEL, "Another model…"],
]);
assert.deepStrictEqual(defaults.efforts.map((o) => o.text), ["max (default)", "low", "medium", "high"]);
assert.deepStrictEqual([defaults.model, defaults.effort], ["", ""]);
const other = pick(local, "claude", OTHER_MODEL, "low", "claude-sonnet-5.5");
assert.deepStrictEqual(other.models, [{ value: "", text: "Its default" }, { value: OTHER_MODEL, text: "Another model…" }]);
assert.deepStrictEqual([other.model, other.effort], [OTHER_MODEL, "low"]);

const chosen = pick(local, "copilot", "claude-sonnet-5.5", "low");
assert.deepStrictEqual([chosen.model, chosen.effort], ["claude-sonnet-5.5", "low"]);
assert.deepStrictEqual(chosen.efforts.map((o) => o.text), ["high (default)", "low", "medium"]);
const fixed = pick(local, "copilot", "gpt-5-mini", "low");
assert.strictEqual(fixed.fixed, true);
assert.strictEqual(fixed.effort, "");
assert.deepStrictEqual(fixed.efforts, [{ value: "", text: "None for this model" }]);
// A model typed by name also changes Reasoning, even when it happens to be listed.
assert.strictEqual(pick(local, "copilot", OTHER_MODEL, "low", " GPT-5-MINI ").fixed, true);

// Refreshed settings keep what is still allowed, but drop the rest, including a free name.
const limited = {
  ...local,
  models: { copilot: {
    default: "claude-opus-5.5", allowed: ["claude-opus-5.5", "claude-sonnet-5.5"],
    effort: "medium", efforts: ["low", "medium"],
  } },
};
const kept = pick(limited, "copilot", chosen.model, chosen.effort);
assert.deepStrictEqual([kept.model, kept.effort], ["claude-sonnet-5.5", "low"]);
assert.deepStrictEqual(kept.models.map((o) => o.value), ["", "claude-sonnet-5.5"]);
assert.deepStrictEqual(kept.efforts.map((o) => o.text), ["medium (default)", "low"]);
const gone = pick(limited, "copilot", "gpt-5.5", "high");
assert.deepStrictEqual([gone.model, gone.effort], ["", ""]);
assert.strictEqual(pick(limited, "copilot", OTHER_MODEL).model, "");

// A level fitted to a model as the worker fits it (src/models.rs fit_effort).
assert.strictEqual(fitEffort("max", ["low", "medium", "high"]), "high");
assert.strictEqual(fitEffort("minimal", ["low", "high"]), "low");
assert.strictEqual(fitEffort("medium", ["medium"]), "medium");
assert.strictEqual(fitEffort("high", []), null);

// Short option labels, vendors in the order they come, ranges and summaries.
assert.strictEqual(modelOptionText(opus), "claude-opus-5.5 · 3x");
assert.strictEqual(modelOptionText({ id: "o9" }), "o9");
assert.deepStrictEqual(modelsByVendor([opus, gpt, mini]).map(([v, l]) => [v, l.length]), [["Anthropic", 1], ["OpenAI", 2]]);
assert.strictEqual(effortRange(["max", "low", "none"]), "low–max");
assert.strictEqual(effortRange([]), null);
assert.strictEqual(
  modelsSummary({ allowed: ["gpt-5.5"], efforts: ["low", "medium"] }),
  "gpt-5.5 · low reasoning · only gpt-5.5 · levels low, medium",
);
assert.strictEqual(modelsSummary({ allowed: ["a", "b", "c", "d"] }), "a · only 4 models");
assert.strictEqual(modelsSummary(undefined), "its default model");

// A squad member as the squad dialog and card describe it: what it runs, where, and
// whether any step can go to it.
const label = (p) => ({ copilot: "Copilot", claude: "Claude", codex: "Codex" })[p];
assert.deepStrictEqual(squadMember({
  capabilities: ["copilot"], owner: "worker", role: "tester",
  models: { copilot: { default: "claude-opus-5.5", effort: "max" } }, shares: [{ name: "data" }],
}, label), { takes: true, text: "digital worker of worker · tester · Copilot (claude-opus-5.5, max reasoning) · folders data" });
assert.deepStrictEqual(squadMember({
  capabilities: ["copilot", "claude"], models: { copilot: { allowed: ["gpt-5.5"] } },
  shares: [{ name: "data" }, { name: "demo", team: true }],
}, label), { takes: true, text: "Copilot (gpt-5.5), Claude · folders data, demo (team plan)" });
// A delegate-only agent (no providers, no team plan) takes none: plans hand it nothing.
assert.deepStrictEqual(squadMember({ capabilities: [], shares: [] }, label), {
  takes: false, why: "it runs no models (it only delegates)",
  text: "takes no squad steps: it runs no models (it only delegates)",
});
assert.deepStrictEqual(squadMember({ capabilities: ["codex"], shares: [], owner: "ana" }, label), {
  takes: false, why: "it shares no folder with you",
  text: "digital worker of ana · takes no squad steps: it shares no folder with you",
});
// A lead with no providers of its own takes team steps: its team plan runs them.
assert.deepStrictEqual(squadMember({ capabilities: [], shares: [{ name: "demo", team: true }] }, label),
  { takes: true, text: "team steps only · folders demo (team plan)" });

const { planModelOptions } = vm.runInThisContext("({ planModelOptions })");
const also = {
  models: { copilot: { default: "CLAUDE-OPUS-5.5", allowed: [opus.id, gpt.id, "o9"], efforts: ["low", "high"] } },
  offers: { copilot: [opus, gpt] },
};
const planPick = (agents, model = null, effort = null) =>
  planModelOptions(new Map([["copilot", agents]]), { model, effort });
const common = planPick([worker, also], gpt.id, "HIGH");
assert.deepStrictEqual(common.models.map((o) => o.options ? [o.label, ...o.options.map((m) => m.value)] : [o.value, o.text]), [
  ["", "claude-opus-5.5 (each peer's default)"],
  ["Anthropic", opus.id],
  ["OpenAI", gpt.id],
]);
assert.deepStrictEqual([common.model, common.effort], [gpt.id, "high"]);
assert.deepStrictEqual(common.efforts.map((o) => o.value), ["", "low", "high"]);
assert.strictEqual(planPick([worker, picky]).models[0].text, "Each step's peer default");
assert.strictEqual(planPick([worker, {}]).models.at(-1).value, OTHER_MODEL, "one peer lists nothing");
assert.strictEqual(planPick([worker], mini.id).fixed, true, "reasoning follows the model");
const typed = planPick([worker, also], mini.id, "max");
assert.deepStrictEqual([typed.model, typed.effort], [mini.id, "max"], "keep explicit lines even if not offered together");
const mixed = planModelOptions(new Map([
  ["copilot", [also]],
  ["claude", [{ models: { claude: { efforts: ["low"] } } }]],
]), { model: null, effort: null });
assert.deepStrictEqual(mixed.models, [{ value: "", text: "Each step's peer default" }]);
assert.deepStrictEqual(mixed.efforts.map((o) => o.value), ["", "low"], "reasoning spans providers");

console.log("ok: model lists");
