// Proofkin web UI: the agent factory at scale. An agent's drift watch (its active release
// evaluated on its deployment on a schedule, with its trend and incidents), improvements its
// owner's model proposes from what failed (a new draft version, shown as a diff next to the
// active release and evaluated next), publishing its releases to peers, and the catalog of
// what peers publish to this profile, with imports (verified, nothing run). Uses factory.js
// (factoryState, factoryField, factoryChips...) and app.js helpers ($, el, api, toast, stamp).
"use strict";

const FACTORY_DRIFT = { passed: ["Passed", "done"], failed: ["Failed", "failed"], skipped: ["Skipped", "waiting"] };
const FACTORY_IMPORT = { imported: ["Imported", "waiting"], evaluating: ["Evaluating", "running"], active: ["Active", "done"] };

function factoryWhen(at) {
  return at ? stamp(Date.parse(at)) : "";
}

/// A drift watch's run in a line.
function factoryRunText(run) {
  if (!run) return "";
  if (run.outcome === "skipped") return `Skipped: ${run.note ?? "nothing to evaluate"}`;
  const own = run.own?.total
    ? `${factoryPercent(run.own.passed / run.own.total)} of its own runs (${run.own.passed} of ${run.own.total})`
    : "no runs of its own cases";
  return `${FACTORY_DRIFT[run.outcome]?.[0] ?? run.outcome}: ${own}, the built-in attacks ${run.adversarial?.passed ?? 0} of ${run.adversarial?.total ?? 0}${run.note ? ` (${run.note})` : ""}`;
}

// ---------- drift ----------
function renderFactoryDrift() {
  const panel = $("#fpanel-drift");
  const agent = factoryState.agent;
  if (!panel || !agent) return;
  const intro = el("p", { class: "muted small", text: `Its active release, evaluated on its deployment on a schedule: every case once and the built-in attacks, judged by its release's gate and within its budgets. Each run's pass rate goes to Health (monitor factory:${agent.name}); failed runs in a row open an incident that alerts you with the failing cases.` });
  if (agent.state === "retired") {
    panel.replaceChildren(intro, el("p", { class: "muted", text: "It is retired: nothing watches it." }));
    return;
  }
  const w = agent.watch;
  panel.replaceChildren(intro, factoryWatchForm(agent, w),
    ...(w ? [factoryWatchStatus(w), factoryTrend(w), factoryIncidents(w)] : []));
}

function factoryWatchForm(agent, w) {
  const when = el("input", { type: "text", maxlength: "60", spellcheck: "false", autocomplete: "off", placeholder: "e.g. daily 03:00, every 6h…", value: w?.when ?? "daily 03:00" });
  const failAfter = el("input", { type: "number", min: "1", max: "10", step: "1", inputmode: "numeric", class: "factory-number", value: String(w?.fail_after ?? 2) });
  const error = el("p", { class: "form-error", role: "alert" });
  const save = el("button", { type: "submit", class: "btn primary", id: "factory-watch-btn" }, !w ? "Watch it" : w.enabled ? "Save" : "Resume");
  const pause = w?.enabled ? el("button", { type: "button", class: "btn", id: "factory-watch-pause" }, "Pause") : null;
  const form = el("form", { class: "factory-inline", novalidate: true },
    el("h3", { text: "Drift watch" }),
    el("div", { class: "factory-grid" },
      factoryField("factory-watch-when", "When it runs", when, "Your time: daily 03:00, every 6h, weekdays 08:30, mon,fri 17:00."),
      factoryField("factory-watch-fail", "Failed runs before an incident", failAfter, "1 to 10 in a row: 2 keeps one flaky run from alerting you.")),
    agent.active_version ? null : el("p", { class: "muted small", text: "It has no active release yet: its runs are skipped until a deployment runs one." }),
    error,
    el("div", { class: "factory-actions" }, pause, save));
  const put = async (enabled, button, done) => {
    error.textContent = "";
    when.removeAttribute("aria-invalid");
    failAfter.removeAttribute("aria-invalid");
    if (!when.value.trim()) {
      error.textContent = "Say when it runs, like daily 03:00.";
      when.setAttribute("aria-invalid", "true");
      when.focus();
      return;
    }
    const n = factoryNumberOf(failAfter, 1, 10);
    if (n.error || n.value === undefined) {
      error.textContent = n.error ?? "Give a whole number from 1 to 10.";
      failAfter.setAttribute("aria-invalid", "true");
      failAfter.focus();
      return;
    }
    button.disabled = true;
    try {
      const set = await api(`/api/factory/agents/${encodeURIComponent(agent.name)}/watch`, {
        method: "PUT", body: JSON.stringify({ when: when.value.trim(), fail_after: n.value, enabled }),
      });
      toast(done(set));
      await refreshFactoryAgent(false);
      ($("#factory-watch-pause") ?? $("#factory-watch-btn"))?.focus();
    } catch (err) {
      error.textContent = err.message;
      button.disabled = false;
      if (/schedule/.test(err.message)) {
        when.setAttribute("aria-invalid", "true");
        when.focus();
      } else button.focus();
    }
  };
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    put(true, save, (set) => `Watching ${agent.name} for drift: ${set.when}, an incident after ${factoryPlural(set.fail_after, "failed run", "failed runs")} in a row`);
  });
  pause?.addEventListener("click", () => put(false, pause, () => `${agent.name}'s drift watch is paused`));
  return form;
}

