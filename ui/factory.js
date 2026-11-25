// Proofkin web UI: the agent factory. Agents you design as blueprints, check, and prove
// with evaluations before they work: a draft from a template or your words, its problems
// with their fixes, an estimate before anything runs, live progress, and the report (the
// release gate, each model's results and the failing runs). Uses app.js helpers: $, el,
// api, toast, confirmDialog, stamp, providerLabel and ACCESS_SHORT.
"use strict";

const factoryState = {
  loaded: false,
  agents: [],
  templates: null,
  open: null, // the agent shown in the dialog: its name
  agent: null, // GET /api/factory/agents/{name}
  draft: null, // GET /api/factory/drafts/{name}
  tab: "overview",
  estimate: null,
  report: null, // the evaluation shown under Evaluations
  opener: null,
};
const FACTORY_NAME = /^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$/;
const FACTORY_STATES = { running: ["Running", "running"], passed: ["Passed", "done"], failed: ["Failed", "failed"], cancelled: ["Cancelled", "cancelled"] };

function factoryPlural(n, one, many) {
  return `${n} ${n === 1 ? one : many}`;
}

function factoryRoute(r) {
  if (!r) return "";
  const model = r.model ? ` ${r.model}` : " (its default model)";
  return `${providerLabel(r.provider)}${model}${r.effort ? `, ${r.effort} reasoning` : ""}`;
}

function factoryPercent(x) {
  return `${Math.round((x ?? 0) * 100)}%`;
}

function factoryEvaluationStatus(e) {
  if (e.state === "running" && !e.running) return ["Stopped", "waiting"];
  return FACTORY_STATES[e.state] ?? [e.state, "waiting"];
}

async function loadFactory() {
  const res = await api("/api/factory/agents");
  factoryState.agents = res.agents;
  factoryState.loaded = true;
  renderFactory();
}

function renderFactory() {
  if (!factoryState.loaded) return;
  const list = $("#factory-list");
  if (!factoryState.agents.length) {
    list.replaceChildren(el("li", { class: "empty" }, "Agents you design, prove with evaluations and then run. ",
      el("strong", { text: "New agent" }), " starts one from a template or your words."));
    return;
  }
  // Redrawn as evaluations move on: focus stays on the agent it was on.
  const focused = document.activeElement?.closest?.("#factory-list [data-agent]")?.dataset.agent;
  list.replaceChildren(...factoryState.agents.map(factoryNode));
  if (focused) list.querySelector(`[data-agent="${CSS.escape(focused)}"] button`)?.focus();
}

/// The agent's latest evaluation, in a line.
function factoryLatest(a) {
  const e = a.evaluations?.[0];
  if (!e) return "Not evaluated yet";
  const [label] = factoryEvaluationStatus(e);
  if (e.state === "running") {
    return e.running ? `Evaluating · ${e.done} of ${factoryPlural(e.total, "run", "runs")} done` : `Evaluation stopped at ${e.done} of ${e.total} runs`;
  }
  const recommended = e.recommended ? ` · recommends ${factoryRoute(e.recommended)}` : "";
  return `Evaluation ${label.toLowerCase()}${recommended}`;
}

function factoryNode(a) {
  const problems = a.errors || a.warnings
    ? [a.errors ? factoryPlural(a.errors, "error", "errors") : null, a.warnings ? factoryPlural(a.warnings, "warning", "warnings") : null].filter(Boolean).join(" · ")
    : "No problems";
  const stateLabel = { draft: "Draft", active: "Active", deprecated: "Deprecated", retired: "Retired", imported: "Imported" }[a.state] ?? a.state;
  return el("li", { class: "factory-agent", "data-agent": a.name },
    avatar(a.name),
    el("div", { class: "who" },
      el("span", { class: "worker-head" }, el("span", { class: "name", text: a.name }),
        el("span", { class: `status ${a.state === "active" ? "done" : "waiting"}`, text: stateLabel })),
      a.title ? el("span", { class: "role", text: a.title }) : null,
      el("span", { class: "sub", text: `${problems} · ${factoryLatest(a)}` }),
      el("div", { class: "worker-actions" },
        el("button", {
          type: "button", class: "btn small", "aria-label": `Open ${a.name}`,
          onclick: (e) => openFactoryAgent(a.name, e.currentTarget),
        }, "Open"))));
}

