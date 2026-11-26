// Proofkin web UI: the agent factory. Agents you design as blueprints, check, and prove
// with evaluations before they work: a draft from a template or your words, configured
// with a form (or as JSON), its problems with their fixes, an estimate before anything
// runs, live progress, and the report (the release gate, each model's results and the
// failing runs). Then they run: signed releases and their sign-offs, deployments as
// digital workers with a canary, rollback, budgets, metrics by release, deprecation and
// retirement; drift watches, improvements and sharing with peers are in factory-share.js.
// Uses app.js helpers: $, el, api, toast, confirmDialog, stamp, providerLabel and
// ACCESS_SHORT.
"use strict";

const factoryState = {
  loaded: false,
  agents: [],
  templates: null,
  open: null, // the agent shown in the dialog: its name
  agent: null, // GET /api/factory/agents/{name}
  draft: null, // GET /api/factory/drafts/{name}
  tab: "overview",
  editMode: "form", // Configure: "form" or "json"
  formEdited: false,
  jsonEdited: false,
  estimate: null,
  report: null, // the evaluation shown under Evaluations
  reportPicked: false, // the owner picked it (else the newest is shown)
  opener: null,
  releaseOpen: new Set(), // releases whose details are open
  metricsSince: "7d",
  rows: 0, // ids for the form's rows
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
  const deployed = a.deployments ?? [];
  const running = deployed.filter((d) => d.running).length;
  const badges = [
    a.active_version ? el("span", {
      class: `badge ${a.state === "retired" ? "neutral" : "ok"}`,
      title: a.state === "retired" ? "Its last release: plans refuse a retired agent" : `The release plans use as agent=${a.name}`,
      text: `Release ${a.active_version}`,
    }) : null,
    a.waiting ? el("span", { class: "badge busy", text: `${factoryPlural(a.waiting, "release waits", "releases wait")} for sign-off` }) : null,
    deployed.length ? el("span", { class: "badge neutral", title: deployed.map((d) => `${d.worker}: ${d.version}, ${d.state}`).join("; "),
      text: `${factoryPlural(deployed.length, "deployment", "deployments")}${running ? `, ${running} running here` : ""}` }) : null,
    a.imported ? el("span", { class: "badge neutral", title: `Imported: its publisher's key is ${a.imported.publisher_id}`, text: `From ${a.imported.publisher}` }) : null,
    a.watch?.incident_open ? el("span", { class: "badge danger", text: "Drift incident open" }) : null,
    a.publication?.versions?.length && a.publication?.peers?.length
      ? el("span", { class: "badge neutral", title: `Published ${a.publication.versions.join(", ")} to ${a.publication.peers.join(", ")}`, text: `Published to ${factoryPlural(a.publication.peers.length, "peer", "peers")}` }) : null,
  ].filter(Boolean);
  return el("li", { class: "factory-agent", "data-agent": a.name },
    avatar(a.name),
    el("div", { class: "who" },
      el("span", { class: "worker-head" }, el("span", { class: "name", text: a.name }),
        el("span", { class: `status ${a.state === "active" ? "done" : "waiting"}`, text: stateLabel })),
      a.title ? el("span", { class: "role", text: a.title }) : null,
      badges.length ? el("span", { class: "factory-badges" }, ...badges) : null,
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
async function openFactoryAgent(name, opener, tab = "overview") {
  factoryState.open = name;
  factoryState.opener = opener ?? null;
  factoryState.estimate = null;
  factoryState.report = null;
  factoryState.reportPicked = false;
  factoryState.releaseOpen = new Set();
  try {
    await refreshFactoryAgent();
  } catch (err) {
    toast(err.message, "error");
    return;
  }
  selectFactoryTab(tab);
  $("#factory-agent-dialog").showModal();
  $(`#ftab-${tab}`).focus();
}

/// What, of the agent, its releases, deployments and lifecycle panels show.
function factoryLife(agent) {
  return JSON.stringify([agent?.state, agent?.active_version, agent?.replacement, agent?.purge_after,
    agent?.releases, agent?.deployments, agent?.history]);
}

/// What, of the agent, its Drift, Improve and Share panels show (but its proposals).
function factoryScale(agent) {
  return JSON.stringify([agent?.state, agent?.active_version, agent?.watch, agent?.publication, agent?.imported,
    agent?.imported_version, (agent?.releases ?? []).map((r) => [r.version, r.state]), agent?.choices?.peers]);
}

async function refreshFactoryAgent(force = true) {
  const name = factoryState.open;
  const enc = encodeURIComponent(name);
  const before = {
    draft: JSON.stringify(factoryState.draft),
    evaluations: JSON.stringify(factoryState.agent?.evaluations ?? []),
    running: factoryRunning()?.id ?? null,
    life: factoryLife(factoryState.agent),
    scale: factoryScale(factoryState.agent),
    proposals: JSON.stringify(factoryState.agent?.proposals ?? []),
  };
  // Only a draft there is is fetched (an import never has one): the first time, as the list says.
  const known = factoryState.agent?.name === name ? factoryState.agent : factoryState.agents?.find((a) => a.name === name);
  const skip = known?.has_draft === false;
  const [agent, fetched] = await Promise.all([
    api(`/api/factory/agents/${enc}`),
    skip ? null : api(`/api/factory/drafts/${enc}`).catch(() => null),
  ]);
  const draft = skip && agent.has_draft ? await api(`/api/factory/drafts/${enc}`).catch(() => null) : fetched;
  if (factoryState.open !== name) return;
  factoryState.agent = agent;
  factoryState.draft = draft;
  $("#factory-agent-h").textContent = agent.title ? `${agent.name} · ${agent.title}` : agent.name;
  // Only what changed is drawn again: what the owner typed, picked or was shown stays.
  const draftChanged = force || JSON.stringify(draft) !== before.draft;
  const runningChanged = force || (factoryRunning()?.id ?? null) !== before.running;
  const evaluationsChanged = force || JSON.stringify(agent.evaluations) !== before.evaluations;
  const lifeChanged = force || factoryLife(agent) !== before.life;
  if (draftChanged || lifeChanged) renderFactoryOverview();
  if (draftChanged) {
    if (force || !factoryState.jsonEdited) renderFactoryBlueprint();
    if (force || !factoryState.formEdited) renderFactoryConfigure();
  }
  if (runningChanged || (draftChanged && !factoryRunning())) renderFactoryEvaluate();
  if (evaluationsChanged) await renderFactoryReports();
  if (lifeChanged || evaluationsChanged) {
    renderFactoryReleases();
    renderFactoryDeployments();
  }
  if (factoryState.tab === "metrics" && (force || lifeChanged)) loadFactoryMetrics();
  // Their forms are drawn again only when what they show changed: what the owner typed stays.
  if (force || factoryScale(agent) !== before.scale) {
    renderFactoryDrift();
    renderFactoryImprove();
    renderFactoryShare();
  } else if (JSON.stringify(agent.proposals ?? []) !== before.proposals) {
    renderFactoryProposals();
  }
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
  if (name === "metrics") loadFactoryMetrics();
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
  const agent = factoryState.agent;
  // An import has no draft: its blueprint is its bundle's (or, once active, its release's).
  const bp = draft?.blueprint ?? agent?.blueprint;
  if (!bp) {
    panel.replaceChildren(el("p", { class: "muted", text: "This agent has no draft here." }), factoryLifecycle(agent));
    return;
  }
  const caps = bp.capabilities ?? {};
  const list = (xs) => (xs?.length ? xs.join(", ") : "none");
  const mcp = (caps.mcp ?? []).map((m) => `${m.server} (${(m.tools ?? []).join(", ")})`);
  const provenance = draft?.provenance ?? {};
  const written = !draft ? (agent?.imported ? `by ${agent.imported.publisher}, who published it to you` : "elsewhere: it has no draft here")
    : provenance.source === "model"
      ? `by ${providerLabel(provenance.provider)}'s model${provenance.model ? ` (${provenance.model})` : ""} from your words${provenance.template ? `, with template ${provenance.template}` : ""}`
      : provenance.source === "template" ? `from template ${provenance.template}` : "from a file";
  const deployed = agent?.deployments ?? [];
  const facts = el("dl", { class: "facts" },
    el("dt", { text: "Purpose" }), el("dd", { text: bp.purpose ?? "" }),
    el("dt", { text: "Asks for" }), el("dd", { text: `${ACCESS_SHORT[caps.access] ?? caps.access} access · skills ${list(bp.skills)} · MCP servers ${list(mcp)} · checks ${list(caps.checks)}${caps.delegate ? " · hands work to others" : ""}` }),
    el("dt", { text: "Runs on" }), el("dd", { text: list((bp.model?.providers ?? []).map(providerLabel)) }),
    el("dt", { text: "Proven by" }), el("dd", { text: `${factoryPlural(bp.evals?.cases?.length ?? 0, "case", "cases")}, ${factoryPlural(bp.evals?.repetitions ?? 3, "run", "runs")} each, and the built-in attacks` }),
    el("dt", { text: "Written" }), el("dd", { text: provenance.at ? `${written}, ${stamp(Date.parse(provenance.at))}` : written }),
    el("dt", { text: "Released" }), el("dd", { text: agent?.active_version
      ? agent.state === "retired"
        ? `${agent.active_version}, its last release: it is retired, and plans refuse it`
        : `${agent.active_version}: plans use it as agent=${agent.name}${agent.waiting ? ` · ${factoryPlural(agent.waiting, "release waits", "releases wait")} for sign-off` : ""}`
      : agent?.waiting ? `Not yet: ${factoryPlural(agent.waiting, "release waits", "releases wait")} for sign-off` : "Not yet" }),
    el("dt", { text: "Deployed" }), el("dd", { text: deployed.length
      ? deployed.map((d) => `${d.worker} (${d.version}, ${FACTORY_DEPLOY_STATES[d.state]?.[0].toLowerCase() ?? d.state})`).join(", ")
      : "Not deployed" }));
  const requirements = el("ol", { class: "factory-requirements" },
    ...(bp.requirements ?? []).map((r) => el("li", {}, el("span", { class: "mono", text: `${r.id} ` }), r.text)));
  const result = el("div", { id: "factory-check-result", role: "status" });
  const check = el("button", {
    type: "button", class: "btn small", id: "factory-check-btn",
    onclick: (e) => checkFactoryFolders(e.currentTarget, result),
  }, "Check in its folders");
  panel.replaceChildren(facts,
    el("h3", { text: "What it must do" }), requirements,
    ...(draft ? [
      el("div", { class: "card-head" }, el("h3", { text: "Problems" }), check),
      el("p", { class: "muted small", text: "Errors keep it from being built and evaluated; warnings point to less power it could ask for. Checking resolves what it asks for in the folders it is deployed to, and grants nothing." }),
      factoryProblems(draft.problems), result,
    ] : [el("p", { class: "muted small", text: "Its files are its publisher's, exactly: it isn't edited here. Share says where it stands." })]),
    factoryLifecycle(agent));
}

/// Where the agent is in its life, what can happen next (deprecate, reactivate, retire),
/// and what happened so far.
function factoryLifecycle(agent) {
  if (!agent) return null;
  const name = agent.name;
  const box = el("div", { class: "factory-lifecycle" }, el("h3", { text: "Lifecycle" }));
  const say = {
    draft: "Not released yet: release it from a passed evaluation under Releases; plans can use it then.",
    imported: "Imported from a peer: evaluate it here before you release it.",
    active: `Active: plans use release ${agent.active_version} as agent=${name}.`,
    deprecated: `Deprecated: plans that name it are warned and pointed to ${agent.replacement}. It keeps working until you retire it.`,
    retired: `Retired: plans refuse it and its digital workers stopped. Its releases and reports are kept for audit${agent.purge_after ? `, until ${stamp(Date.parse(agent.purge_after))}` : ""}.`,
  }[agent.state] ?? agent.state;
  box.append(el("p", { class: "small", text: say }));
  const form = el("div", { class: "factory-inline", id: "factory-life-form", hidden: true });
  const actions = el("div", { class: "factory-actions" });
  const open = (build) => {
    form.replaceChildren(...build());
    form.hidden = false;
    form.querySelector("select, input")?.focus();
  };
  const close = (back) => {
    form.hidden = true;
    form.replaceChildren();
    back?.focus();
  };
  if (agent.state === "active" || agent.state === "deprecated") {
    if (agent.state === "active") {
      const deprecate = el("button", { type: "button", class: "btn small", id: "factory-deprecate-btn" }, "Deprecate…");
      deprecate.addEventListener("click", () => open(() => factoryDeprecateForm(agent, () => close(deprecate))));
      actions.append(deprecate);
    } else {
      const reactivate = el("button", { type: "button", class: "btn small", id: "factory-reactivate-btn" }, "Reactivate");
      reactivate.addEventListener("click", () => factoryLifecycleSet(reactivate, { state: "active" }, `${name} is active again`));
      actions.append(reactivate);
    }
    const retire = el("button", { type: "button", class: "btn small danger", id: "factory-retire-btn" }, "Retire…");
    retire.addEventListener("click", () => open(() => factoryRetireForm(agent, () => close(retire))));
    actions.append(retire);
    box.append(actions, form);
  }
  const history = agent.history ?? [];
  if (history.length) {
    box.append(el("details", { class: "factory-history-box" },
      el("summary", { text: `History (${history.length})` }),
      el("ol", { class: "factory-history" }, ...history.map((h) => el("li", {},
        el("span", { class: "muted", text: `${stamp(Date.parse(h.at))} · ${h.by}: ` }),
        h.what, h.why ? el("span", { class: "muted", text: ` (${h.why})` }) : null)))));
  }
  return box;
}

function factoryDeprecateForm(agent, cancel) {
  const others = (state.agents ?? []).filter((a) => a.name !== agent.name);
  const replacement = el("select", { id: "factory-replacement" },
    ...others.map((a) => el("option", { value: a.name, text: a.name })));
  const reason = el("input", { type: "text", id: "factory-deprecate-reason", maxlength: "500", placeholder: "e.g. fin-analyst-2 reads tables better…" });
  const error = el("p", { class: "form-error", role: "alert" });
  const go = el("button", { type: "button", class: "btn primary small" }, "Deprecate");
  go.addEventListener("click", () => {
    if (!replacement.value) {
      error.textContent = "Pick the agent plans should use instead.";
      replacement.focus();
      return;
    }
    factoryLifecycleSet(go, { state: "deprecated", replacement: replacement.value, reason: reason.value.trim() || undefined },
      `${agent.name} is deprecated: plans are pointed to ${replacement.value}`, error);
  });
  return [
    factoryField("factory-replacement", "Use instead", replacement,
      others.length ? "Plans that name it are warned and pointed to this agent. It keeps working." : "There's no other agent yet: add one to the factory or under Tools."),
    factoryField("factory-deprecate-reason", "Why (optional)", reason),
    error,
    el("div", { class: "factory-actions" }, go, el("button", { type: "button", class: "btn small", onclick: cancel }, "Cancel")),
  ];
}

function factoryRetireForm(agent, cancel) {
  const reason = el("input", { type: "text", id: "factory-retire-reason", maxlength: "500", placeholder: "e.g. The quarterly reports moved to another team…" });
  const purge = el("input", { type: "checkbox", id: "factory-retire-purge" });
  const error = el("p", { class: "form-error", role: "alert" });
  const go = el("button", { type: "button", class: "btn danger small" }, "Retire");
  go.addEventListener("click", async () => {
    if (!reason.value.trim()) {
      error.textContent = "Say why it is retired: its history keeps it.";
      reason.setAttribute("aria-invalid", "true");
      reason.focus();
      return;
    }
    const workers = (agent.deployments ?? []).filter((d) => d.state !== "stopped").map((d) => d.worker);
    const ok = await confirmDialog({
      title: `Retire ${agent.name}?`,
      body: `Plans refuse it from now on${workers.length ? `, and its digital workers stop (${workers.join(", ")})` : ""}. It can't be made active again. Its releases and reports are kept for audit${purge.checked ? ", then deleted 90 days from now" : ""}.`,
      confirm: "Retire agent",
      danger: true,
    });
    if (!ok) return;
    factoryLifecycleSet(go, { state: "retired", reason: reason.value.trim(), purge: purge.checked }, `Retired ${agent.name}`, error);
  });
  return [
    factoryField("factory-retire-reason", "Why", reason, "Kept in its history."),
    el("label", { class: "check" }, purge, el("span", { text: "Delete its releases, reports and builds 90 days from now" })),
    error,
    el("div", { class: "factory-actions" }, go, el("button", { type: "button", class: "btn small", onclick: cancel }, "Cancel")),
  ];
}

async function factoryLifecycleSet(button, body, done, error) {
  if (error) error.textContent = "";
  button.disabled = true;
  try {
    await api(`/api/factory/agents/${encodeURIComponent(factoryState.open)}/lifecycle`, { method: "POST", body: JSON.stringify(body) });
    toast(done);
    await refreshFactoryAgent(false);
    await loadFactory().catch(() => {});
    $("#ftab-overview").focus();
  } catch (err) {
    if (error) error.textContent = err.message;
    else toast(err.message, "error");
    button.disabled = false;
    button.focus();
  }
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
  const panel = $("#factory-json-box");
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

// ---------- configure: the blueprint as a form ----------
const FACTORY_ACCESS = [["read-only", "Read only"], ["workspace-write", "Edit files"], ["full", "Full: edit files, run checks"]];
const FACTORY_M365 = ["all", "email", "meetings", "files", "teams", "people", "connectors"];
const FACTORY_EFFORTS = ["minimal", "low", "medium", "high", "xhigh", "max"];
const FACTORY_PREFER = [["cost", "Lowest cost"], ["latency", "Fastest"], ["quality", "Best results"]];
const FACTORY_PROVIDERS = ["copilot", "claude", "codex"];

/// Configure shows the blueprint as a form, or as JSON (all of it, its eval suite too).
function setFactoryEditMode(mode, focus = false) {
  factoryState.editMode = mode;
  for (const b of document.querySelectorAll("#factory-edit-mode [role=radio]")) {
    const on = b.dataset.mode === mode;
    b.setAttribute("aria-checked", String(on));
    b.tabIndex = on ? 0 : -1;
    if (on && focus) b.focus();
  }
  $("#factory-form-box").hidden = mode !== "form";
  $("#factory-json-box").hidden = mode !== "json";
}

/// A field: its label above, its hint below.
function factoryField(id, label, control, hint) {
  control.id = id;
  if (hint) control.setAttribute("aria-describedby", `${id}-hint`);
  return el("div", { class: "field" },
    el("label", { for: id, text: label }), control,
    hint ? el("span", { class: "muted small", id: `${id}-hint`, text: hint }) : null);
}

/// Checkbox chips of `options` ([value, label, about]), those in `checked` ticked. What the
/// blueprint names that isn't offered here now is shown too, so saving keeps it.
function factoryChips(key, legend, options, checked, hint) {
  const offered = new Set(options.map(([v]) => v));
  const all = [...options, ...checked.filter((v) => !offered.has(v)).map((v) => [v, `${v} (not here)`, "It names this, but it isn't offered here now"])];
  const hintId = `fcfg-${key}-hint`;
  return el("fieldset", { class: "field", id: `fcfg-${key}`, "aria-describedby": hint ? hintId : undefined },
    el("legend", { text: legend }),
    all.length
      ? el("div", { class: "mcp-options factory-chips" }, ...all.map(([value, label, about]) => el("label", { class: "mcp-chip", title: about || undefined },
        el("input", { type: "checkbox", name: `fcfg-${key}`, value, checked: checked.includes(value) || undefined }),
        el("span", { text: label }))))
      : el("p", { class: "muted small", text: "None here yet." }),
    hint ? el("span", { class: "muted small", id: hintId, text: hint }) : null);
}

function factoryTicked(form, key) {
  return [...form.querySelectorAll(`input[name="fcfg-${key}"]:checked`)].map((i) => i.value);
}

/// The number in `input`: none when it's empty, else why it isn't one from `min` to `max`.
function factoryNumberOf(input, min, max, whole = true) {
  const raw = input.value.trim();
  if (!raw && !input.validity?.badInput) return { value: undefined };
  const n = Number(raw);
  if (!raw || !Number.isFinite(n) || n < min || n > max || (whole && !Number.isInteger(n))) {
    return { error: `Give ${whole ? "a whole number" : "a number"} from ${min} to ${max}, or leave it empty.` };
  }
  return { value: n };
}

/// A row of one of the form's lists (requirements, MCP servers, models), with its Remove.
function factoryRow(kind, what, fields, add, after = null) {
  const remove = el("button", { type: "button", class: "btn small", "aria-label": `Remove ${what}` }, "Remove");
  const row = el("li", { class: `factory-row ${kind}` }, ...fields, remove, after);
  remove.addEventListener("click", () => {
    const next = row.nextElementSibling ?? row.previousElementSibling;
    row.remove();
    factoryState.formEdited = true;
    (next?.querySelector("input, select") ?? add).focus();
  });
  return row;
}

function factoryRequirementRow(r, add) {
  const text = el("input", {
    type: "text", maxlength: "500", value: r.text ?? "", "data-req": r.id, "aria-label": `Requirement ${r.id}`,
    placeholder: "e.g. Quotes each figure with the page it came from…",
  });
  return factoryRow("req", `requirement ${r.id}`, [el("span", { class: "mono req-id", text: r.id }), text], add);
}

/// An MCP server it asks for; `offered`: each server your folders offer now, with its tools
/// (none listed: all of them).
function factoryMcpRow(m, offered, add) {
  const names = [...new Set([...offered.keys(), ...(m.server ? [m.server] : [])])];
  const server = el("select", { "data-mcp": "server", "aria-label": "MCP server" },
    ...names.map((s) => el("option", { value: s, text: offered.has(s) ? s : `${s} (not offered)` })));
  if (m.server) server.value = m.server;
  const hintId = `fcfg-row-${++factoryState.rows}`;
  const tools = el("input", {
    type: "text", "data-mcp": "tools", value: (m.tools ?? []).join(", "), spellcheck: "false", autocomplete: "off",
    "aria-label": "Tools it calls", "aria-describedby": hintId, placeholder: "e.g. read_pdf, or * for all…",
  });
  const why = el("input", { type: "text", "data-mcp": "why", value: m.why ?? "", maxlength: "300", "aria-label": "Why it needs it", placeholder: "e.g. Reports are PDF files…" });
  const offers = el("span", { class: "muted small factory-row-hint", id: hintId });
  const show = () => {
    const t = offered.get(server.value);
    offers.textContent = t === undefined ? `Your folders don't offer ${server.value} now.` : t.length ? `${server.value} offers ${t.join(", ")}` : `${server.value} offers all its tools`;
  };
  server.addEventListener("change", show);
  show();
  return factoryRow("mcp", `MCP server ${m.server ?? ""}`.trim(), [server, tools, why], add, offers);
}

function factoryCandidateRow(c, add) {
  const provider = el("select", { "data-cand": "provider", "aria-label": "Provider" },
    ...FACTORY_PROVIDERS.map((p) => el("option", { value: p, text: providerLabel(p) })));
  provider.value = c.provider ?? FACTORY_PROVIDERS[0];
  const model = el("input", {
    type: "text", "data-cand": "model", value: c.model ?? "", spellcheck: "false", autocomplete: "off",
    "aria-label": "Model", placeholder: "e.g. gpt-6-sol…",
  });
  const effort = el("select", { "data-cand": "effort", "aria-label": "Reasoning effort" },
    el("option", { value: "", text: "Default effort" }), ...FACTORY_EFFORTS.map((e) => el("option", { value: e, text: e })));
  effort.value = c.effort ?? "";
  return factoryRow("cand", `model ${c.model ?? ""}`.trim(), [provider, model, effort], add);
}

function renderFactoryConfigure() {
  const box = $("#factory-form-box");
  const draft = factoryState.draft;
  if (!draft) {
    box.replaceChildren(el("p", { class: "muted", text: "This agent has no draft to configure." }));
    return;
  }
  factoryState.formEdited = false;
  const bp = draft.blueprint;
  const choices = factoryState.agent?.choices ?? {};
  const caps = bp.capabilities ?? {};
  const model = bp.model ?? {};
  const memory = bp.memory ?? {};
  const budgets = bp.budgets ?? {};
  const folders = choices.folders ?? [];
  const offered = new Map();
  for (const f of folders) {
    for (const m of f.mcp ?? []) {
      const had = offered.get(m.server);
      // A folder that offers all its tools: all of them; else what any folder offers.
      offered.set(m.server, had === undefined ? [...(m.tools ?? [])] : had.length && m.tools?.length ? [...new Set([...had, ...m.tools])] : []);
    }
  }
  const text = (attrs) => el("input", { type: "text", autocomplete: "off", ...attrs });
  const number = (attrs) => el("input", { type: "number", inputmode: "numeric", class: "factory-number", ...attrs });
  const section = (heading, ...children) => el("section", { class: "factory-form-section" }, el("h3", { text: heading }), ...children);

  const addReq = el("button", { type: "button", class: "btn small", id: "fcfg-add-req" }, "Add requirement");
  const reqs = el("ol", { class: "factory-rows", id: "fcfg-reqs", "aria-label": "Requirements" },
    ...(bp.requirements ?? []).map((r) => factoryRequirementRow(r, addReq)));
  addReq.addEventListener("click", () => {
    const n = Math.max(0, ...[...reqs.querySelectorAll("[data-req]")].map((i) => Number(i.dataset.req.replace(/^R/, "")) || 0));
    const row = factoryRequirementRow({ id: `R${n + 1}`, text: "" }, addReq);
    reqs.append(row);
    factoryState.formEdited = true;
    row.querySelector("input").focus();
  });
  const instructions = el("textarea", { rows: "8", maxlength: "32768", class: "factory-instructions", placeholder: "e.g. You analyze financial reports for an executive audience. Quote figures exactly…" });
  instructions.value = bp.instructions ?? "";
  const access = el("select", {}, ...FACTORY_ACCESS.map(([v, label]) => el("option", { value: v, text: label })));
  access.value = caps.access ?? "read-only";
  const addMcp = el("button", { type: "button", class: "btn small", id: "fcfg-add-mcp", disabled: !offered.size || undefined }, "Add MCP server");
  const mcpRows = el("ul", { class: "factory-rows", id: "fcfg-mcp", "aria-label": "MCP servers" },
    ...(caps.mcp ?? []).map((m) => factoryMcpRow(m, offered, addMcp)));
  addMcp.addEventListener("click", () => {
    const used = new Set([...mcpRows.querySelectorAll("[data-mcp=server]")].map((s) => s.value));
    const next = [...offered.keys()].find((s) => !used.has(s)) ?? [...offered.keys()][0];
    const row = factoryMcpRow({ server: next, tools: [], why: "" }, offered, addMcp);
    mcpRows.append(row);
    factoryState.formEdited = true;
    row.querySelector("select").focus();
  });
  const delegate = el("input", { type: "checkbox", id: "fcfg-delegate", checked: caps.delegate || undefined });
  const prefer = el("select", {}, ...FACTORY_PREFER.map(([v, label]) => el("option", { value: v, text: label })));
  prefer.value = model.prefer ?? "cost";
  const addCand = el("button", { type: "button", class: "btn small", id: "fcfg-add-cand" }, "Add model");
  const cands = el("ul", { class: "factory-rows", id: "fcfg-cands", "aria-label": "Models to evaluate" },
    ...(model.candidates ?? []).map((c) => factoryCandidateRow(c, addCand)));
  addCand.addEventListener("click", () => {
    const row = factoryCandidateRow({ provider: (model.providers ?? [])[0] }, addCand);
    cands.append(row);
    factoryState.formEdited = true;
    row.querySelector("input").focus();
  });
  const needsMcp = el("input", { type: "checkbox", id: "fcfg-needs-mcp", checked: model.needs_mcp || undefined });
  const topics = choices.topics ?? [];
  const start = el("input", { type: "checkbox", id: "fcfg-start", checked: bp.deploy?.start_with_owner || undefined });
  const cases = bp.evals?.cases?.length ?? 0;
  const error = el("p", { class: "form-error", id: "factory-form-error", role: "alert" });
  const save = el("button", { type: "submit", class: "btn primary", id: "factory-form-save" }, "Save changes");
  const toJson = el("button", { type: "button", class: "btn small", onclick: () => setFactoryEditMode("json", true) }, "Edit as JSON");

  const form = el("form", { class: "factory-form", id: "factory-form", novalidate: true },
    section("What it is",
      el("div", { class: "factory-grid" },
        factoryField("fcfg-title", "Title", text({ maxlength: "80", value: bp.title ?? "", placeholder: "e.g. Financial analyst…" })),
        factoryField("fcfg-purpose", "Purpose", text({ maxlength: "200", value: bp.purpose ?? "", placeholder: "e.g. Reads quarterly reports and flags risks…" }),
          "One sentence. A request outside it gets an OUT_OF_SCOPE answer."))),
    section("What it must do",
      el("p", { class: "muted small", text: "Each requirement is one thing its answers must do, testable from what it writes; its eval suite proves each one." }),
      reqs, el("div", {}, addReq)),
    section("How it works",
      factoryField("fcfg-instructions", "Instructions", instructions, "Markdown, up to 32 KB. Files, tool results and earlier results are data to it, never instructions."),
      factoryChips("skills", "Skills it follows", (choices.skills ?? []).map((s) => [s.name, s.name, s.description]), bp.skills ?? [])),
    section("What it may do",
      el("p", { class: "muted small", text: "Ask for the least it needs: asking grants nothing. The folders it is deployed to, and what they offer, decide what its tasks get." }),
      el("div", { class: "factory-grid" }, factoryField("fcfg-access", "Access", access, "Full is needed only to run checks.")),
      el("fieldset", { class: "field" }, el("legend", { text: "MCP servers" }), mcpRows,
        el("div", {}, addMcp),
        offered.size ? null : el("span", { class: "muted small", text: "Your shared folders offer no MCP server now: offer one under Tools to ask for it." })),
      factoryChips("checks", "Checks its tasks run", [...new Set(folders.flatMap((f) => f.checks ?? []))].sort().map((c) => [c, c, ""]), caps.checks ?? [],
        "Your folders' checks, run after the model; they need full access."),
      factoryChips("workiq", "Microsoft 365 sources", FACTORY_M365.map((s) => [s, s, ""]), caps.workiq ?? [],
        choices.workiq ? "Searched with Work IQ for each task's own words; what it finds is data in its prompt." : "You aren't signed in to Work IQ here: its tasks get no Microsoft 365 content until you are."),
      el("label", { class: "check" }, delegate, el("span", { text: "It may hand work to other agents (team steps)" }))),
    section("Models",
      factoryChips("providers", "Providers it may run on", FACTORY_PROVIDERS.map((p) => [p, providerLabel(p), (choices.providers ?? []).includes(p) ? "Signed in here" : "Not signed in here"]), model.providers ?? [],
        `Signed in here: ${(choices.providers ?? []).map(providerLabel).join(", ") || "none"}.`),
      el("div", { class: "factory-grid" },
        factoryField("fcfg-prefer", "Between models that pass, prefer", prefer),
        factoryField("fcfg-multiplier", "Highest cost multiplier", number({ min: "0.1", max: "100", step: "any", value: model.max_multiplier ?? "", placeholder: "e.g. 1.5…" }), "Empty: no limit.")),
      el("fieldset", { class: "field" }, el("legend", { text: "Models to evaluate" }), cands, el("div", {}, addCand),
        el("span", { class: "muted small", text: "None: each provider's default model. The evaluation recommends one; a release runs on it." })),
      el("label", { class: "check" }, needsMcp, el("span", { text: "Its models must use MCP servers (Codex can't here)" }))),
    section("Memory",
      factoryChips("topics", "Knowledge in every task", topics.map((t) => [t, t, ""]), memory.context ?? []),
      el("div", { class: "factory-grid" },
        factoryField("fcfg-lessons", "Lessons go to topic", text({ value: memory.lessons ?? "", spellcheck: "false", placeholder: "e.g. fin-analyst-lessons…" }), "From verified results only. Empty: it keeps none."),
        factoryField("fcfg-retention", "Days lessons are kept", number({ min: "1", max: "3650", step: "1", value: memory.retention_days ?? "", placeholder: "90" })))),
    section("Budgets",
      el("p", { class: "muted small", text: "Its digital workers keep to them before any model call. Empty: no limit." }),
      el("div", { class: "factory-grid" },
        factoryField("fcfg-tasks", "Tasks a day", number({ min: "1", max: "10000", step: "1", value: budgets.tasks_per_day ?? "" })),
        factoryField("fcfg-attempts", "Attempts a task", number({ min: "1", max: "5", step: "1", value: budgets.attempts ?? "" })),
        factoryField("fcfg-timeout", "Minutes a task", number({ min: "1", max: "10080", step: "any", value: budgets.timeout_secs ? budgets.timeout_secs / 60 : "" })))),
    section("Deployment and sign-offs",
      factoryChips("folders", "Folders it works in when deployed", folders.map((f) => [f.name, f.name, `${ACCESS_SHORT[f.access] ?? f.access} at most`]), bp.deploy?.folders ?? []),
      el("label", { class: "check" }, start, el("span", { text: "Its digital workers start with your agent" })),
      factoryChips("approvers", "Who signs off each release", [["me", "me (you)", "You, in this UI"], ...(choices.peers ?? []).map((p) => [p, p, "On your allowlist"])], bp.approvers ?? [],
        "Each must accept a release before it is active; you also sign off on anything it asks for beyond the release before.")),
    section("Eval suite",
      el("p", { class: "muted small", text: `${factoryPlural(cases, "case proves", "cases prove")} its requirements; you edit them as JSON.` }),
      el("div", {}, toJson)),
    error,
    el("div", { class: "factory-actions" }, save),
    el("div", { id: "factory-form-result", role: "status" }));
  const edited = () => { factoryState.formEdited = true; };
  form.addEventListener("input", edited);
  form.addEventListener("change", edited);
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    saveFactoryForm(form, save, error);
  });
  box.replaceChildren(form);
}

/// The blueprint the form says: the draft's, with what the form edits; or the first field in
/// error and why.
function factoryFormValue(form) {
  const bp = structuredClone(factoryState.draft.blueprint);
  const wrong = (field, error) => ({ field, error });
  const value = (id) => $(`#${id}`).value.trim();
  bp.title = value("fcfg-title");
  if (!bp.title) return wrong($("#fcfg-title"), "Give it a title.");
  bp.purpose = value("fcfg-purpose");
  if (!bp.purpose) return wrong($("#fcfg-purpose"), "Say what it is for, in a sentence.");
  bp.requirements = [];
  for (const input of form.querySelectorAll("[data-req]")) {
    if (!input.value.trim()) return wrong(input, `Write requirement ${input.dataset.req}, or remove it.`);
    bp.requirements.push({ id: input.dataset.req, text: input.value.trim() });
  }
  if (!bp.requirements.length) return wrong($("#fcfg-add-req"), "Add a requirement: what its answers must do.");
  bp.instructions = $("#fcfg-instructions").value;
  if (!bp.instructions.trim()) return wrong($("#fcfg-instructions"), "Write its instructions: how it works.");
  bp.skills = factoryTicked(form, "skills");
  const mcp = [];
  for (const row of form.querySelectorAll(".factory-row.mcp")) {
    const [server, tools, why] = ["server", "tools", "why"].map((k) => row.querySelector(`[data-mcp=${k}]`));
    const list = tools.value.split(",").map((t) => t.trim()).filter(Boolean);
    if (!list.length) return wrong(tools, `Name the tools it calls on ${server.value} (* for all of them).`);
    if (!why.value.trim()) return wrong(why, `Say why it needs ${server.value}: it is read before the server is offered.`);
    mcp.push({ server: server.value, tools: list, why: why.value.trim() });
  }
  bp.capabilities = {
    ...bp.capabilities, access: $("#fcfg-access").value, mcp, checks: factoryTicked(form, "checks"),
    workiq: factoryTicked(form, "workiq"), delegate: $("#fcfg-delegate").checked,
  };
  const providers = factoryTicked(form, "providers");
  if (!providers.length) return wrong(form.querySelector('input[name="fcfg-providers"]'), "Pick a provider it may run on.");
  const candidates = [];
  for (const row of form.querySelectorAll(".factory-row.cand")) {
    const [provider, model, effort] = ["provider", "model", "effort"].map((k) => row.querySelector(`[data-cand=${k}]`));
    if (!model.value.trim()) return wrong(model, "Name the model, or remove its row.");
    candidates.push({ provider: provider.value, model: model.value.trim(), ...(effort.value ? { effort: effort.value } : {}) });
  }
  const numbers = {};
  for (const [id, min, max, whole] of [
    ["fcfg-multiplier", 0.1, 100, false], ["fcfg-retention", 1, 3650, true],
    ["fcfg-tasks", 1, 10000, true], ["fcfg-attempts", 1, 5, true], ["fcfg-timeout", 1, 10080, false],
  ]) {
    const n = factoryNumberOf($(`#${id}`), min, max, whole);
    if (n.error) return wrong($(`#${id}`), n.error);
    numbers[id] = n.value;
  }
  bp.model = { ...bp.model, providers, candidates, prefer: $("#fcfg-prefer").value, needs_mcp: $("#fcfg-needs-mcp").checked };
  if (numbers["fcfg-multiplier"] === undefined) delete bp.model.max_multiplier;
  else bp.model.max_multiplier = numbers["fcfg-multiplier"];
  bp.memory = { context: factoryTicked(form, "topics") };
  if (value("fcfg-lessons")) bp.memory.lessons = value("fcfg-lessons");
  if (numbers["fcfg-retention"] !== undefined) bp.memory.retention_days = numbers["fcfg-retention"];
  bp.budgets = {};
  if (numbers["fcfg-tasks"] !== undefined) bp.budgets.tasks_per_day = numbers["fcfg-tasks"];
  if (numbers["fcfg-attempts"] !== undefined) bp.budgets.attempts = numbers["fcfg-attempts"];
  if (numbers["fcfg-timeout"] !== undefined) bp.budgets.timeout_secs = Math.round(numbers["fcfg-timeout"] * 60);
  bp.deploy = { folders: factoryTicked(form, "folders"), start_with_owner: $("#fcfg-start").checked };
  bp.approvers = factoryTicked(form, "approvers");
  return { blueprint: bp };
}

async function saveFactoryForm(form, button, error) {
  error.textContent = "";
  for (const bad of form.querySelectorAll("[aria-invalid]")) bad.removeAttribute("aria-invalid");
  const said = factoryFormValue(form);
  if (said.error) {
    error.textContent = said.error;
    said.field.setAttribute("aria-invalid", "true");
    said.field.focus();
    return;
  }
  button.disabled = true;
  button.textContent = "Saving…";
  try {
    const res = await api(`/api/factory/drafts/${encodeURIComponent(factoryState.open)}`, { method: "PUT", body: JSON.stringify(said.blueprint) });
    factoryState.formEdited = false;
    await refreshFactoryAgent();
    await loadFactory().catch(() => {});
    $("#factory-form-result").replaceChildren(
      el("h3", { text: res.passed ? "Saved: no errors" : "Saved, with errors to fix" }), factoryProblems(res.problems));
    toast(`Saved ${factoryState.open}`);
    $("#factory-form-save").focus();
  } catch (err) {
    error.textContent = err.message;
    button.disabled = false;
    button.textContent = "Save changes";
    button.focus();
  }
}

// ---------- releases ----------
const FACTORY_RELEASE_STATES = { released: ["Released", "done"], "pending-approval": ["Waiting for sign-off", "waiting"], rejected: ["Rejected", "failed"], broken: ["Doesn't verify", "failed"] };
const FACTORY_DEPLOY_STATES = { starting: ["Starting", "waiting"], canary: ["Canary", "running"], running: ["Running", "done"], "rolled-back": ["Rolled back", "cancelled"], stopped: ["Stopped", "failed"] };

/// Whether version `a` comes before `b` (major.minor.patch).
function factoryVersionBefore(a, b) {
  const pa = String(a).split(".").map(Number);
  const pb = String(b ?? "").split(".").map(Number);
  for (let i = 0; i < 3; i++) {
    if ((pa[i] || 0) !== (pb[i] || 0)) return (pa[i] || 0) < (pb[i] || 0);
  }
  return false;
}

function renderFactoryReleases() {
  const panel = $("#fpanel-releases");
  const agent = factoryState.agent;
  if (!agent) return;
  const releases = agent.releases ?? [];
  // An import's releases are its publisher's versions, each made once from what passed here.
  const passed = (agent.evaluations ?? []).filter((e) => e.state === "passed" && e.gate?.passed && !e.canary
    && !(agent.imported && releases.some((r) => r.build === e.build)));
  const parts = [el("p", { class: "muted small", text: `A release is a signed, exact copy of a build that passed its evaluation. Plans use the active one as agent=${agent.name}; digital workers run the one you deploy. Your approvers sign off first, and you sign off on anything it asks for beyond the release before.` })];
  if (agent.state === "retired") parts.push(el("p", { class: "small", text: "It is retired: it gets no new releases." }));
  else if (agent.imported && !passed.length) parts.push(el("p", { class: "muted small", text: `Each version of ${agent.imported.publisher}'s you import is released here, signed by you with its publisher's version, once it passes an evaluation on your accounts or you accept its publisher's report (Share).` }));
  else if (!passed.length) parts.push(el("p", { class: "empty" }, "No evaluation has passed yet. ", el("strong", { text: "Evaluate" }), " proves a build; a release is made only from one that passed."));
  else parts.push(factoryReleaseForm(agent, passed));
  parts.push(el("h3", { text: releases.length ? `Releases (${releases.length})` : "Releases" }));
  parts.push(releases.length
    ? el("ul", { class: "factory-items", id: "factory-release-list" }, ...releases.map((r) => factoryReleaseItem(agent, r)))
    : el("p", { class: "muted small", text: "None yet." }));
  panel.replaceChildren(...parts);
}

function factoryReleaseForm(agent, passed) {
  const evaluation = el("select", {}, ...passed.map((e) => el("option", { value: e.id, text: `${stamp(Date.parse(e.finished ?? e.started))} · build ${e.build.slice(0, 8)}` })));
  const version = el("input", { type: "text", maxlength: "20", spellcheck: "false", autocomplete: "off", placeholder: "e.g. 1.3.0…" });
  const note = el("textarea", { rows: "2", maxlength: "1000", placeholder: "e.g. Quotes the page of every figure…" });
  const error = el("p", { class: "form-error", role: "alert" });
  const go = el("button", { type: "submit", class: "btn primary", id: "factory-release-btn" }, "Release");
  const form = el("form", { class: "factory-inline", novalidate: true },
    el("h3", { text: "Release it" }),
    el("div", { class: "factory-grid" },
      factoryField("factory-rel-eval", "From evaluation", evaluation, "One that passed its gate: the release is exactly its build."),
      factoryField("factory-rel-version", "Version (optional)", version, "Empty: what its change needs. A new requirement or capability is a minor version; a changed or removed one, a major.")),
    factoryField("factory-rel-note", "Release note (optional)", note),
    error,
    el("div", { class: "factory-actions" }, go));
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    error.textContent = "";
    version.removeAttribute("aria-invalid");
    const v = version.value.trim();
    if (v && !/^\d+\.\d+\.\d+$/.test(v)) {
      error.textContent = "A version is three numbers, like 1.3.0.";
      version.setAttribute("aria-invalid", "true");
      version.focus();
      return;
    }
    go.disabled = true;
    go.textContent = "Releasing…";
    try {
      const body = { evaluation: evaluation.value };
      if (v) body.version = v;
      if (note.value.trim()) body.note = note.value.trim();
      const r = await api(`/api/factory/agents/${encodeURIComponent(agent.name)}/releases`, { method: "POST", body: JSON.stringify(body) });
      const waits = (r.pending ?? []).map((p) => (p.approver === "me" ? "you" : p.approver));
      toast(r.state === "released" ? `Released ${agent.name} ${r.version}` : `Release ${r.version} of ${agent.name} waits for ${waits.join(", ")}`);
      await refreshFactoryAgent(false);
      await loadFactory().catch(() => {});
      ($(`#factory-release-list [data-version="${CSS.escape(r.version)}"] button`) ?? $(`#factory-release-list [data-version="${CSS.escape(r.version)}"] summary`))?.focus();
    } catch (err) {
      error.textContent = err.message;
      go.disabled = false;
      go.textContent = "Release";
      go.focus();
    }
  });
  return form;
}