function factoryWatchStatus(w) {
  const [label, kind] = w.incident_open ? ["Incident open", "failed"]
    : !w.enabled ? ["Paused", "waiting"]
      : w.last ? FACTORY_DRIFT[w.last.outcome] ?? [w.last.outcome, "waiting"] : ["Not run yet", "waiting"];
  const failed = w.last?.failed ?? [];
  return el("dl", { class: "facts" },
    el("dt", { text: "Now" }), el("dd", {}, el("span", { class: `status ${kind}`, text: label })),
    el("dt", { text: "Next run" }), el("dd", { text: w.enabled ? factoryWhen(w.next_run) || "–" : "Paused: it runs nothing" }),
    el("dt", { text: "Last run" }), el("dd", { text: w.last
      ? `${factoryWhen(w.last.at)} · ${w.last.version}${w.last.worker ? ` on ${w.last.worker}` : ""} · ${factoryRunText(w.last)}`
      : "None yet" }),
    failed.length ? el("dt", { text: "Failing" }) : null,
    failed.length ? el("dd", {}, el("ul", { class: "small" }, ...failed.map((f) => el("li", { text: f })))) : null,
    el("dt", { text: "In a row" }), el("dd", { text: w.failures
      ? `${factoryPlural(w.failures, "failed run", "failed runs")} of the ${w.fail_after} that open an incident`
      : "No failed run" }),
    el("dt", { text: "28 days" }), el("dd", { text: w.runs
      ? `${w.passed} of ${factoryPlural(w.runs, "run", "runs")} passed its gate (${factoryPercent(w.passed / w.runs)}; the objective is 95%)`
      : "No runs yet" }));
}

function factoryTrend(w) {
  const runs = w.trend ?? [];
  if (!runs.length) return el("p", { class: "muted small", text: "Its trend shows here once it has run." });
  const say = (r) => `${stamp(r.ts * 1000)}: ${r.pass_rate == null ? "no rate" : factoryPercent(r.pass_rate)} of its own runs, ${r.ok ? "passed" : "failed"} its gate`;
  const bar = (r) => {
    const span = el("span", { class: "bar" });
    // Through the DOM: the page's policy allows no style attributes.
    span.style.height = `${Math.max(6, Math.round((r.pass_rate ?? 0) * 100))}%`;
    return span;
  };
  return el("div", { class: "factory-trend-box" },
    el("h3", { text: "Trend" }),
    el("ol", { class: "factory-trend", "aria-label": "Each run's pass rate, oldest first" },
      ...runs.map((r) => el("li", { class: r.ok ? "ok" : "bad", title: say(r) },
        bar(r),
        el("span", { class: "visually-hidden", text: say(r) })))),
    el("p", { class: "muted small", text: `${factoryPlural(runs.length, "run", "runs")}: the height is its pass rate; red ones failed the gate.` }));
}