// ---------- new agent ----------
async function openFactoryNew() {
  $("#factory-new-error").textContent = "";
  try {
    if (!factoryState.templates) factoryState.templates = (await api("/api/factory/templates")).templates;
  } catch (err) {
    toast(err.message, "error");
    return;
  }
  const picked = document.querySelector("#factory-templates input:checked")?.value ?? "";
  const option = (value, title, about) => el("label", { class: "factory-template" },
    el("input", { type: "radio", name: "factory-template", value, checked: value === picked || undefined }),
    el("span", {}, el("span", { class: "t-title", text: title }), el("span", { class: "t-purpose", text: about })));
  $("#factory-templates").replaceChildren(
    option("", "No template", "Your model writes the draft from your words alone."),
    ...factoryState.templates.map((t) => option(t.name, t.title,
      `${t.purpose} · ${(ACCESS_SHORT[t.access] ?? t.access).toLowerCase()} · ${factoryPlural(t.eval_cases, "case", "cases")}`)));
  $("#factory-new-dialog").showModal();
  $("#factory-name").focus();
}

async function submitFactoryNew(e) {
  e.preventDefault();
  const error = $("#factory-new-error");
  error.textContent = "";
  const nameInput = $("#factory-name");
  const name = nameInput.value.trim();
  const template = document.querySelector("#factory-templates input:checked")?.value ?? "";
  const description = $("#factory-description").value.trim();
  nameInput.removeAttribute("aria-invalid");
  if (!FACTORY_NAME.test(name)) {
    error.textContent = name ? "A name may only use letters, digits, '-', '_' and '.' (not first)." : "Enter a name.";
    nameInput.setAttribute("aria-invalid", "true");
    nameInput.focus();
    return;
  }
  if (!template && !description) {
    error.textContent = "Say what the agent should do, or pick a template.";
    $("#factory-description").focus();
    return;
  }
  const button = $("#factory-new-submit");
  button.disabled = true;
  button.textContent = description ? "Your model is drafting it…" : "Creating…";
  $("#factory-new-progress").textContent = description ? "Your model reads your words and writes the blueprint, read-only in an empty folder." : "";
  try {
    const body = { name };
    if (template) body.template = template;
    if (description) body.description = description;
    const draft = await api("/api/factory/drafts", { method: "POST", body: JSON.stringify(body) });
    $("#factory-new-form").reset();
    $("#factory-new-dialog").close();
    const errors = draft.problems.filter((p) => p.error).length;
    toast(errors ? `Created draft ${name}, with ${factoryPlural(errors, "error", "errors")} to fix` : `Created draft ${name}`);
    await loadFactory().catch(() => {});
    openFactoryAgent(name, $("#factory-new-btn"));
  } catch (err) {
    error.textContent = err.message;
    const field = /name|taken|already/.test(err.message) ? nameInput : /description/.test(err.message) ? $("#factory-description") : null;
    (field ?? button).focus();
  } finally {
    button.disabled = false;
    button.textContent = "Create draft";
    $("#factory-new-progress").textContent = "";
  }
}

// ---------- the agent ----------
async function openFactoryAgent(name, opener) {
  factoryState.open = name;
  factoryState.opener = opener ?? null;
  factoryState.estimate = null;
  factoryState.report = null;
  try {
    await refreshFactoryAgent();
  } catch (err) {
    toast(err.message, "error");
    return;
  }
  selectFactoryTab("overview");
  $("#factory-agent-dialog").showModal();
  $("#ftab-overview").focus();
}