function factoryReleaseItem(agent, r) {
  const [label, kind] = FACTORY_RELEASE_STATES[r.state] ?? [r.state, "waiting"];
  const head = el("div", { class: "worker-head" },
    el("strong", { text: r.version }),
    el("span", { class: `status ${kind}`, text: label }),
    agent.active_version === r.version ? el("span", { class: "badge ok", text: "Active" }) : null,
    r.change ? el("span", { class: "badge neutral", title: r.change_why ?? "", text: `${r.change} change` }) : null);
  if (r.state === "broken") {
    return el("li", { class: "factory-item", "data-version": r.version }, head,
      el("span", { class: "small warn-text", text: `${r.error}. Don't deploy it; roll back to a release that verifies.` }));
  }
  const who = (a) => (a === "me" ? "you" : a);
  const details = el("details", { class: "factory-release-details", open: factoryState.releaseOpen.has(r.version) || undefined },
    el("summary", { text: "Manifest, changes and evaluation" }));
  const body = el("div", { class: "small" });
  details.append(body);
  // Opening it (or drawing it open) loads what the release holds.
  details.addEventListener("toggle", () => {
    if (details.open) {
      factoryState.releaseOpen.add(r.version);
      factoryReleaseDetails(agent.name, r.version, body);
    } else {
      factoryState.releaseOpen.delete(r.version);
    }
  });
  return el("li", { class: "factory-item", "data-version": r.version }, head,
    el("span", { class: "muted small", text: `${stamp(Date.parse(r.created))} · signed by ${r.signer} · from evaluation ${r.evaluation}${r.model ? ` · runs on ${factoryRoute(r.model)}` : ""}` }),
    r.note ? el("span", { class: "small", text: r.note }) : null,
    r.increases?.length ? el("span", { class: "small warn-text", text: `Asks for more than the release before: ${r.increases.join("; ")}` }) : null,
    ...(r.approvals ?? []).map((a) => el("span", { class: "small", text: `${who(a.approver)} ${a.approved ? "approved" : "rejected"} it${a.note ? `: ${a.note}` : ""} (${stamp(Date.parse(a.at))})` })),
    r.pending?.length ? el("span", { class: "small", text: `Waiting for ${r.pending.map((p) => who(p.approver)).join(", ")}` }) : null,
    r.why ? el("span", { class: "small", text: r.why }) : null,
    r.origin?.commit ? el("span", { class: "muted small mono", text: `From commit ${r.origin.commit.slice(0, 12)}${r.origin.dir ? ` of ${r.origin.dir}` : ""}` }) : null,
    (r.pending ?? []).some((p) => p.approver === "me") ? factoryOwnAnswer(agent.name, r.version) : null,
    details);
}