function factoryIncidents(w) {
  const events = w.incidents ?? [];
  if (!events.length) return null;
  const kinds = { opened: ["Opened", "failed"], resolved: ["Resolved", "done"] };
  return el("div", {},
    el("h3", { text: "Incidents" }),
    el("ul", { class: "factory-items" }, ...events.map((e) => {
      const [label, kind] = kinds[e.kind] ?? [e.kind, "waiting"];
      return el("li", { class: "factory-item" },
        el("div", { class: "worker-head" }, el("span", { class: `status ${kind}`, text: label }),
          el("span", { class: "muted small", text: stamp(e.ts * 1000) })),
        el("span", { text: e.text }));
    })));
}

// ---------- improve ----------
function renderFactoryImprove() {
  const panel = $("#fpanel-improve");
  const agent = factoryState.agent;
  if (!panel || !agent) return;
  const intro = el("p", { class: "muted small", text: "Your model proposes a better version from what failed: its failing cases with their answers (secrets redacted), its deployments' failed tasks (outcomes only, never their text) and your note. The proposal is a new draft version with regression cases, shown next to the active release and evaluated next on the model it runs on. It asks for nothing more, and only a release changes what runs." });
  const why = agent.state === "retired" ? "It is retired: it isn't improved."
    : agent.imported ? `It is ${agent.imported.publisher}'s: only its publisher improves it. Tell them what failed, then import the version they publish next from your Catalog.` : null;
  const list = el("div", { id: "factory-proposals-box" }, factoryProposalList(agent));
  if (why) {
    panel.replaceChildren(intro, el("p", { class: "muted", text: why }), el("h3", { text: "Proposals" }), list);
    return;
  }
  const cases = (factoryState.draft?.blueprint?.evals?.cases ?? agent.blueprint?.evals?.cases ?? [])
    .map((c) => [c.id, `${c.id} (${c.requirement})`, c.input]);
  const picks = factoryChips("improve-cases", "Cases to improve", cases, [], "None ticked: every case that failed in the last evaluation of what runs.");
  const note = el("textarea", { rows: "3", maxlength: "2000", placeholder: "e.g. Quote the sentence each risk comes from; don't paraphrase it…" });
  const providers = agent.choices?.providers ?? [];
  const using = providers.length > 1 ? el("select", {}, ...providers.map((p) => el("option", { value: p, text: providerLabel(p) }))) : null;
  const evaluate = el("input", { type: "checkbox", id: "factory-improve-evaluate", checked: true });
  const status = el("p", { class: "muted small", id: "factory-improve-progress", role: "status" });
  const error = el("p", { class: "form-error", role: "alert" });
  const go = el("button", { type: "submit", class: "btn primary", id: "factory-improve-btn" }, "Propose an improvement");
  const form = el("form", { class: "factory-inline", novalidate: true },
    el("h3", { text: "Propose an improvement" }),
    picks,
    factoryField("factory-improve-note", "Your note (optional)", note, "What to improve, in your words, at most 2,000 characters: it's kept with the proposal."),
    using ? factoryField("factory-improve-using", "Written by", using) : null,
    el("label", { class: "check", for: "factory-improve-evaluate" }, evaluate,
      el("span", { text: "Evaluate it next, on the model its active release runs on" })),
    status, error,
    el("div", { class: "factory-actions" }, go));
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    error.textContent = "";
    go.disabled = true;
    go.textContent = "Your model is writing it…";
    status.textContent = "Your model reads what failed and writes the new version, read-only in an empty folder; it takes a minute.";
    try {
      const body = { cases: factoryTicked(form, "improve-cases"), evaluate: evaluate.checked };
      if (note.value.trim()) body.note = note.value.trim();
      if (using) body.provider = using.value;
      const res = await api(`/api/factory/agents/${encodeURIComponent(agent.name)}/improvements`, { method: "POST", body: JSON.stringify(body) });
      const id = res.proposal?.id ?? "";
      toast(res.evaluation ? `Improvement ${id} of ${agent.name} is its draft now; evaluating it (${res.evaluation.id})`
        : res.evaluation_error ? `Improvement ${id} is its draft now; it wasn't evaluated: ${res.evaluation_error}`
          : `Improvement ${id} of ${agent.name} is its draft now`);
      await refreshFactoryAgent();
      await loadFactory().catch(() => {});
      ($("#factory-proposals [data-proposal] summary") ?? $("#factory-improve-btn"))?.focus();
    } catch (err) {
      error.textContent = err.message;
      go.disabled = false;
      go.textContent = "Propose an improvement";
      status.textContent = "";
      (/note/.test(err.message) ? note : go).focus();
    }
  });
  panel.replaceChildren(intro, form, el("h3", { text: "Proposals" }), list);
}