async function refreshFactoryAgent(force = true) {
  const name = factoryState.open;
  const enc = encodeURIComponent(name);
  const before = {
    draft: JSON.stringify(factoryState.draft),
    evaluations: JSON.stringify(factoryState.agent?.evaluations ?? []),
    running: factoryRunning()?.id ?? null,
  };
  const [agent, draft] = await Promise.all([
    api(`/api/factory/agents/${enc}`),
    api(`/api/factory/drafts/${enc}`).catch(() => null),
  ]);
  if (factoryState.open !== name) return;
  factoryState.agent = agent;
  factoryState.draft = draft;
  $("#factory-agent-h").textContent = agent.title ? `${agent.name} · ${agent.title}` : agent.name;
  // Only what changed is drawn again: what the owner typed, picked or was shown stays.
  const draftChanged = force || JSON.stringify(draft) !== before.draft;
  const runningChanged = force || (factoryRunning()?.id ?? null) !== before.running;
  if (draftChanged) {
    renderFactoryOverview();
    if (force || !factoryState.jsonEdited) renderFactoryBlueprint();
  }
  if (runningChanged || (draftChanged && !factoryRunning())) renderFactoryEvaluate();
  if (force || JSON.stringify(agent.evaluations) !== before.evaluations) await renderFactoryReports();
}

function selectFactoryTab(name, focus = false) {
  factoryState.tab = name;
  for (const tab of document.querySelectorAll("#factory-agent-dialog [role=tab]")) {
    const on = tab.dataset.tab === name;
    tab.setAttribute("aria-selected", String(on));
    tab.tabIndex = on ? 0 : -1;
    $(`#${tab.getAttribute("aria-controls")}`).hidden = !on;
    if (on && focus) tab.focus();
  }
}

function factoryProblems(problems) {
  if (!problems.length) return el("p", { class: "muted small", text: "No problems: it can be built and evaluated." });
  return el("ul", { class: "factory-problems" }, ...problems.map((p) => el("li", { class: p.error ? "error" : "warning" },
    el("span", { class: `status ${p.error ? "failed" : "cancelled"}`, text: p.error ? "Error" : "Warning" }),
    el("span", { class: "p-text" },
      el("span", { text: p.message }),
      p.path ? el("span", { class: "mono muted small", text: ` ${p.path}` }) : null,
      p.fix ? el("span", { class: "p-fix small" }, "Fix: ", el("span", { class: "mono", text: p.fix })) : null))));
}

function renderFactoryOverview() {
  const panel = $("#fpanel-overview");
  const draft = factoryState.draft;
  if (!draft) {
    panel.replaceChildren(el("p", { class: "muted", text: "This agent has no draft here." }));
    return;
  }
  const bp = draft.blueprint;
  const caps = bp.capabilities ?? {};
  const list = (xs) => (xs?.length ? xs.join(", ") : "none");
  const mcp = (caps.mcp ?? []).map((m) => `${m.server} (${(m.tools ?? []).join(", ")})`);
  const provenance = draft.provenance ?? {};
  const written = provenance.source === "model"
    ? `by ${providerLabel(provenance.provider)}'s model${provenance.model ? ` (${provenance.model})` : ""} from your words${provenance.template ? `, with template ${provenance.template}` : ""}`
    : provenance.source === "template" ? `from template ${provenance.template}` : "from a file";
  const facts = el("dl", { class: "facts" },
    el("dt", { text: "Purpose" }), el("dd", { text: bp.purpose ?? "" }),
    el("dt", { text: "Asks for" }), el("dd", { text: `${ACCESS_SHORT[caps.access] ?? caps.access} access · skills ${list(bp.skills)} · MCP servers ${list(mcp)} · checks ${list(caps.checks)}${caps.delegate ? " · hands work to others" : ""}` }),
    el("dt", { text: "Runs on" }), el("dd", { text: list((bp.model?.providers ?? []).map(providerLabel)) }),
    el("dt", { text: "Proven by" }), el("dd", { text: `${factoryPlural(bp.evals?.cases?.length ?? 0, "case", "cases")}, ${factoryPlural(bp.evals?.repetitions ?? 3, "run", "runs")} each, and the built-in attacks` }),
    el("dt", { text: "Written" }), el("dd", { text: `${written}, ${stamp(Date.parse(provenance.at))}` }));
  const requirements = el("ol", { class: "factory-requirements" },
    ...(bp.requirements ?? []).map((r) => el("li", {}, el("span", { class: "mono", text: `${r.id} ` }), r.text)));
  const result = el("div", { id: "factory-check-result", role: "status" });
  const check = el("button", {
    type: "button", class: "btn small", id: "factory-check-btn",
    onclick: (e) => checkFactoryFolders(e.currentTarget, result),
  }, "Check in its folders");
  panel.replaceChildren(facts,
    el("h3", { text: "What it must do" }), requirements,
    el("div", { class: "card-head" }, el("h3", { text: "Problems" }), check),
    el("p", { class: "muted small", text: "Errors keep it from being built and evaluated; warnings point to less power it could ask for. Checking resolves what it asks for in the folders it is deployed to, and grants nothing." }),
    factoryProblems(draft.problems), result);
}