/// Your own sign-off of release `version`, answered here as under Waiting for your approval,
/// where the factory asks for it.
function factoryOwnAnswer(name, version) {
  const asked = (state.approvals ?? []).find((a) => a.title?.includes(`agent ${name} ${version}`));
  if (!asked) {
    return el("p", { class: "small factory-own-answer", "data-own-answer": version, text: "It waits for you: you're asked here and under Waiting for your approval within seconds." });
  }
  const note = el("input", { type: "text", maxlength: "500", class: "approval-note", "aria-label": `Note for release ${version}`, placeholder: "Note (required to reject)…" });
  const error = el("p", { class: "form-error", role: "alert" });
  const answer = async (yes, button) => {
    if (!yes && !note.value.trim()) {
      error.textContent = "Say why you reject it.";
      note.focus();
      return;
    }
    button.disabled = true;
    try {
      await api(`/api/approvals/${asked.id}`, { method: "POST", body: JSON.stringify({ approved: yes, changes: false, note: note.value }) });
      toast(yes ? `Approved ${name} ${version}` : `Rejected ${name} ${version}`);
      // Its answer goes when the factory records it: focus stays in the panel meanwhile.
      $("#ftab-releases").focus();
    } catch (err) {
      button.disabled = false;
      error.textContent = err.message;
    }
  };
  return el("div", { class: "factory-own-answer", "data-own-answer": version },
    el("strong", { class: "small", text: `${asked.title} It waits for you.` }),
    el("details", {}, el("summary", { text: "What you're signing off" }),
      el("div", { class: "result", tabindex: "0" }, renderMarkdown(asked.text))),
    note, error,
    el("div", { class: "factory-actions" },
      el("button", { type: "button", class: "btn primary small", onclick: (e) => answer(true, e.currentTarget) }, "Approve"),
      el("button", { type: "button", class: "btn small danger", onclick: (e) => answer(false, e.currentTarget) }, "Reject")));
}