function factoryProposalList(agent) {
  const proposals = agent.proposals ?? [];
  return proposals.length
    ? el("ul", { class: "factory-items", id: "factory-proposals" }, ...proposals.map(factoryProposalItem))
    : el("p", { class: "muted small", text: "None yet." });
}

/// The proposals again (their evaluations move on), the form as it is.
function renderFactoryProposals() {
  const box = $("#factory-proposals-box");
  if (!box) {
    renderFactoryImprove();
    return;
  }
  const focused = document.activeElement?.closest?.("#factory-proposals [data-proposal]")?.dataset.proposal;
  box.replaceChildren(factoryProposalList(factoryState.agent));
  if (focused) box.querySelector(`[data-proposal="${CSS.escape(focused)}"] summary`)?.focus();
}

/// A diff, a line each: added lines green, removed ones red.
function factoryChange(lines, label) {
  return el("pre", { class: "factory-change", tabindex: "0", "aria-label": label },
    ...lines.map((l) => el("span", { class: l.startsWith("+") ? "add" : l.startsWith("-") ? "del" : l.startsWith("~") ? "chg" : "", text: `${l}\n` })));
}

function factoryProposalItem(p) {
  const e = p.evaluation;
  const [label, kind] = e ? factoryEvaluationStatus(e) : ["Not evaluated yet", "waiting"];
  const by = p.provenance?.provider ? `${providerLabel(p.provenance.provider)}'s model${p.provenance.model ? ` (${p.provenance.model})` : ""}` : "your model";
  const parts = [
    p.failures?.length ? factoryPlural(p.failures.length, "failing case", "failing cases") : null,
    p.tasks?.length ? factoryPlural(p.tasks.length, "failed task", "failed tasks") : null,
    p.note ? "your note" : null,
  ].filter(Boolean);
  const from = parts.length > 1 ? `${parts.slice(0, -1).join(", ")} and ${parts.at(-1)}` : parts.join("");
  const changed = (p.instructions ?? []).some((l) => !l.startsWith("  "));
  return el("li", { class: "factory-item", "data-proposal": p.id },
    el("div", { class: "worker-head" },
      el("span", { class: "name mono", text: p.id }),
      el("span", { class: `status ${kind}`, text: e ? `Evaluation ${label.toLowerCase()}` : label })),
    el("span", { class: "sub", text: `Of ${p.basis}, ${factoryWhen(p.at)}, by ${by}${from ? `, from ${from}` : ""}` }),
    el("details", { class: "factory-release-details", open: true },
      // An id, so focus stays on it while the page refreshes.
      el("summary", { id: `fprop-${p.id}`, text: "What it changes, next to the active release" }),
      p.diff?.length ? factoryChange(p.diff, `What ${p.id} changes`) : el("p", { class: "muted small", text: "It changes nothing but the wording." }),
      changed ? factoryChange(p.instructions, `${p.id}'s instructions, line by line`) : null,
      p.regression?.length ? el("p", { class: "small", text: `Regression cases: ${p.regression.join(", ")}` }) : null,
      p.failures?.length ? el("ul", { class: "small" }, ...p.failures.map((f) => el("li", { text: `${f.case}: ${f.why}` }))) : null),
    p.errors ? el("p", { class: "form-error", text: `Its draft has ${factoryPlural(p.errors, "error", "errors")}: fix them under Configure.` }) : null,
    e?.state === "passed"
      ? el("div", { class: "factory-actions start" },
        el("span", { class: "small", text: "It passed: release it to make it what runs." }),
        el("button", { type: "button", class: "btn small", onclick: () => selectFactoryTab("releases", true) }, "Go to Releases"))
      : null);
}