async function checkFactoryFolders(button, result) {
  button.disabled = true;
  button.textContent = "Checking…";
  try {
    const res = await api(`/api/factory/drafts/${encodeURIComponent(factoryState.open)}/check`, { method: "POST", body: "{}" });
    const grants = res.grants?.length
      ? el("ul", { class: "factory-grants" }, ...res.grants.map((g) => el("li", {},
        el("strong", { text: g.folder }), `: ${(ACCESS_SHORT[g.access] ?? g.access).toLowerCase()} access`,
        Object.keys(g.mcp ?? {}).length ? `; MCP servers ${Object.keys(g.mcp).join(", ")}` : "",
        g.checks?.length ? `; checks ${g.checks.join(", ")}` : "")))
      : el("p", { class: "muted small", text: "It names no folders to be deployed to yet." });
    result.replaceChildren(el("h3", { text: res.passed ? "In its folders: no errors" : "In its folders" }), grants, factoryProblems(res.problems));
  } catch (err) {
    result.replaceChildren(el("p", { class: "form-error", role: "alert", text: err.message }));
  } finally {
    button.disabled = false;
    button.textContent = "Check in its folders";
    // A disabled button loses focus: back on it.
    if (document.activeElement === document.body) button.focus();
  }
}

function renderFactoryBlueprint() {
  const panel = $("#fpanel-blueprint");
  const draft = factoryState.draft;
  if (!draft) {
    panel.replaceChildren(el("p", { class: "muted", text: "This agent has no draft to edit." }));
    return;
  }
  const area = el("textarea", {
    id: "factory-json", class: "factory-json mono", spellcheck: "false", autocomplete: "off",
    "aria-label": `Blueprint of ${factoryState.open} (JSON)`, "aria-describedby": "factory-json-hint",
  });
  area.value = JSON.stringify(draft.blueprint, null, 2);
  factoryState.jsonEdited = false;
  area.addEventListener("input", () => { factoryState.jsonEdited = true; });
  const status = el("div", { id: "factory-save-result", role: "status" });
  const error = el("p", { class: "form-error", id: "factory-save-error", role: "alert" });
  const save = el("button", { type: "button", class: "btn primary", id: "factory-save" }, "Save changes");
  save.addEventListener("click", () => saveFactoryBlueprint(area, save, error, status));
  panel.replaceChildren(
    el("p", { class: "muted small", id: "factory-json-hint", text: "The whole blueprint as JSON. It is checked when you save and kept with its problems; its name stays." }),
    area, error,
    el("div", { class: "dialog-actions" }, save),
    status);
}

async function saveFactoryBlueprint(area, button, error, status) {
  error.textContent = "";
  let blueprint;
  try {
    blueprint = JSON.parse(area.value);
  } catch (err) {
    error.textContent = `This isn't JSON: ${err.message}`;
    area.focus();
    return;
  }
  button.disabled = true;
  button.textContent = "Saving…";
  try {
    const res = await api(`/api/factory/drafts/${encodeURIComponent(factoryState.open)}`, { method: "PUT", body: JSON.stringify(blueprint) });
    factoryState.jsonEdited = false;
    await refreshFactoryAgent();
    $("#factory-save-result").replaceChildren(
      el("h3", { text: res.passed ? "Saved: no errors" : "Saved, with errors to fix" }), factoryProblems(res.problems));
    toast(`Saved ${factoryState.open}`);
    $("#factory-save").focus();
  } catch (err) {
    error.textContent = err.message;
    area.focus();
  } finally {
    button.disabled = false;
    button.textContent = "Save changes";
  }
}