/// The approvals waiting for you changed: the releases' own sign-offs follow (but not one
/// you are typing in).
function onFactoryApprovals() {
  if (!$("#factory-agent-dialog").open) return;
  for (const box of document.querySelectorAll("#fpanel-releases [data-own-answer]")) {
    if (box.contains(document.activeElement)) continue;
    box.replaceWith(factoryOwnAnswer(factoryState.open, box.dataset.ownAnswer));
  }
}

async function factoryReleaseDetails(name, version, body) {
  body.replaceChildren(el("p", { class: "muted small", text: "Loading…" }));
  try {
    const r = await api(`/api/factory/agents/${encodeURIComponent(name)}/releases/${encodeURIComponent(version)}`);
    const files = Object.keys(r.files ?? {});
    body.replaceChildren(
      el("dl", { class: "facts" },
        el("dt", { text: "Manifest" }), el("dd", { class: "mono", text: `sha256 ${r.manifest_sha256}` }),
        el("dt", { text: "Signed by" }), el("dd", { text: `${r.signer} (${r.signer_id.slice(0, 10)}…), verified` }),
        el("dt", { text: "Asks for" }), el("dd", { text: r.capabilities }),
        el("dt", { text: "Evaluation" }), el("dd", { text: r.eval_summary ?? r.evaluation }),
        el("dt", { text: "Files" }), el("dd", { class: "mono", text: files.join(", ") || "none" })),
      r.diff ? el("h4", { text: `What changed from ${r.previous}` }) : null,
      r.diff ? (r.diff.length ? el("ul", { class: "factory-diff" }, ...r.diff.map((d) => el("li", { text: d })))
        : el("p", { class: "muted small", text: "Nothing but its version." })) : null);
  } catch (err) {
    body.replaceChildren(el("p", { class: "form-error", role: "alert", text: err.message }));
  }
}