// ---------- share ----------
function renderFactoryShare() {
  const panel = $("#fpanel-share");
  const agent = factoryState.agent;
  if (!panel || !agent) return;
  if (agent.imported) {
    panel.replaceChildren(factoryImportedBox(agent));
    return;
  }
  const intro = el("p", { class: "muted small", text: "Peers you publish it to see it in their catalog: its name, version, purpose, the most access it asks for and how its evaluation went, never its knowledge, outputs or paths. They may import a release: it is verified, and runs nothing until it passes an evaluation on their own accounts. Peers can also just delegate to its deployment." });
  const released = (agent.releases ?? []).filter((r) => r.state === "released").map((r) => r.version);
  const why = agent.state === "retired" ? "It is retired: it is published to no one."
    : !released.length ? "Release it first: only released versions are published." : null;
  if (why) {
    panel.replaceChildren(intro, el("p", { class: "muted", text: why }));
    return;
  }
  const shared = agent.publication ?? { peers: [], versions: [] };
  const peers = (agent.choices?.peers ?? []).map((p) => [p, p]);
  const versions = released.map((v) => [v, v === agent.active_version ? `${v} (active)` : v]);
  const ticked = shared.versions.length ? shared.versions : agent.active_version ? [agent.active_version] : [];
  const error = el("p", { class: "form-error", role: "alert" });
  const publish = el("button", { type: "submit", class: "btn primary", id: "factory-publish-btn" }, shared.versions.length ? "Save" : "Publish");
  const unpublish = shared.versions.length ? el("button", { type: "button", class: "btn", id: "factory-unpublish-btn" }, "Unpublish") : null;
  const now = shared.versions.length && shared.peers.length
    ? `Published: ${shared.versions.join(", ")} to ${shared.peers.join(", ")}.` : "Not published.";
  const form = el("form", { class: "factory-inline", novalidate: true },
    el("h3", { text: "Publish to peers" }),
    el("p", { class: "small", role: "status", text: now }),
    peers.length ? factoryChips("share-peers", "Peers", peers, shared.peers, "Names on your allowlist: publishing grants them the agent there.")
      : el("p", { class: "muted small", text: "No peer on your allowlist yet: allow one under Peers first." }),
    factoryChips("share-versions", "Releases", versions, ticked, "The released versions they see."),
    error,
    el("div", { class: "factory-actions" }, unpublish, peers.length ? publish : null));
  const put = async (body, button, done) => {
    error.textContent = "";
    button.disabled = true;
    try {
      const set = await api(`/api/factory/agents/${encodeURIComponent(agent.name)}/publication`, { method: "PUT", body: JSON.stringify(body) });
      toast(done(set));
      await refreshFactoryAgent(false);
      ($("#factory-unpublish-btn") ?? $("#factory-publish-btn"))?.focus();
    } catch (err) {
      error.textContent = err.message;
      button.disabled = false;
      button.focus();
    }
  };
  form.addEventListener("submit", (e) => {
    e.preventDefault();
    const body = { peers: factoryTicked(form, "share-peers"), versions: factoryTicked(form, "share-versions") };
    if (!body.peers.length || !body.versions.length) {
      error.textContent = !body.peers.length ? "Pick at least one peer." : "Pick at least one release.";
      form.querySelector(`input[name="fcfg-${body.peers.length ? "share-versions" : "share-peers"}"]`)?.focus();
      return;
    }
    put(body, publish, (set) => `Published ${agent.name} ${set.versions.join(", ")} to ${set.peers.join(", ")}`);
  });
  unpublish?.addEventListener("click", () => put({ peers: [], versions: [] }, unpublish, () => `${agent.name} is unpublished`));
  panel.replaceChildren(intro, form);
}