// ---------- evaluate ----------
function factoryRunning() {
  return factoryState.agent?.evaluations?.find((e) => e.running) ?? null;
}

function renderFactoryEvaluate() {
  const panel = $("#fpanel-evaluate");
  const agent = factoryState.agent;
  const running = factoryRunning();
  if (running) {
    const [label, kind] = factoryEvaluationStatus(running);
    const cancel = el("button", { type: "button", class: "btn danger", id: "factory-cancel" }, "Cancel evaluation");
    cancel.addEventListener("click", () => cancelFactoryEvaluation(running, cancel));
    panel.replaceChildren(
      el("div", { class: "worker-head" }, el("span", { class: `status ${kind}`, text: label }),
        el("span", { class: "mono small", text: running.id })),
      el("progress", { id: "factory-progress", max: String(running.total || 1), value: String(running.done), "aria-label": "Runs done" }),
      el("p", { class: "small", id: "factory-progress-text", text: `${running.done} of ${factoryPlural(running.total, "run", "runs")} done, on your evaluation worker. Each run is a real task at the access the agent asks for.` }),
      el("div", { class: "dialog-actions" }, cancel));
    return;
  }
  const candidates = agent?.candidates ?? [];
  const leftOut = agent?.left_out ?? [];
  const reps = factoryState.draft?.blueprint?.evals?.repetitions ?? 3;
  const picks = el("div", { class: "share-checks", id: "factory-candidates" },
    ...candidates.map((c, i) => el("label", { class: "check" },
      el("input", { type: "checkbox", value: String(i), checked: true }),
      el("span", { text: factoryRoute(c) }))));
  const repetitions = el("select", { id: "factory-reps", "aria-describedby": "factory-reps-hint" },
    ...[1, 2, 3, 4, 5].map((n) => el("option", { value: String(n), selected: n === reps || undefined, text: factoryPlural(n, "run", "runs") })));
  const estimate = el("div", { id: "factory-estimate", role: "status" });
  const error = el("p", { class: "form-error", id: "factory-eval-error", role: "alert" });
  const estimateBtn = el("button", { type: "button", class: "btn", id: "factory-estimate-btn" }, "Estimate");
  const run = el("button", { type: "button", class: "btn primary", id: "factory-run", disabled: true }, "Run evaluation");
  estimateBtn.addEventListener("click", () => estimateFactory(estimateBtn, run, estimate, error));
  run.addEventListener("click", () => runFactoryEvaluation(run, error));
  for (const input of [picks, repetitions]) {
    input.addEventListener("change", () => {
      factoryState.estimate = null;
      run.disabled = true;
      estimate.replaceChildren();
    });
  }
  panel.replaceChildren(
    el("p", { class: "muted small", text: "Its cases and the built-in attacks run on each model you pick, a few times each, as real tasks of your evaluation worker. The release gate needs 90% of its own runs, 2 of 3 of each requirement's and every attack to pass; a judge of another model family scores rubrics and never decides." }),
    el("fieldset", { class: "field" }, el("legend", { text: "Models" }),
      candidates.length ? picks : el("p", { class: "muted small", text: "No model it can be evaluated on here." }),
      ...leftOut.map((l) => el("span", { class: "muted small", text: `Left out: ${factoryRoute(l.candidate)}: ${l.why}` }))),
    el("label", { class: "field narrow" }, el("span", { text: "Runs per case" }), repetitions,
      el("span", { class: "muted small", id: "factory-reps-hint", text: "More runs show how steady it is, and cost more." })),
    estimate, error,
    el("div", { class: "dialog-actions" }, estimateBtn, run));
}

function factoryEvaluationBody(confirm) {
  const candidates = factoryState.agent?.candidates ?? [];
  const picked = [...document.querySelectorAll("#factory-candidates input:checked")].map((i) => candidates[Number(i.value)]);
  const body = { confirm, repetitions: Number($("#factory-reps").value) };
  // All of them is the default; a choice names models.
  if (picked.length !== candidates.length) body.candidates = picked.filter((c) => c.model);
  return body;
}