// ---------- deployments ----------
function renderFactoryDeployments() {
  const panel = $("#fpanel-deployments");
  const agent = factoryState.agent;
  if (!agent) return;
  const released = (agent.releases ?? []).filter((r) => r.state === "released")
    .sort((a, b) => (factoryVersionBefore(a.version, b.version) ? 1 : -1));
  const deployed = agent.deployments ?? [];
  const parts = [el("p", { class: "muted small", text: "A deployment is a digital worker, named after the agent, that runs one release in the folders you pick, at the access it asks for and never more than a folder gives. Its canary runs the release's smoke cases on it before it takes work; a canary that fails goes back to the release before." })];
  if (agent.state === "retired") parts.push(el("p", { class: "small", text: "It is retired: its workers stopped, and it isn't deployed again." }));
  else if (!released.length) parts.push(el("p", { class: "empty" }, "Nothing to deploy yet: ", el("strong", { text: "Releases" }), " makes a release from a passed evaluation."));
  else parts.push(factoryDeployForm(agent, released));
  parts.push(el("h3", { text: deployed.length ? `Deployments (${deployed.length})` : "Deployments" }));
  parts.push(deployed.length
    ? el("ul", { class: "factory-items", id: "factory-deploy-list" }, ...deployed.map(factoryDeploymentItem))
    : el("p", { class: "muted small", text: "Not deployed yet." }));
  if (deployed.length && agent.state !== "retired") {
    const earlier = released.filter((r) => r.version !== agent.active_version);
    if (earlier.length) parts.push(factoryRollbackForm(agent, earlier));
    parts.push(factoryBudgetsForm(agent));
  }
  panel.replaceChildren(...parts);
}