function factoryImportedBox(agent) {
  const o = agent.imported;
  const versions = el("ul", { class: "factory-items" }, ...(o.versions ?? []).map((v) => {
    const [label, kind] = FACTORY_IMPORT[v.state] ?? [v.state, "waiting"];
    return el("li", { class: "factory-item" },
      el("div", { class: "worker-head" }, el("span", { class: "name mono", text: v.version }), el("span", { class: `status ${kind}`, text: label })),
      el("span", { class: "sub", text: `Imported ${factoryWhen(v.at)}${v.accepted_report ? "; you accepted its publisher's report instead of evaluating it here" : ""}` }));
  }));
  const evaluate = agent.imported_version
    ? el("button", { type: "button", class: "btn primary", id: "factory-import-evaluate", onclick: () => selectFactoryTab("evaluate", true) }, `Evaluate ${agent.imported_version} on your accounts`)
    : null;
  return el("div", { class: "factory-lifecycle" },
    el("h3", { text: `Imported from ${o.publisher}` }),
    el("p", { class: "small" }, "Its publisher's key: ", el("span", { class: "mono", text: o.publisher_id }),
      ". Every file was checked against the release its publisher signed with that key, and nothing of it ran."),
    versions,
    el("p", { class: "muted small", text: "It becomes active once it passes an evaluation here, on your accounts, or once you accept its publisher's report; what it asks for beyond read-only access needs your sign-off, under Releases. Only its publisher publishes and improves it." }),
    evaluate ? el("div", { class: "factory-actions start" }, evaluate) : null);
}

// ---------- catalog ----------
const factoryCatalog = { entries: [], loaded: false };

async function loadFactoryCatalog() {
  const res = await api("/api/factory/catalog");
  factoryCatalog.entries = res.entries ?? [];
  factoryCatalog.loaded = true;
  const fresh = factoryCatalog.entries.filter((e) => !e.here).length;
  const count = $("#factory-catalog-count");
  count.textContent = String(fresh);
  count.hidden = !fresh;
  $("#factory-catalog-btn").setAttribute("aria-label", fresh ? `Catalog, ${factoryPlural(fresh, "release", "releases")} you haven't imported` : "Catalog");
  if ($("#factory-catalog-dialog").open) renderFactoryCatalog();
}

function renderFactoryCatalog() {
  const list = $("#factory-catalog-list");
  if (!factoryCatalog.entries.length) {
    list.replaceChildren(el("li", { class: "empty" }, "No peer publishes an agent to you right now. A peer who publishes one (",
      el("strong", { text: "Share" }), " on its agent) shows here while you're connected."));
    return;
  }
  const focused = document.activeElement?.closest?.("#factory-catalog-list [data-entry]")?.dataset.entry;
  list.replaceChildren(...factoryCatalog.entries.map(factoryCatalogItem));
  if (focused) list.querySelector(`[data-entry="${CSS.escape(focused)}"] button, [data-entry="${CSS.escape(focused)}"] input`)?.focus();
}