async function estimateFactory(button, run, box, error) {
  error.textContent = "";
  if (!document.querySelector("#factory-candidates input:checked")) {
    error.textContent = "Pick at least one model.";
    $("#factory-candidates input")?.focus();
    return;
  }
  button.disabled = true;
  button.textContent = "Estimating…";
  try {
    const est = await api(`/api/factory/agents/${encodeURIComponent(factoryState.open)}/evaluations`, {
      method: "POST", body: JSON.stringify(factoryEvaluationBody(false)),
    });
    factoryState.estimate = est;
    box.replaceChildren(el("p", {},
      el("strong", { text: `${factoryPlural(est.runs, "run", "runs")}` }),
      ` (${factoryPlural(est.cases, "case", "cases")}, ${est.adversarial} of them built-in attacks, × ${factoryPlural(est.repetitions, "run", "runs")} × ${factoryPlural(est.candidates, "model", "models")})`,
      est.judge_calls ? `, and ${factoryPlural(est.judge_calls, "score", "scores")} by judges` : "",
      `: about ${factoryPlural(est.minutes, "minute", "minutes")} on ${factoryPlural(est.slots, "task slot", "task slots")} of ${est.worker}. Nothing has run yet.`));
    run.disabled = false;
    run.textContent = `Run ${factoryPlural(est.tasks, "task", "tasks")}`;
    run.focus();
  } catch (err) {
    error.textContent = err.message;
    button.focus();
  } finally {
    button.disabled = false;
    button.textContent = "Estimate";
  }
}

async function runFactoryEvaluation(button, error) {
  error.textContent = "";
  button.disabled = true;
  button.textContent = "Starting…";
  try {
    const e = await api(`/api/factory/agents/${encodeURIComponent(factoryState.open)}/evaluations`, {
      method: "POST", body: JSON.stringify(factoryEvaluationBody(true)),
    });
    toast(`Evaluating ${factoryState.open} (${e.id})`);
    await refreshFactoryAgent();
    $("#factory-cancel")?.focus();
  } catch (err) {
    error.textContent = err.message;
    button.disabled = false;
    button.textContent = "Run evaluation";
    button.focus();
  }
}

async function cancelFactoryEvaluation(e, button) {
  const ok = await confirmDialog({
    title: `Cancel evaluation ${e.id}?`,
    body: "Its runs still out stop on your evaluation worker. Its report keeps the runs that ended, and the agent stays as it is.",
    confirm: "Cancel evaluation",
    danger: true,
  });
  if (!ok) return;
  button.disabled = true;
  button.textContent = "Cancelling…";
  try {
    await api(`/api/factory/evaluations/${encodeURIComponent(e.id)}`, { method: "DELETE" });
    toast(`Cancelled evaluation ${e.id}`);
    await refreshFactoryAgent();
    $("#factory-estimate-btn")?.focus();
  } catch (err) {
    toast(err.message, "error");
    button.disabled = false;
    button.textContent = "Cancel evaluation";
  }
}

// ---------- evaluations ----------
async function renderFactoryReports() {
  const panel = $("#fpanel-reports");
  const evaluations = factoryState.agent?.evaluations ?? [];
  if (!evaluations.length) {
    panel.replaceChildren(el("p", { class: "empty" }, "No evaluations yet. ", el("strong", { text: "Evaluate" }), " proves it on the models it may run on."));
    return;
  }
  const shown = factoryState.report?.id && evaluations.some((e) => e.id === factoryState.report.id) ? factoryState.report.id : evaluations[0].id;
  const pick = el("select", { id: "factory-report-pick", "aria-label": "Evaluation" },
    ...evaluations.map((e) => el("option", { value: e.id, selected: e.id === shown || undefined, text: `${stamp(Date.parse(e.started))} · ${factoryEvaluationStatus(e)[0]}` })));
  const body = el("div", { id: "factory-report" });
  pick.addEventListener("change", () => showFactoryReport(pick.value, body));
  panel.replaceChildren(el("label", { class: "field" }, el("span", { text: "Evaluation" }), pick), body);
  await showFactoryReport(shown, body);
}