function factoryDeployForm(agent, released) {
  const version = el("select", {}, ...released.map((r) => el("option", { value: r.version, text: r.version === agent.active_version ? `${r.version} (active)` : r.version })));
  version.value = agent.active_version ?? released[0].version;
  const folders = factoryChips("dep-folders", "Folders it works in",
    (agent.choices?.folders ?? []).map((f) => [f.name, f.name, `${ACCESS_SHORT[f.access] ?? f.access} at most`]),
    factoryState.draft?.blueprint?.deploy?.folders ?? [], "Its own data folder too. None ticked: the folders its release names.");
  const canary = el("input", { type: "checkbox", id: "factory-dep-canary", checked: true });
  const start = el("input", { type: "checkbox", id: "factory-dep-start", checked: true });
  const error = el("p", { class: "form-error", role: "alert" });
  const go = el("button", { type: "submit", class: "btn primary", id: "factory-deploy-btn" }, "Deploy");
  const form = el("form", { class: "factory-inline", novalidate: true },
    el("h3", { text: "Deploy it" }),
    el("div", { class: "factory-grid" }, factoryField("factory-dep-version", "Release", version)),
    folders,
    el("label", { class: "check" }, canary, el("span", { text: "Run its canary first: the release's smoke cases, on its worker, before it takes work" })),
    el("label", { class: "check" }, start, el("span", { text: "Start it now (else start it later under Digital workers)" })),
    error,
    el("div", { class: "factory-actions" }, go));
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    error.textContent = "";
    go.disabled = true;
    go.textContent = "Deploying…";
    try {
      const body = { version: version.value, canary: canary.checked, start: start.checked };
      const picked = factoryTicked(form, "dep-folders");
      if (picked.length) body.folders = picked;
      const d = await api(`/api/factory/agents/${encodeURIComponent(agent.name)}/deployments`, { method: "POST", body: JSON.stringify(body) });
      toast(`Deployed ${agent.name} ${d.version} on ${d.worker}${body.canary && body.start ? ": its canary runs first" : ""}`);
      await refreshFactoryAgent(false);
      await loadFactory().catch(() => {});
      (document.getElementById(`factory-power-${d.worker}`) ?? $("#ftab-deployments")).focus();
    } catch (err) {
      error.textContent = err.message;
      go.disabled = false;
      go.textContent = "Deploy";
      go.focus();
    }
  });
  return form;
}