function factoryCatalogItem(e) {
  const key = `${e.publisher_id}/${e.name}@${e.version}`;
  const s = e.eval_summary ?? {};
  const [label, kind] = e.here ? FACTORY_IMPORT[e.here] ?? [e.here, "waiting"] : [null, null];
  const id = `fcat-${e.publisher_id.slice(0, 10)}-${e.name}-${e.version}`.replace(/[^A-Za-z0-9_-]/g, "-");
  const accept = el("input", { type: "checkbox", id: `${id}-accept` });
  const error = el("p", { class: "form-error", role: "alert" });
  const button = el("button", { type: "button", class: "btn small primary", "aria-label": `Import ${e.name} ${e.version} from ${e.publisher}` }, "Import");
  button.addEventListener("click", () => importFactoryEntry(e, accept.checked, button, error));
  return el("li", { class: "factory-agent", "data-entry": key },
    avatar(e.name),
    el("div", { class: "who" },
      el("span", { class: "worker-head" }, el("span", { class: "name", text: `${e.name} ${e.version}` }),
        label ? el("span", { class: `status ${kind}`, text: label }) : null),
      el("span", { class: "role", text: e.description }),
      el("span", { class: "factory-badges" },
        el("span", { class: "badge neutral", text: `From ${e.publisher}`, title: `Its publisher's key: ${e.publisher_id}` }),
        el("span", { class: `badge ${e.access === "read-only" ? "ok" : "busy"}`, text: `${ACCESS_SHORT[e.access] ?? e.access} access` })),
      el("span", { class: "sub", text: `${factoryPercent(s.pass_rate)} of its own runs passed on ${e.publisher}'s accounts · ${factoryPlural(s.cases ?? 0, "case", "cases")} of its own · the built-in attacks ${s.adversarial_passed ? "resisted" : "not all resisted"}` }),
      e.here ? el("span", { class: "sub", text: e.here === "active" ? "You imported it: it is active here." : "You imported it: open it in the Agent factory to evaluate it." })
        : el("div", { class: "worker-actions" },
          el("label", { class: "check small", for: accept.id }, accept, el("span", { text: "Accept its publisher's report instead of evaluating it here" })),
          button),
      error));
}

async function importFactoryEntry(e, accept, button, error) {
  error.textContent = "";
  if (accept) {
    const ok = await confirmDialog({
      title: `Accept ${e.publisher}'s report for ${e.name} ${e.version}?`,
      body: `It becomes active without an evaluation on your accounts: its models, folders and grants were ${e.publisher}'s when it passed. This is kept in its history. What it asks for beyond read-only access still needs your sign-off.`,
      confirm: "Import and accept",
    });
    if (!ok) return;
  }
  button.disabled = true;
  button.textContent = "Importing…";
  try {
    const imported = await api("/api/factory/imports", {
      method: "POST", body: JSON.stringify({ peer: e.publisher, name: e.name, version: e.version, accept_publisher_report: accept }),
    });
    toast(imported.state === "active" ? `Imported ${e.name} ${e.version} from ${e.publisher}: active`
      : `Imported ${e.name} ${e.version} from ${e.publisher}: verified, nothing ran; evaluate it to make it active`);
    await Promise.all([loadFactoryCatalog(), loadFactory()]).catch(() => {});
    $("#factory-catalog-dialog").close();
    openFactoryAgent(e.name, $("#factory-catalog-btn"), imported.state === "active" ? "overview" : "share");
  } catch (err) {
    error.textContent = err.message;
    button.disabled = false;
    button.textContent = "Import";
    button.focus();
  }
}

async function openFactoryCatalog() {
  try {
    await loadFactoryCatalog();
  } catch (err) {
    toast(err.message, "error");
    return;
  }
  renderFactoryCatalog();
  $("#factory-catalog-dialog").showModal();
  ($("#factory-catalog-list button") ?? $("#factory-catalog-close")).focus();
}

let factoryCatalogTimer = null;
function onFactoryCatalogEvent() {
  clearTimeout(factoryCatalogTimer);
  factoryCatalogTimer = setTimeout(() => loadFactoryCatalog().catch(() => {}), 250);
}

(function initFactoryShare() {
  $("#factory-catalog-btn").addEventListener("click", openFactoryCatalog);
  $("#factory-catalog-close").addEventListener("click", () => $("#factory-catalog-dialog").close());
  $("#factory-catalog-dialog").addEventListener("close", () => {
    if (!$("#factory-agent-dialog").open) $("#factory-catalog-btn").focus();
  });
})();