async function showFactoryReport(id, body) {
  let e;
  try {
    e = await api(`/api/factory/evaluations/${encodeURIComponent(id)}`);
  } catch (err) {
    body.replaceChildren(el("p", { class: "form-error", role: "alert", text: err.message }));
    return;
  }
  factoryState.report = e;
  const [label, kind] = factoryEvaluationStatus(e);
  const head = el("div", { class: "worker-head" },
    el("span", { class: `status ${kind}`, text: label }),
    el("span", { class: "mono small", text: `${e.id} · build ${e.build.slice(0, 12)}` }),
    e.verified === undefined ? null : el("span", { class: `badge evidence ${e.verified ? "verified" : "unverified"}`, text: e.verified ? "Signed, verifies" : "Doesn't verify" }));
  const meta = el("p", { class: "muted small", text: `Started ${stamp(Date.parse(e.started))} by ${e.by}${e.finished ? `, ended ${stamp(Date.parse(e.finished))}` : ""} · ${factoryPlural(e.cases.length, "case", "cases")} (suite ${e.adversarial}) × ${factoryPlural(e.repetitions, "run", "runs")} × ${factoryPlural(e.candidates.length, "model", "models")} · ${e.done} of ${e.total} runs done` });
  const gate = e.state === "running"
    ? el("p", { text: e.running ? "Running: the gate is decided when it ends." : "It stopped before it ended: evaluate it again to go on." })
    : e.gate.passed
      ? el("p", {}, el("strong", { text: "Gate passed" }), e.recommended ? `: a release takes ${factoryRoute(e.recommended)}.` : ".")
      : el("div", {}, el("p", {}, el("strong", { text: "Gate failed:" })), el("ul", {}, ...e.gate.reasons.map((r) => el("li", { text: r }))));
  body.replaceChildren(head, meta, gate, factoryMatrix(e), factoryFailing(e));
}

/// Each model's results: a row each, its metrics in columns.
function factoryMatrix(e) {
  if (!e.results?.length) return el("p", { class: "muted small", text: "Results come when it ends." });
  const requirements = [...new Set(e.results.flatMap((r) => Object.keys(r.requirements)))].sort();
  const head = el("tr", {}, ...["Model", "Gate", "Pass rate", ...requirements, "Attacks", "p95", "Attempts", "Cost", "Judge"].map((h) => el("th", { scope: "col", text: h })));
  const rows = e.results.map((r) => el("tr", {},
    el("th", { scope: "row", text: factoryRoute(r.candidate) }),
    el("td", {}, el("span", { class: `status ${r.gate.passed ? "done" : "failed"}`, text: r.gate.passed ? "Passed" : "Failed" })),
    el("td", { text: factoryPercent(r.pass_rate) }),
    ...requirements.map((q) => el("td", { text: q in r.requirements ? factoryPercent(r.requirements[q]) : "–" })),
    el("td", { text: `${r.adversarial.passed}/${r.adversarial.total}` }),
    el("td", { text: `${Math.round(r.p95_secs)}s` }),
    el("td", { text: r.attempts.toFixed(1) }),
    el("td", { text: r.multiplier ?? "unknown" }),
    el("td", { text: r.judge_score === null || r.judge_score === undefined ? "–" : r.judge_score.toFixed(1) })));
  return el("div", { class: "factory-matrix-wrap", tabindex: "0", role: "region", "aria-label": "Results by model" },
    el("table", { class: "factory-matrix" }, el("thead", {}, head), el("tbody", {}, ...rows)));
}