function factoryBudgetsText(b) {
  if (!b) return "";
  return [
    b.tasks_per_day ? `${b.tasks_per_day} tasks a day` : null,
    b.attempts ? `${factoryPlural(b.attempts, "attempt", "attempts")} a task` : null,
    b.timeout_secs ? `${factoryPlural(Math.round(b.timeout_secs / 60), "minute", "minutes")} a task` : null,
  ].filter(Boolean).join(", ");
}

function factoryDeploymentItem(d) {
  const [label, kind] = FACTORY_DEPLOY_STATES[d.state] ?? [d.state, "waiting"];
  // While it is in its canary, the canary's record is only begun.
  const canary = d.state === "canary"
    ? el("span", { class: "small", text: `Its canary runs the release's smoke cases now${d.canary?.evaluation ? ` (${d.canary.evaluation})` : ""}; it takes work once they pass.` })
    : !d.canary ? null
      : d.canary.passed
        ? el("span", { class: "small", text: `Canary passed: ${factoryPlural(d.canary.cases, "smoke case", "smoke cases")}${d.canary.evaluation ? ` (${d.canary.evaluation})` : ""}` })
        : el("div", { class: "small" },
          el("span", { class: "warn-text", text: `Canary failed: ${d.canary.failed.length} of ${factoryPlural(d.canary.cases, "smoke case", "smoke cases")}` }),
          el("ul", {}, ...d.canary.failed.map((f) => el("li", { text: f }))));
  const budgets = factoryBudgetsText(d.budgets);
  const power = d.state === "stopped" ? null
    : el("button", { type: "button", class: "btn small", id: `factory-power-${d.worker}`, "aria-label": `${d.running ? "Stop" : "Start"} ${d.worker}` }, d.running ? "Stop" : "Start");
  power?.addEventListener("click", () => factoryWorkerPower(d, power));
  return el("li", { class: "factory-item", "data-worker": d.worker },
    el("div", { class: "worker-head" },
      el("strong", { text: d.worker }),
      el("span", { class: `status ${kind}`, text: label }),
      el("span", { class: `badge ${d.running ? "ok" : "neutral"}`, text: d.running ? "Running here" : "Not running here" })),
    el("span", { class: "muted small", text: `Release ${d.version}${d.previous_version ? ` (before: ${d.previous_version})` : ""} · works in ${["its data folder", ...(d.folders ?? [])].join(", ")}${d.at ? ` · changed ${stamp(Date.parse(d.at))}` : ""}` }),
    canary,
    budgets ? el("span", { class: "small", text: `Budgets you set: ${budgets}` }) : null,
    d.note ? el("span", { class: "small", text: d.note }) : null,
    power ? el("div", { class: "factory-actions start" }, power) : null);
}

async function factoryWorkerPower(d, button) {
  button.disabled = true;
  try {
    await api(`/api/workers/${encodeURIComponent(d.worker)}/${d.running ? "stop" : "start"}`, { method: "POST" });
    toast(`${d.running ? "Stopped" : "Started"} ${d.worker}`);
  } catch (err) {
    toast(err.message, "error");
  }
  await refreshFactoryAgent(false).catch(() => {});
  (document.getElementById(`factory-power-${d.worker}`) ?? $("#ftab-deployments")).focus();
}

function factoryRollbackForm(agent, earlier) {
  const active = agent.active_version;
  const older = (v) => factoryVersionBefore(v, active);
  const to = el("select", {}, ...earlier.map((r) => el("option", { value: r.version, text: older(r.version) ? r.version : `${r.version} (newer)` })));
  const before = earlier.find((r) => older(r.version));
  if (before) to.value = before.version;
  // To an earlier release it rolls back; to a newer one (after a rollback), forward again.
  const way = () => (older(to.value) ? "back" : "forward");
  const reason = el("input", { type: "text", maxlength: "500", placeholder: "e.g. 1.3.0 misses figures in tables…" });
  const error = el("p", { class: "form-error", role: "alert" });
  const go = el("button", { type: "submit", class: "btn danger", id: "factory-rollback-btn" }, `Roll ${way()}`);
  to.addEventListener("change", () => { go.textContent = `Roll ${way()}`; });
  const form = el("form", { class: "factory-inline", novalidate: true },
    el("h3", { text: before ? "Roll back" : "Roll forward" }),
    el("div", { class: "factory-grid" },
      factoryField("factory-rb-version", "To release", to),
      factoryField("factory-rb-reason", "Why", reason, "Kept in its history and audit.")),
    error,
    el("div", { class: "factory-actions" }, go));
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    error.textContent = "";
    reason.removeAttribute("aria-invalid");
    if (!reason.value.trim()) {
      error.textContent = `Say why you roll it ${way()}.`;
      reason.setAttribute("aria-invalid", "true");
      reason.focus();
      return;
    }
    const ok = await confirmDialog({
      title: `Roll ${agent.name} ${way()} to ${to.value}?`,
      body: `Its deployments take release ${to.value} within seconds (those running here start again), and plans use it as agent=${agent.name}. Release ${active} stays, for audit and to go back to.`,
      confirm: `Roll ${way()}`,
      danger: true,
    });
    if (!ok) return;
    go.disabled = true;
    go.textContent = "Changing its release…";
    try {
      await api(`/api/factory/agents/${encodeURIComponent(agent.name)}/rollback`, { method: "POST", body: JSON.stringify({ to_version: to.value, reason: reason.value.trim() }) });
      toast(`Rolled ${agent.name} ${way()} to ${to.value}`);
      await refreshFactoryAgent(false);
      await loadFactory().catch(() => {});
      $("#ftab-deployments").focus();
    } catch (err) {
      error.textContent = err.message;
      go.disabled = false;
      go.textContent = `Roll ${way()}`;
      go.focus();
    }
  });
  return form;
}