/// The runs that failed, each with why and what it answered.
function factoryFailing(e) {
  const failing = e.runs.filter((r) => r.state === "done" && !r.passed);
  if (!failing.length) return el("p", { class: "muted small", text: e.done ? "No run failed." : "" });
  const cases = new Map(e.cases.map((c) => [c.id, c]));
  return el("div", {}, el("h3", { text: `Failing runs (${failing.length})` }),
    el("ul", { class: "factory-failing" }, ...failing.map((r) => {
      const c = cases.get(r.case) ?? {};
      const why = [r.error, ...(r.grades ?? []).filter((g) => g.passed === false).map((g) => g.note)].filter(Boolean);
      return el("li", {},
        el("span", { class: "worker-head" }, el("strong", { text: r.case }),
          el("span", { class: "muted small", text: `${factoryRoute(e.candidates[r.candidate])} · run ${r.repetition}${c.attack ? ` · attack: ${c.attack}` : c.requirement ? ` · proves ${c.requirement}` : ""}` })),
        el("ul", { class: "small" }, ...why.map((w) => el("li", { text: w }))),
        r.output ? el("details", {}, el("summary", { text: r.redacted ? "Its answer (the canary redacted)" : "Its answer" }),
          el("pre", { class: "factory-output", tabindex: "0", text: r.output })) : null);
    })));
}

// ---------- live updates ----------
let factoryTimer = null;
function onFactoryEvent(msg) {
  // What the owner's model is doing while it writes a draft.
  if (msg.progress) {
    if ($("#factory-new-dialog").open) $("#factory-new-progress").textContent = msg.progress;
    return;
  }
  // Many runs end close together: one refresh for them.
  clearTimeout(factoryTimer);
  factoryTimer = setTimeout(async () => {
    await loadFactory().catch(() => {});
    const dialog = $("#factory-agent-dialog");
    if (!dialog.open || (msg.agent && msg.agent !== factoryState.open)) return;
    const running = factoryRunning();
    if (running && msg.evaluation === running.id && msg.state === "running" && $("#factory-progress")) {
      // In place: focus and scroll stay where they are.
      $("#factory-progress").value = msg.done ?? running.done;
      $("#factory-progress").max = msg.total ?? running.total;
      $("#factory-progress-text").textContent = `${msg.done ?? running.done} of ${factoryPlural(msg.total ?? running.total, "run", "runs")} done, on your evaluation worker. Each run is a real task at the access the agent asks for.`;
      return;
    }
    const focused = document.activeElement?.id;
    const inDialog = dialog.contains(document.activeElement);
    await refreshFactoryAgent(false).catch(() => {});
    // Who cancelled it was told already; an evaluation that ended on its own is news.
    if (msg.state === "passed" || msg.state === "failed") {
      toast(`Evaluation of ${msg.agent} ${msg.state}`);
    }
    const again = focused ? document.getElementById(focused) : null;
    if (again) again.focus();
    // What had focus went (e.g. Cancel, once it ended): its panel's first action, or its tab.
    else if (inDialog) {
      (document.querySelector(`#fpanel-${factoryState.tab} button:not([disabled])`) ?? $(`#ftab-${factoryState.tab}`)).focus();
    }
  }, 250);
}

(function initFactory() {
  $("#factory-new-btn").addEventListener("click", openFactoryNew);
  $("#factory-new-cancel").addEventListener("click", () => $("#factory-new-dialog").close());
  $("#factory-new-dialog").addEventListener("close", () => $("#factory-new-btn").focus());
  $("#factory-new-form").addEventListener("submit", submitFactoryNew);
  $("#factory-agent-close").addEventListener("click", () => $("#factory-agent-dialog").close());
  $("#factory-agent-dialog").addEventListener("close", () => {
    factoryState.open = null;
    const opener = factoryState.opener;
    // Back to its item's button; if that's gone, to the card's main action.
    const again = opener?.isConnected ? opener : document.querySelector(`#factory-list [data-agent="${CSS.escape(factoryState.agent?.name ?? "")}"] button`);
    (again ?? $("#factory-new-btn")).focus();
  });
  const tablist = $("#factory-agent-dialog [role=tablist]");
  tablist.addEventListener("click", (e) => {
    const tab = e.target.closest("[role=tab]");
    if (tab) selectFactoryTab(tab.dataset.tab);
  });
  tablist.addEventListener("keydown", (e) => {
    const tabs = [...tablist.querySelectorAll("[role=tab]")];
    const i = tabs.findIndex((t) => t.dataset.tab === factoryState.tab);
    const next = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
    if (next === undefined) return;
    e.preventDefault();
    selectFactoryTab(tabs[(next + tabs.length) % tabs.length].dataset.tab, true);
  });
})();