function factoryBudgetsForm(agent) {
  const set = (agent.deployments ?? []).find((d) => factoryBudgetsText(d.budgets))?.budgets ?? {};
  const number = (id, value, max) => el("input", { type: "number", id, min: "1", max: String(max), step: "1", inputmode: "numeric", class: "factory-number", value: value ?? "" });
  const fields = [
    ["factory-bud-tasks", "Tasks a day", number("factory-bud-tasks", set.tasks_per_day, 10000), 10000, "tasks_per_day", 1],
    ["factory-bud-attempts", "Attempts a task", number("factory-bud-attempts", set.attempts, 5), 5, "attempts", 1],
    ["factory-bud-timeout", "Minutes a task", number("factory-bud-timeout", set.timeout_secs ? Math.round(set.timeout_secs / 60) : "", 10080), 10080, "timeout_secs", 60],
  ];
  const error = el("p", { class: "form-error", role: "alert" });
  const save = el("button", { type: "submit", class: "btn primary", id: "factory-budgets-btn" }, "Save budgets");
  const clear = el("button", { type: "button", class: "btn", id: "factory-budgets-clear" }, "Use the release's");
  const form = el("form", { class: "factory-inline", novalidate: true },
    el("h3", { text: "Budgets" }),
    el("p", { class: "muted small", text: "Its workers keep to them before any model call: a task over them is refused, and one that runs too long stops. They replace its release's; empty: the release's." }),
    el("div", { class: "factory-grid" }, ...fields.map(([id, label, input]) => factoryField(id, label, input))),
    error,
    el("div", { class: "factory-actions" }, clear, save));
  const put = async (body, button, done) => {
    error.textContent = "";
    button.disabled = true;
    try {
      const now = await api(`/api/factory/agents/${encodeURIComponent(agent.name)}/budgets`, { method: "PUT", body: JSON.stringify(body) });
      toast(`${done}: ${factoryBudgetsText(now) || "no limits"}`);
      await refreshFactoryAgent(false);
      $("#factory-budgets-btn")?.focus();
    } catch (err) {
      error.textContent = err.message;
      button.disabled = false;
      button.focus();
    }
  };
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const body = {};
    for (const [, , input, max, key, scale] of fields) {
      input.removeAttribute("aria-invalid");
      const n = factoryNumberOf(input, 1, max);
      if (n.error) {
        error.textContent = n.error;
        input.setAttribute("aria-invalid", "true");
        input.focus();
        return;
      }
      if (n.value !== undefined) body[key] = n.value * scale;
    }
    put(body, save, `Budgets of ${agent.name}`);
  });
  clear.addEventListener("click", () => put({}, clear, `${agent.name} keeps its release's budgets`));
  return form;
}

// ---------- metrics ----------
const FACTORY_SINCE = [["24h", "Last 24 hours"], ["7d", "Last 7 days"], ["30d", "Last 30 days"], ["90d", "Last 90 days"]];

async function loadFactoryMetrics() {
  const panel = $("#fpanel-metrics");
  const name = factoryState.open;
  if (!name) return;
  if (panel.dataset.agent !== name || !$("#factory-metrics")) {
    panel.dataset.agent = name;
    const since = el("select", {}, ...FACTORY_SINCE.map(([v, label]) => el("option", { value: v, text: label })));
    since.value = factoryState.metricsSince;
    since.addEventListener("change", () => {
      factoryState.metricsSince = since.value;
      loadFactoryMetrics();
    });
    panel.replaceChildren(
      el("p", { class: "muted small", text: "Its tasks by release, from the audit logs of the digital workers it is deployed on: counts, times, attempts, checks and models, never what was asked or answered." }),
      el("div", { class: "factory-grid" }, factoryField("factory-met-since", "Since", since)),
      el("div", { id: "factory-metrics", role: "status" }, el("p", { class: "muted small", text: "Loading…" })));
  }
  const body = $("#factory-metrics");
  body.setAttribute("aria-busy", "true");
  try {
    const m = await api(`/api/factory/agents/${encodeURIComponent(name)}/metrics?since=${encodeURIComponent(factoryState.metricsSince)}`);
    if (factoryState.open !== name) return;
    body.replaceChildren(factoryMetricsTable(m));
  } catch (err) {
    body.replaceChildren(el("p", { class: "form-error", role: "alert", text: err.message }));
  } finally {
    body.removeAttribute("aria-busy");
  }
}

function factoryUsage(u) {
  if (!u) return "–";
  const parts = [];
  if (u.input_tokens != null || u.output_tokens != null) parts.push(`${(u.input_tokens ?? 0).toLocaleString()} in, ${(u.output_tokens ?? 0).toLocaleString()} out tokens`);
  if (u.premium_requests != null) parts.push(`${u.premium_requests.toFixed(1)} premium requests`);
  if (u.cost_usd != null) parts.push(`$${u.cost_usd.toFixed(2)}`);
  return parts.join("; ") || "–";
}

function factoryMetricsTable(m) {
  if (!m.versions?.length) {
    return el("p", { class: "empty" }, "No tasks in this time. Deploy it and give its digital worker tasks (", el("strong", { text: "Run task" }),
      " on it, or a plan with agent=", m.agent, "): they're counted here by release.");
  }
  const ran = (v) => v.tasks > v.refused;
  const head = el("tr", {}, ...["Release", "Tasks", "Done", "Failed", "Refused", "p50", "p95", "Attempts", "Checks passed", "Models", "Usage"].map((h) => el("th", { scope: "col", text: h })));
  const rows = m.versions.map((v) => el("tr", {},
    el("th", { scope: "row", text: v.version }),
    el("td", { text: String(v.tasks) }),
    el("td", { text: String(v.done) }),
    el("td", { text: String(v.failed + (v.cancelled ?? 0)), title: v.cancelled ? `${v.cancelled} cancelled` : undefined }),
    el("td", { text: v.refused ? `${v.refused} (${Object.entries(v.refused_by ?? {}).map(([why, n]) => `${why} ${n}`).join(", ")})` : "0" }),
    el("td", { text: ran(v) ? `${Math.round(v.p50_secs)}s` : "–" }),
    el("td", { text: ran(v) ? `${Math.round(v.p95_secs)}s` : "–" }),
    el("td", { text: ran(v) ? v.attempts.toFixed(1) : "–" }),
    el("td", { text: v.check_pass_rate == null ? "–" : factoryPercent(v.check_pass_rate) }),
    el("td", { text: Object.entries(v.models ?? {}).map(([model, n]) => `${model} ${n}`).join(", ") || "–" }),
    el("td", { text: factoryUsage(v.usage) })));
  return el("div", { class: "factory-matrix-wrap", tabindex: "0", role: "region", "aria-label": "Tasks by release" },
    el("table", { class: "factory-matrix" }, el("thead", {}, head), el("tbody", {}, ...rows)));
}

// ---------- evaluate ----------
function factoryRunning() {
  // A deployment's canary runs on its own worker, not as one of the owner's evaluations.
  return factoryState.agent?.evaluations?.find((e) => e.running && !e.canary) ?? null;
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
  // Only a draft, or a version imported and not evaluated here yet, is evaluated here.
  if (agent && !factoryState.draft && !agent.imported_version) {
    panel.replaceChildren(el("p", { class: "muted small", text: `${agent.imported
      ? `Each version of ${agent.imported.publisher}'s you import is evaluated here once, before it is active.`
      : "It has no draft here to evaluate."} What runs is evaluated again on a schedule under Drift.` }));
    return;
  }
  const candidates = agent?.candidates ?? [];
  const leftOut = agent?.left_out ?? [];
  const reps = (factoryState.draft?.blueprint ?? agent?.blueprint)?.evals?.repetitions ?? 3;
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
  // The newest full evaluation, unless the owner picked another.
  const picked = factoryState.reportPicked && factoryState.report?.id;
  const newest = (evaluations.find((e) => !e.canary) ?? evaluations[0]).id;
  const shown = picked && evaluations.some((e) => e.id === picked) ? picked : newest;
  const pick = el("select", { id: "factory-report-pick", "aria-label": "Evaluation" },
    ...evaluations.map((e) => el("option", {
      value: e.id, selected: e.id === shown || undefined,
      text: `${stamp(Date.parse(e.started))} · ${e.canary ? `canary of ${e.canary.version} · ` : ""}${factoryEvaluationStatus(e)[0]}`,
    })));
  const body = el("div", { id: "factory-report" });
  pick.addEventListener("change", () => {
    factoryState.reportPicked = true;
    showFactoryReport(pick.value, body);
  });
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
  // What the owner's model is doing while it writes a draft, or an improvement.
  if (msg.progress) {
    if ($("#factory-new-dialog").open) $("#factory-new-progress").textContent = msg.progress;
    if (msg.agent && msg.agent === factoryState.open && $("#factory-improve-progress")) {
      $("#factory-improve-progress").textContent = msg.progress;
    }
    return;
  }
  // Many runs end close together: one refresh for them.
  clearTimeout(factoryTimer);
  factoryTimer = setTimeout(async () => {
    await loadFactory().catch(() => {});
    // Deployments start, stop and change digital workers.
    if (typeof loadWorkers === "function") loadWorkers().catch(() => {});
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
    // A modal dialog keeps focus in it: on the page's body, it was lost there (a button that
    // went away or was disabled).
    const inDialog = dialog.contains(document.activeElement) || document.activeElement === document.body;
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
  // Configure: the form or JSON, a radio group (arrow keys move between them).
  const modes = $("#factory-edit-mode");
  modes.addEventListener("click", (e) => {
    const mode = e.target.closest("[role=radio]")?.dataset.mode;
    if (mode) setFactoryEditMode(mode);
  });
  modes.addEventListener("keydown", (e) => {
    if (!["ArrowRight", "ArrowLeft", "ArrowUp", "ArrowDown"].includes(e.key)) return;
    e.preventDefault();
    setFactoryEditMode(factoryState.editMode === "form" ? "json" : "form", true);
  });
})();
