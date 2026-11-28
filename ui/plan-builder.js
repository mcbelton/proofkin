// The plan builder: a plan's claims and steps as forms, so a plan is written without
// knowing its syntax and can't hold the usual mistakes: a step without a task or
// someone to run it, a claim nothing can prove, a step waiting for itself. It edits
// the plan text through ui/plan-text.js, so the editor's Text view shows the same plan.
"use strict";

const planBuilder = (() => {
  const VIEW_KEY = "p2pagents-plan-view";
  let plan = null;
  /// What each line of the plan text is (planText.formatPlan), for "line N" messages.
  let lines = [];
  /// The plan check's last result.
  let check = null;
  /// Cards added here, with the fields you have been in: what they still need shows
  /// once you have been there (or after Run); a plan you open shows it all at once.
  let fresh = new WeakMap();
  let showAll = false;
  /// Open sections and filters, kept across re-renders.
  const open = new Set();
  const filters = new Map();
  /// The field to focus after the next render (a card just added).
  let focusNext = null;
  /// What Work IQ answered when asked for your agents (see workiqAgentsNow).
  let workiqAgents = null;

  const editor = () => $("#plan-editor-text");
  const root = () => $("#plan-builder");
  const active = () => $("#plan-editor-body").dataset.view === "builder";
  const cardKey = (kind, i) => `${kind === "claim" ? "c" : "s"}${i}`;

  // ---------- what peers offer ----------
  const workers = () => state.peers.filter((p) => p.capabilities.length);

  /// Who can do a step: each worker in its data folder and in the folders it shares
  /// with you, and each squad.
  function runners() {
    const peers = workers().flatMap((p) => [
      [`@${p.name}`, `${p.name}, in its data folder`],
      ...(p.shares ?? []).filter((f) => f.name !== "data")
        .map((f) => [`@${p.name}:${f.name}`, `${p.name}, in ${f.name} (${ACCESS_SHORT[f.access] ?? f.access})`]),
    ]);
    const squads = (state.squads ?? []).map((q) => [`#${q.name}`, `#${q.name}: any online member`]);
    return { Peers: peers, Squads: squads };
  }

  /// Who can take a team step: a peer's folder with a team plan (its agent runs the
  /// plan on its own team), or a squad.
  function leads() {
    const folders = state.peers.flatMap((p) => (p.shares ?? []).filter((f) => f.team)
      .map((f) => [`@${p.name}:${f.name}`, `${p.name}'s team, for ${f.name}`]));
    const squads = (state.squads ?? []).map((q) => [`#${q.name}`, `#${q.name}: any online member's team`]);
    return { Teams: folders, Squads: squads };
  }

  /// Who can open a pull request step's pull request: a peer's folder that takes them (a
  /// linked repository whose owner allows it), where you have edit access.
  function openers() {
    const folders = state.peers.flatMap((p) => (p.shares ?? [])
      .filter((f) => f.pull_requests && ["workspace-write", "full"].includes(f.access))
      .map((f) => [`@${p.name}:${f.name}`, `${p.name}, from ${f.name}`]));
    return { Folders: folders };
  }

  /// Who can sign off: you, or a teammate on the allowlist.
  function signers() {
    return {
      People: [["@me", "me (you, in this UI)"],
        ...state.trusted.map((t) => [`@${t.name}`, `${t.name}${t.connected ? "" : " (not connected)"}`])],
    };
  }

  /// The folder a step runs in, as its peer (or a squad's online members) share it:
  /// checks, MCP servers, worktrees and access.
  function folderOf(target) {
    if (!target) return null;
    const [who, name = "data"] = target.slice(1).split(":");
    const peers = target.startsWith("#")
      ? state.peers.filter((p) => (state.squads ?? []).find((q) => q.name === who)?.members.some((m) => m.name === p.name))
      : state.peers.filter((p) => p.name === who);
    const shares = peers.flatMap((p) => (p.shares ?? []).filter((f) => f.name === name));
    if (!shares.length) return null;
    return {
      name,
      checks: [...new Set(shares.flatMap((f) => f.checks ?? []))],
      mcp: [...new Set(shares.flatMap((f) => f.mcp ?? []))],
      about: Object.assign({}, ...shares.map((f) => f.mcp_about ?? {})),
      worktrees: shares.some((f) => f.worktrees),
    };
  }

  // ---------- building blocks ----------
  /// A field: the label above the control, a hint and what it still needs below.
  function field(key, label, control, hint) {
    control.id = `pb-${key}`;
    control.dataset.key = key;
    const described = [hint ? `${control.id}-hint` : null, `${control.id}-need`].filter(Boolean).join(" ");
    control.setAttribute("aria-describedby", described);
    return el("div", { class: "pb-field" },
      el("label", { for: control.id, text: label }),
      control,
      hint ? el("p", { class: "muted small pb-hint", id: `${control.id}-hint`, text: hint }) : null,
      el("p", { class: "pb-need", id: `${control.id}-need`, "data-need": key, hidden: true }));
  }

  /// A group of checkboxes under a legend.
  function group(key, legend, items, hint, ...after) {
    return el("fieldset", { class: "pb-field pb-group", "data-group": key },
      el("legend", { text: legend }),
      hint ? el("p", { class: "muted small pb-hint", text: hint }) : null,
      items.length ? el("div", { class: "pb-checks" }, ...items) : null,
      ...after,
      el("p", { class: "pb-need", "data-need": key, hidden: true }));
  }

  function checkbox(key, label, checked, onChange, { disabled = false, title } = {}) {
    const box = el("input", { type: "checkbox", "data-key": key, checked, disabled });
    box.addEventListener("change", () => onChange(box.checked));
    return el("label", { class: "pb-check", title }, box, el("span", {}, label));
  }

  function select(groups, value, empty) {
    const known = Object.values(groups).flat().map(([v]) => v);
    const s = el("select", {},
      el("option", { value: "", text: empty }),
      ...Object.entries(groups).filter(([, opts]) => opts.length).map(([name, opts]) =>
        el("optgroup", { label: name }, ...opts.map(([v, label]) => el("option", { value: v, text: label })))),
      value && !known.includes(value) ? el("option", { value, text: `${value} (not available now)` }) : null);
    s.value = value ?? "";
    return s;
  }

  /// A <details> that stays open across re-renders.
  function details(key, summary, children, startOpen = false) {
    const d = el("details", { class: "pb-details", open: open.has(key) || startOpen }, el("summary", { text: summary }), ...children);
    d.addEventListener("toggle", () => (d.open ? open.add(key) : open.delete(key)));
    return d;
  }

  /// Toggles `value` in a step's list option.
  const toggle = (s, option, value) => (on) => {
    const now = planText.valuesOf(s, option);
    planText.setValues(s, option, on ? [...now, value] : now.filter((v) => v !== value));
  };

  const removeButton = (label, onclick) =>
    el("button", { type: "button", class: "icon-btn", title: label, "aria-label": label, onclick }, icon(ICONS.trash));

  // ---------- claims ----------
  function claimRow(c, i) {
    const k = cardKey("claim", i);
    const id = el("input", { type: "text", class: "mono pb-id", spellcheck: "false", autocomplete: "off" });
    id.value = c.id;
    id.addEventListener("change", () => rename("claim", c, id));
    const text = el("input", { type: "text", spellcheck: "true", placeholder: "What must be true at the end, e.g. The login change has an approved API contract" });
    text.value = planText.isTodoClaim(c.text) ? "" : c.text;
    text.addEventListener("input", () => { c.text = text.value; changed(); });
    const remove = removeButton(`Remove claim ${c.id}`, async () => {
      const proving = plan.steps.filter((s) => planText.valuesOf(s, "claims").includes(c.id)).map((s) => s.id);
      if (!planText.isTodoClaim(c.text) && !(await confirmDialog({
        title: `Remove claim ${c.id}?`,
        body: `Its statement goes${proving.length ? `, and ${proving.join(", ")} no longer ${proving.length === 1 ? "proves" : "prove"} it` : ""}.`,
        confirm: "Remove claim", danger: true,
      }))) return;
      planText.removeClaim(plan, c.id);
      changed(true);
    });
    return el("li", { class: "pb-claim", "data-card": k },
      el("div", { class: "pb-claim-row" },
        field(`${k}-id`, "Id", id),
        field(`${k}-text`, "What must be true", text),
        remove),
      el("div", { class: "pb-marks", "data-marks": k }));
  }

  // ---------- steps ----------
  function stepCard(s, i) {
    const k = cardKey("step", i);
    const name = el("input", { type: "text", class: "mono pb-id", spellcheck: "false", autocomplete: "off" });
    name.value = s.id;
    name.addEventListener("change", () => rename("step", s, name));
    const current = stepKind(s);
    const kind = el("div", { class: "segmented pb-kind", role: "radiogroup", "aria-label": `Step ${s.id} is` },
      ...[["task", "Agent task"], ["team", "Team task"], ["signoff", "Sign-off"], ["pr", "Pull request"]].map(([name, label]) => el("button", {
        type: "button", role: "radio", "aria-checked": String(current === name),
        tabindex: current === name ? "0" : "-1", "data-key": `${k}-kind-${name}`,
      }, label)));
    radioGroup(kind, (btn) => setKind(s, btn.dataset.key.slice(btn.dataset.key.lastIndexOf("-") + 1)));
    const remove = removeButton(`Remove step ${s.id}`, async () => {
      if (!planText.isTodo(planText.taskOf(s)) && !(await confirmDialog({
        title: `Remove step ${s.id}?`,
        body: "What it does goes, and steps that wait for it no longer do.",
        confirm: "Remove step", danger: true,
      }))) return;
      planText.removeStep(plan, s.id);
      changed(true);
    });
    const head = el("div", { class: "pb-head" },
      field(`${k}-id`, "Step", name), kind,
      el("span", { class: "pb-badge", "data-badge": k, hidden: true, text: "Incomplete" }),
      remove);

    const workiq = !s.approve && !s.team && !s.pr && planText.isWorkiq(s.target);
    const who = select(s.approve ? signers() : s.team ? leads() : s.pr ? openers()
      : { ...runners(), "Microsoft 365": [[planText.WORKIQ, "Microsoft 365 Copilot (@workiq)"]] }, s.target,
      s.approve ? "Choose who signs off…" : s.team ? "Choose whose team does it…" : s.pr ? "Choose whose clone opens it…" : "Choose who does it…");
    who.addEventListener("change", () => {
      planText.setTarget(s, who.value || null);
      if (s.approve) s.target = s.target?.split(":")[0] ?? null;
      changed(true);
    });
    const todo = el("textarea", {
      rows: s.pr ? 1 : 3, spellcheck: "true",
      placeholder: s.approve ? "What the approver checks, e.g. The ADRs cover every decision and the contract has examples"
        : s.pr ? "Its title, e.g. Accept empty input in the parser (without one: the title of the work it opens)"
        : workiq ? "What to ask, e.g. What did we decide about the launch date, and why?"
        : "What the agent does, e.g. Write the ADRs for the login change, and its REST API contract",
    });
    todo.value = planText.isTodo(planText.taskOf(s)) ? "" : planText.taskOf(s);
    // The text may go on over several lines: they are written under the step, indented.
    todo.addEventListener("input", () => { planText.setTask(s, todo.value); changed(); });

    const body = s.pr
      ? [field(`${k}-target`, "Whose clone opens it", who, "A peer's folder that takes pull request steps: its agent pushes the branch with the work of the steps before it there and opens the pull request with its owner's GitHub account"),
        field(`${k}-todo`, "Title", todo, "It closes the issues the work is for, carries the work's signed proof, and waits until it is merged"),
        reviewersField(s, k), waitsField(s, k), moreOptions(s, k)]
      : s.team
      ? [field(`${k}-target`, "Whose team does it", who, "A peer's folder with a team plan: its agent runs the plan on its own team, and you see each of its steps as it goes"),
        field(`${k}-todo`, "What to do", todo),
        provesField(s, k, true), waitsField(s, k), moreOptions(s, k)]
      : s.approve
      ? [field(`${k}-target`, "Who signs off", who, "Their answer is signed evidence"),
        field(`${k}-todo`, "What to check", todo, "The approver sees this and the results of the steps it waits for"),
        changesField(s, k), provesField(s, k, true), waitsField(s, k)]
      : workiq
      ? [field(`${k}-target`, "Who does it", who, "Your own agent asks Microsoft 365 Copilot (Work IQ), with the results of the steps it waits for. The answer is context for later steps, never proof"),
        field(`${k}-todo`, "What to ask", todo),
        workiqAgentField(s, k), waitsField(s, k), moreOptions(s, k)]
      : [field(`${k}-target`, "Who does it", who, "A peer and one of the folders it shares with you, any member of a squad, or Microsoft 365 Copilot"),
        field(`${k}-todo`, "What to do", todo),
        agentField(s, k),
        skillsField(s, k), checksField(s, k), provesField(s, k, planText.valuesOf(s, "verify").length > 0),
        waitsField(s, k), moreOptions(s, k)];
    return el("li", { class: `pb-step${s.approve ? " signoff" : ""}${s.team ? " team" : ""}${s.pr ? " pr" : ""}`, "data-card": k }, head, ...body,
      el("div", { class: "pb-marks", "data-marks": k }));
  }

  /// How often a sign-off's approver may ask for changes (changes=; default 2): the steps
  /// it waits for are done again with their note, then they are asked again.
  function changesField(s, k) {
    const rounds = el("select", {}, ...[[0, "Never: they approve or reject"], [1, "Once"], [2, "Up to twice"],
      [3, "Up to 3 times"], [4, "Up to 4 times"], [5, "Up to 5 times"]].map(([n, text]) => el("option", { value: String(n), text })));
    rounds.value = planText.valuesOf(s, "changes")[0] ?? "2";
    rounds.addEventListener("change", () => { planText.setValues(s, "changes", rounds.value === "2" ? null : rounds.value); changed(); });
    return field(`${k}-changes`, "May ask for changes", rounds,
      "The steps it waits for are done again with the approver's note, from their earlier work; then they are asked again");
  }

  /// Who a pull request step asks to review it (reviewers=): without any, those who
  /// signed off before it.
  function reviewersField(s, k) {
    const input = el("input", {
      type: "text", placeholder: "me, carol, octocat…", autocomplete: "off", autocapitalize: "off", spellcheck: "false",
    });
    input.value = planText.valuesOf(s, "reviewers").join(", ");
    input.addEventListener("change", () => {
      const names = input.value.split(/[\s,]+/).map((n) => n.replace(/^@/, "")).filter(Boolean);
      planText.setValues(s, "reviewers", names);
      changed();
    });
    return field(`${k}-reviewers`, "Who reviews it", input,
      "me, peers of your allowlist (the GitHub account it has for them) or GitHub logins; none asks no one. Empty: those who signed off before it");
  }

  /// The agent a task acts as (from the factory, a plugin or the profile's own): what it brings.
  function agentField(s, k) {
    const agents = state.agents ?? [];
    const chosen = planText.valuesOf(s, "agent")[0] ?? "";
    if (!agents.length && !chosen) return null;
    const label = (a) => `${a.name}${a.description ? `: ${a.description}` : ""}`;
    const pick = select({
      Yours: agents.filter((a) => a.source === "profile").map((a) => [a.name, label(a)]),
      "From the factory": agents.filter((a) => a.source === "factory").map((a) => [a.name, `${a.name} ${a.version}${a.deprecated ? " (deprecated)" : ""}`]),
      "From plugins": agents.filter((a) => a.source === "plugin").map((a) => [a.name, `${label(a)} (plugin ${a.plugin})`]),
    }, chosen, "None: just the task");
    pick.addEventListener("change", () => { planText.setValues(s, "agent", pick.value || null); changed(true); });
    // agent=name@version pins a factory agent's release.
    const a = agents.find((x) => x.name === chosen) ?? agents.find((x) => x.source === "factory" && `${x.name}@${x.version}` === chosen);
    const brings = a && [
      a.source === "factory" ? `Its release's instructions come first (${chosen.includes("@") ? `release ${chosen.split("@")[1]}` : `its active release, ${a.version}`})` : "Its instructions come first",
      a.skills.length ? `it follows ${a.skills.join(", ")}` : "",
      a.mcp.length ? `it uses the MCP servers ${a.mcp.join(", ")}, which the folder must offer` : "",
      a.deprecated ? `it is deprecated: use ${a.deprecated} instead` : "",
    ].filter(Boolean).join("; ");
    return field(`${k}-agent`, "Agent", pick,
      brings ? `${brings}.` : "A role from the factory, a plugin or your own, with its instructions, skills and MCP servers");
  }

  /// The declarative agents Work IQ offers your account (GET /api/workiq/agents), asked for
  /// once each time the editor opens: { agents } or { error }, { loading } until it answers.
  function workiqAgentsNow() {
    if (workiqAgents) return workiqAgents;
    const asked = (workiqAgents = { loading: true });
    api("/api/workiq/agents")
      .then((res) => Object.assign(asked, { loading: false, agents: res.agents ?? [] }))
      .catch((e) => Object.assign(asked, { loading: false, error: e.message }))
      .finally(() => {
        if (workiqAgents === asked && active()) render();
      });
    return asked;
  }

  /// The Work IQ agent a @workiq step asks (agent=ID): none, Microsoft 365 Copilot answers.
  function workiqAgentField(s, k) {
    const now = workiqAgentsNow();
    const chosen = planText.valuesOf(s, "agent")[0] ?? "";
    const agents = now.agents ?? [];
    const named = (a) => (agents.filter((b) => b.name === a.name).length > 1 ? `${a.name} (${a.id})` : a.name);
    const pick = el("select", {},
      el("option", { value: "", text: "None: Microsoft 365 Copilot answers" }),
      ...agents.map((a) => el("option", { value: a.id, text: named(a), title: a.id })),
      chosen && !agents.some((a) => a.id === chosen)
        ? el("option", { value: chosen, text: now.loading ? chosen : `${chosen} (not available now)` }) : null);
    pick.value = chosen;
    pick.addEventListener("change", () => { planText.setValues(s, "agent", pick.value || null); changed(); });
    const hint = now.loading ? "Asking Work IQ for the agents your account has…"
      : now.error ? `Couldn't list your Work IQ agents: ${now.error}`
        : agents.length ? "One of the declarative agents your account has in Microsoft 365 answers instead"
          : "Work IQ lists no agents for your account: Microsoft 365 Copilot answers";
    return field(`${k}-agent`, "Agent", pick, hint);
  }

  function skillsField(s, k) {
    const chosen = planText.valuesOf(s, "skill");
    const chips = chosen.map((name) => el("span", { class: "pb-chip" }, el("span", { class: "mono", text: name }),
      el("button", {
        type: "button", class: "pb-chip-x", "aria-label": `Remove skill ${name}`, title: `Remove skill ${name}`,
        "data-key": `${k}-skill-x-${name}`, onclick: () => { toggle(s, "skill", name)(false); changed(true); },
      }, "×")));
    const filterKey = `${k}-skills`;
    const filter = el("input", { type: "search", placeholder: "Filter skills…", "aria-label": "Filter skills", "data-key": `${k}-skill-filter` });
    filter.value = filters.get(filterKey) ?? "";
    const rows = (state.skills ?? []).map((sk) => {
      const row = checkbox(`${k}-skill-${sk.name}`, [el("span", { class: "mono", text: sk.name }), " ", el("span", { class: "muted small", text: sk.description })],
        chosen.includes(sk.name), (on) => { toggle(s, "skill", sk.name)(on); changed(true); });
      row.dataset.find = `${sk.name} ${sk.description}`.toLowerCase();
      return row;
    });
    const list = el("div", { class: "pb-pick" }, ...rows);
    const apply = () => {
      const q = filter.value.trim().toLowerCase();
      filters.set(filterKey, filter.value);
      for (const r of rows) r.hidden = Boolean(q) && !r.dataset.find.includes(q);
    };
    filter.addEventListener("input", apply);
    apply();
    return group(`${k}-skills`, "Skills", chips, "Instructions the agent follows, e.g. adr writes architecture decision records",
      details(filterKey, chosen.length ? "Add or remove skills" : "Pick skills", [filter, list]));
  }

  function checksField(s, k) {
    const folder = folderOf(s.target);
    const chosen = planText.valuesOf(s, "verify");
    const own = folder?.checks ?? [];
    const builtins = state.builtinChecks ?? [];
    const extra = chosen.filter((c) => !own.includes(c) && !builtins.some((b) => b.name === c));
    const set = (name) => (on) => {
      toggle(s, "verify", name)(on);
      if (!planText.valuesOf(s, "verify").length) planText.setValues(s, "attempts", null);
      changed(true);
    };
    const boxes = [...own, ...extra].map((c) => checkbox(`${k}-check-${c}`, el("span", { class: "mono", text: c }), chosen.includes(c), set(c)));
    const hint = !s.target ? "Choose who does it to see the checks its folder offers."
      : own.length ? "The peer runs them after the agent, and the step passes only if they pass: that is what proves claims."
        : `${folder ? `The ${folder.name} folder` : "This folder"} has no checks of its own (its owner adds them with share add --check NAME=COMMAND).`;
    const builtin = details(`${k}-builtin`, "Built-in checks for spec, design and review packages",
      [el("p", { class: "muted small pb-hint", text: "They check the files the spec and design playbooks write; on other work they fail." }),
        el("div", { class: "pb-checks column" }, ...builtins.map((b) => checkbox(`${k}-check-${b.name}`,
          [el("span", { class: "mono", text: b.name }), " ", el("span", { class: "muted small", text: b.about })], chosen.includes(b.name), set(b.name))))],
      chosen.some((c) => builtins.some((b) => b.name === c)));
    return group(`${k}-checks`, "Checks after the task", boxes, hint, builtin);
  }

  function provesField(s, k, canProve) {
    const chosen = planText.valuesOf(s, "claims");
    if (!plan.claims.length) {
      return group(`${k}-claims`, "Proves", [], "No claims yet: add what must be true above, then pick what this step proves.");
    }
    const boxes = plan.claims.map((c) => checkbox(`${k}-proves-${c.id}`,
      [el("span", { class: "mono", text: c.id }), " ", planText.isTodoClaim(c.text) ? el("em", { class: "muted", text: "not written yet" }) : c.text],
      chosen.includes(c.id), (on) => { toggle(s, "claims", c.id)(on); changed(true); },
      { disabled: !canProve && !chosen.includes(c.id) }));
    const hint = s.approve ? "Approving proves them." : s.team ? "Proven when the team's run proves its own claims." : canProve ? "Proven when its checks pass."
      : "A task proves a claim only with a check: pick one above, or let a sign-off prove it.";
    // A claim this task can't prove can move to a sign-off on it.
    const signoff = !canProve && !s.approve && chosen.length
      ? el("button", { type: "button", class: "btn small", "data-key": `${k}-add-signoff`, onclick: () => addSignoff(s) },
        `Let a sign-off on ${s.id} prove ${chosen.join(", ")}`)
      : null;
    return group(`${k}-claims`, "Proves", boxes, hint, signoff);
  }

  function waitsField(s, k) {
    const circular = planText.dependents(plan, s.id);
    const ids = [...plan.steps.map((o) => o.id).filter((id) => id !== s.id && !circular.has(id)),
      ...s.after.filter((d) => d === s.id || circular.has(d) || !plan.steps.some((o) => o.id === d))];
    const boxes = [...new Set(ids)].map((id) => checkbox(`${k}-after-${id}`, el("span", { class: "mono", text: id }), s.after.includes(id), (on) => {
      s.after = on ? [...s.after, id] : s.after.filter((d) => d !== id);
      if (!on && planText.valuesOf(s, "apply")[0] === id) planText.setValues(s, "apply", null);
      changed(true);
    }));
    const hint = boxes.length ? `It starts once these are done, and gets their results; without any, it starts right away.${circular.size ? " Steps that wait for it aren't listed." : ""}`
      : "No other steps it could wait for.";
    return group(`${k}-after`, "Waits for", boxes, hint);
  }

  /// The knowledge topic a step's result is saved as (save=), for later steps to get.
  function saveField(s, k) {
    const save = el("input", { type: "text", class: "mono", spellcheck: "false", autocomplete: "off", placeholder: "e.g. architecture" });
    save.value = planText.valuesOf(s, "save")[0] ?? "";
    save.addEventListener("change", () => {
      const topic = save.value.trim();
      if (topic && !/^(?!\.)[A-Za-z0-9._-]{1,64}$/.test(topic)) {
        toast("A topic name uses letters, digits, '-', '_' or '.'", "error");
        save.value = planText.valuesOf(s, "save")[0] ?? "";
        return;
      }
      planText.setValues(s, "save", topic || null);
      changed(true);
    });
    return field(`${k}-save`, "Save its result as knowledge", save, "A topic later steps can get");
  }

  function moreOptions(s, k) {
    const folder = folderOf(s.target);
    const items = [];
    let set = 0;
    // How long it may take; then it is cancelled (on every level below it too).
    const limit = el("select", {}, ...[["", "No limit"], ["10m", "10 minutes"], ["30m", "30 minutes"], ["1h", "an hour"],
      ["2h", "2 hours"], ["6h", "6 hours"], ["1d", "a day"]].map(([value, text]) => el("option", { value, text })));
    const timeout = planText.valuesOf(s, "timeout")[0] ?? "";
    if (timeout && ![...limit.options].some((o) => o.value === timeout)) limit.append(el("option", { value: timeout, text: timeout }));
    limit.value = timeout;
    limit.addEventListener("change", () => { planText.setValues(s, "timeout", limit.value || null); changed(); });
    items.push(field(`${k}-timeout`, "Cancel it after", limit, "It fails then, and steps waiting for it are skipped"));
    set += Boolean(timeout);
    // The GitHub issue it works on, when this agent tracks its runs (Settings: Tracker).
    const issue = el("input", { type: "text", inputmode: "numeric", placeholder: "#12", autocomplete: "off", spellcheck: "false" });
    issue.value = planText.valuesOf(s, "issue")[0] ?? "";
    issue.addEventListener("change", () => {
      const n = issue.value.trim().replace(/^#/, "");
      planText.setValues(s, "issue", n ? `#${n}` : null);
      changed();
    });
    items.push(field(`${k}-issue`, "Its issue", issue, "An issue of the tracker's repository it works on; without one, a tracked run gives it a sub-issue"));
    set += Boolean(issue.value);
    if (s.pr) return details(`${k}-more`, `More options${set ? ` (${set} set)` : ""}`, items);
    // Microsoft 365 Copilot answers a @workiq step: no provider, model, folder or tools of its
    // own, but its answer can be kept as knowledge.
    if (!s.team && planText.isWorkiq(s.target)) {
      items.push(saveField(s, k));
      set += planText.valuesOf(s, "save").length > 0;
      return details(`${k}-more`, `More options${set ? ` (${set} set)` : ""}`, items);
    }
    if (s.team) {
      const levels = el("select", {}, ...[1, 2, 3].map((n) => el("option", {
        value: String(n), text: n === 1 ? "Their team does it" : `It may go ${n} levels down (their team's own team steps)`,
      })));
      levels.value = planText.valuesOf(s, "levels")[0] ?? "1";
      levels.addEventListener("change", () => { planText.setValues(s, "levels", levels.value === "1" ? null : levels.value); changed(); });
      items.push(field(`${k}-levels`, "How far down it may go", levels, "Work never goes back up to an agent it came from"));
      set += levels.value !== "1";
    }
    if (planText.valuesOf(s, "verify").length) {
      const tries = el("select", {}, ...[1, 2, 3, 4, 5].map((n) => el("option", { value: String(n), text: n === 1 ? "Once" : `Up to ${n} times, each told what failed` })));
      tries.value = planText.valuesOf(s, "attempts")[0] ?? "1";
      tries.addEventListener("change", () => { planText.setValues(s, "attempts", tries.value === "1" ? null : tries.value); changed(); });
      items.push(field(`${k}-attempts`, "Tries while its checks fail", tries));
      set += tries.value !== "1";
    }
    // A team step's models are the team plan's.
    if (!s.team) {
      const provider = el("select", {}, ...["copilot", "claude", "codex"].map((p) => el("option", { value: p, text: providerLabel(p) })));
      provider.value = s.using ?? "copilot";
      provider.addEventListener("change", () => { s.using = provider.value === "copilot" ? null : provider.value; changed(true); });
      items.push(field(`${k}-using`, "Provider", provider, "Which of the peer's providers runs it"));
      set += Boolean(s.using);
      // The model: its peer's default (selected, named when it says it), or one the peer
      // lets you pick (those it allows, else all its account offers; one by name when it
      // allows any and lists none).
      const menu = modelMenu(stepAgents(s.target, provider.value), provider.value);
      const deflt = plan.model ? null : menu.defaultModel?.toLowerCase();
      const theirs = s.target?.startsWith("#") ? "Each member's default" : "Its default";
      const current = planText.valuesOf(s, "model")[0] ?? "";
      const modelDefault = plan.model ? `${plan.model} (the plan's)`
        : menu.defaultModel ? `${menu.defaultModel} (default)` : theirs;
      const model = el("select", {},
        el("option", { value: "", text: modelDefault }),
        ...modelOptionNodes(modelOptionData(menu.models.filter((m) => m.id.toLowerCase() !== deflt))),
        ...(menu.other ? [el("option", { value: OTHER_MODEL, text: "Another model…" })] : []));
      if (current && ![...model.options].some((o) => o.value === current)) {
        model.append(el("option", { value: current, text: current }));
      }
      model.value = current;
      const name = el("input", {
        type: "text", class: "mono", spellcheck: "false", autocomplete: "off",
        placeholder: "e.g. claude-sonnet-5.5", "aria-label": `Step ${s.id}'s model, by name`,
      });
      name.hidden = true;
      // Its reasoning levels follow the model: drawn again.
      model.addEventListener("change", () => {
        name.hidden = model.value !== OTHER_MODEL;
        if (!name.hidden) {
          name.focus();
          return;
        }
        planText.setValues(s, "model", model.value || null);
        changed(true);
      });
      name.addEventListener("change", () => { planText.setValues(s, "model", name.value.trim() || null); changed(true); });
      const modelField = field(`${k}-model`, "Model", model, menu.other
        ? "Use the default above, or pick another model by name"
        : `The models ${s.target} lets you pick; it refuses others`);
      modelField.append(name);
      items.push(modelField);
      set += Boolean(current);
      // How hard it reasons: its peer's default (selected, as the model takes it), or
      // another level its peer allows that the model supports.
      const pick = effortChoices(menu, current || plan.model);
      const effortNow = planText.valuesOf(s, "effort")[0] ?? "";
      const effortDefault = plan.effort ? `${plan.effort} (the plan's)`
        : pick.fixed ? "None for this model" : pick.fitted ? `${pick.fitted} (default)` : theirs;
      const effort = el("select", { disabled: (pick.fixed && !effortNow) || undefined },
        el("option", { value: "", text: effortDefault }),
        ...pick.levels.filter((e) => plan.effort || e !== pick.fitted).map((e) => el("option", { value: e, text: e })));
      if (effortNow && ![...effort.options].some((o) => o.value === effortNow)) {
        effort.append(el("option", { value: effortNow, text: effortNow }));
      }
      effort.value = effortNow;
      effort.addEventListener("change", () => { planText.setValues(s, "effort", effort.value || null); changed(); });
      items.push(field(`${k}-effort`, "Reasoning", effort, pick.fixed
        ? "This model takes no reasoning level"
        : "The levels its peer allows that the model supports"));
      set += Boolean(effortNow);
    }
    const context = planText.valuesOf(s, "context");
    const topics = [...new Set(["all", ...(state.knowledge ?? []).map((t) => t.name), ...context])];
    if (topics.length > 1 || context.length) {
      items.push(group(`${k}-context`, "Knowledge it gets", topics.map((t) => checkbox(`${k}-context-${t}`,
        el("span", { class: "mono", text: t === "all" ? "all (every topic)" : t }), context.includes(t),
        (on) => { toggle(s, "context", t)(on); changed(true); }))));
      set += context.length > 0;
    }
    // A peer's or squad's step may search your Microsoft 365 for its own words (workiq=). A
    // team step's lead plans its own steps, so it is offered none, but the sources its text
    // has show, to be cleared.
    const sources = planText.valuesOf(s, "workiq");
    if (!s.team || sources.length) {
      const known = planText.WORKIQ_SOURCES.map(([name]) => name);
      const boxes = [...planText.WORKIQ_SOURCES, ...sources.filter((v) => !known.includes(v)).map((v) => [v, null])]
        .map(([name, what]) => checkbox(`${k}-workiq-${name}`,
          el("span", { class: "mono", text: name === "all" ? "all (everything)" : name }), sources.includes(name),
          (on) => { planText.setValues(s, "workiq", planText.toggleSource(planText.valuesOf(s, "workiq"), name, on)); changed(true); },
          { title: what ? `Searches ${what}` : "Not a Microsoft 365 source: uncheck it" }));
      items.push(group(`${k}-workiq`, "Microsoft 365 (Work IQ)", boxes,
        "When it is sent, the step searches your Microsoft 365 for its own words and gets what it finds. A peer gets only what its Microsoft 365 grant allows"));
      set += sources.length > 0;
    }
    items.push(saveField(s, k));
    set += planText.valuesOf(s, "save").length > 0;
    const mcp = planText.valuesOf(s, "mcp");
    const servers = [...new Set([...(folder?.mcp ?? []), ...mcp])];
    if (servers.length && !s.team) {
      items.push(group(`${k}-mcp`, "MCP servers it may use", servers.map((m) => checkbox(`${k}-mcp-${m}`,
        [el("span", { class: "mono", text: m }), folder?.about?.[m] ? el("span", { class: "muted small", text: ` ${folder.about[m]}` }) : null],
        mcp.includes(m), (on) => { toggle(s, "mcp", m)(on); changed(true); }))));
      set += mcp.length > 0;
    }
    const applied = planText.valuesOf(s, "apply")[0] ?? "";
    if (!s.team && ((folder?.worktrees && s.after.length) || applied)) {
      const apply = el("select", {}, el("option", { value: "", text: "Nothing: start from the folder as it is" }),
        ...s.after.map((d) => el("option", { value: d, text: `What ${d} changed (its commit)` })));
      apply.value = applied;
      apply.addEventListener("change", () => { planText.setValues(s, "apply", apply.value || null); changed(); });
      items.push(field(`${k}-apply`, "Start from", apply, "Another peer checks that work independently, in its own worktree"));
      set += Boolean(applied);
    }
    // The work of an earlier run's step, merged or not (base=RUN:STEP).
    const based = planText.valuesOf(s, "base")[0] ?? "";
    if (!s.team && (folder?.worktrees || based)) {
      const base = el("input", {
        type: "text", placeholder: "20261124-031414-851a6:impl…", autocomplete: "off", autocapitalize: "off", spellcheck: "false",
      });
      base.value = based;
      base.addEventListener("change", () => { planText.setValues(s, "base", base.value.trim() || null); changed(); });
      items.push(field(`${k}-base`, "Or start from an earlier run's work", base,
        "An evidence run and its step (RUN:STEP): its commit, merged or not, is applied first. A run's card has Build on it"));
      set += Boolean(based);
    }
    return details(`${k}-more`, `More options${set ? ` (${set} set)` : ""}`, items);
  }

  // ---------- changes ----------
  /// Renames a claim or step from its id field: only to a free, valid name.
  function rename(kind, item, input) {
    const to = input.value.trim();
    const list = kind === "claim" ? plan.claims : plan.steps;
    if (to === item.id) return;
    const why = !/^(?!\.)[A-Za-z0-9._-]{1,64}$/.test(to) || (kind === "step" && ["after", "approve", "team", "pr", "using"].includes(to))
      ? "Use letters, digits, '-', '_' or '.'"
      : list.some((x) => x !== item && x.id === to) ? `There is already a ${kind} called ${to}` : null;
    if (why) {
      toast(why, "error");
      input.value = item.id;
      return;
    }
    (kind === "claim" ? planText.renameClaim : planText.renameStep)(plan, item.id, to);
    changed(true);
  }

  /// What a step is: an agent task, a team task, a sign-off or a pull request.
  const stepKind = (s) => (s.approve ? "signoff" : s.team ? "team" : s.pr ? "pr" : "task");

  function setKind(s, kind) {
    const was = stepKind(s);
    if (was === kind) return;
    s.approve = kind === "signoff";
    s.team = kind === "team";
    s.pr = kind === "pr";
    // Only a pull request step asks for reviews, and only a sign-off for changes.
    if (kind !== "pr") planText.setValues(s, "reviewers", null);
    if (kind !== "signoff") planText.setValues(s, "changes", null);
    if (kind === "pr") {
      // Opened from the work before it: only a time limit, an issue and its reviewers.
      s.target = null;
      s.using = null;
      for (const word of [...s.options]) {
        const option = word.slice(0, word.indexOf("="));
        if (!["timeout", "issue", "reviewers", "reviewer"].includes(option)) planText.setValues(s, option, null);
      }
    } else if (kind === "signoff") {
      // A person's decision: no folder, checks, retries, patch, saved result, tools, agent or search.
      s.target = "@me";
      s.using = null;
      for (const option of ["verify", "attempts", "apply", "save", "mcp", "agent", "levels", "base", "workiq"]) planText.setValues(s, option, null);
    } else if (kind === "team") {
      // The lead's team plan does and proves it: no checks, retries, patch, tools or agent of its
      // own, and the builder offers it no Microsoft 365 search.
      s.target = null;
      s.using = null;
      for (const option of ["verify", "attempts", "apply", "mcp", "agent", "base", "workiq"]) planText.setValues(s, option, null);
    } else {
      s.target = null;
      planText.setValues(s, "levels", null);
    }
    changed(true);
  }

  /// The first of make(from), make(from + 1), ... that isn't taken.
  function freeName(taken, make, from = 1) {
    let n = from;
    while (taken.includes(make(n))) n++;
    return make(n);
  }

  function addClaim() {
    const claim = { id: freeName(plan.claims.map((c) => c.id), (n) => `C-${n}`), text: "", notes: [] };
    plan.claims.push(claim);
    fresh.set(claim, new Set());
    focusNext = `c${plan.claims.length - 1}-text`;
    changed(true);
  }

  function addStep() {
    const options = Object.values(runners()).flat();
    const step = {
      id: freeName(plan.steps.map((s) => s.id), (n) => `step${n}`), after: [], approve: false,
      target: options.length === 1 ? options[0][0] : null, using: null, options: [], todo: "", notes: [],
    };
    plan.steps.push(step);
    fresh.set(step, new Set());
    const i = plan.steps.length - 1;
    focusNext = step.target ? `s${i}-todo` : `s${i}-target`;
    changed(true);
  }

  /// Adds a sign-off: on step `on` (which can't prove its claims, so they move to the
  /// sign-off), or on the last task, proving the claims nothing proves yet.
  function addSignoff(on = null) {
    const ids = plan.steps.map((s) => s.id);
    const last = on ?? plan.steps.filter((s) => !s.approve).at(-1);
    const proven = new Set(plan.steps.flatMap((s) => planText.valuesOf(s, "claims")));
    const claims = on ? planText.valuesOf(on, "claims") : plan.claims.map((c) => c.id).filter((c) => !proven.has(c));
    if (on) planText.setValues(on, "claims", null);
    const signoff = {
      id: ids.includes("review") ? freeName(ids, (n) => `review-${n}`, 2) : "review",
      after: last ? [last.id] : [], approve: true, target: "@me", using: null,
      options: claims.length ? [`claims=${claims.join(",")}`] : [], todo: "", notes: [],
    };
    const at = on ? plan.steps.indexOf(on) + 1 : plan.steps.length;
    plan.steps.splice(at, 0, signoff);
    fresh.set(signoff, new Set());
    focusNext = `s${at}-todo`;
    changed(true);
  }

  /// After an edit: writes the plan text (the editor's check and Run use it) and, for
  /// a change of structure, draws the builder again.
  function changed(again = false) {
    const out = planText.formatPlan(plan);
    lines = out.items;
    const text = editor();
    text.value = out.text;
    text.dispatchEvent(new Event("input", { bubbles: true }));
    if (again) render();
    else marks();
  }

  // ---------- drawing ----------
  function render() {
    if (!plan) return;
    const was = document.activeElement;
    const key = root().contains(was) ? was.dataset.key : null;
    const sel = key && typeof was.selectionStart === "number" ? [was.selectionStart, was.selectionEnd] : null;
    const notes = plan.notes.length ? el("section", { class: "pb-section" },
      field("notes", "Notes", Object.assign(el("textarea", { rows: Math.min(plan.notes.length, 4), spellcheck: "true" }), { value: plan.notes.join("\n") }),
        "Comments at the top of the plan; agents don't see them")) : null;
    notes?.querySelector("textarea").addEventListener("input", (e) => { plan.notes = e.target.value.split("\n"); changed(); });
    const empty = !plan.claims.length && !plan.steps.length;
    root().replaceChildren(...[
      empty ? el("p", { class: "pb-intro", text: "Start with a step: what an agent should do, and where. Then add what must be true, and a sign-off or checks that prove it." }) : null,
      notes,
      el("section", { class: "pb-section" },
        el("h3", { class: "pb-h", text: "What must be true" }),
        el("p", { class: "muted small", text: "Claims the run proves. Only a check a peer runs, or a sign-off, proves a claim." }),
        plan.claims.length ? el("ul", { class: "pb-claims" }, ...plan.claims.map(claimRow)) : null,
        el("button", { type: "button", class: "btn small", "data-key": "add-claim", onclick: addClaim }, "+ Add a claim")),
      el("section", { class: "pb-section" },
        el("h3", { class: "pb-h", text: "Steps" }),
        el("p", { class: "muted small", text: "Each step is a task an agent does, or a sign-off by a person. Steps run at the same time unless one waits for another." }),
        plan.steps.length ? el("ol", { class: "pb-steps" }, ...plan.steps.map(stepCard)) : null,
        el("div", { class: "pb-add" },
          el("button", { type: "button", class: "btn small", "data-key": "add-step", onclick: () => addStep() }, "+ Add a step"),
          el("button", { type: "button", class: "btn small", "data-key": "add-signoff", onclick: () => addSignoff() }, "+ Add a sign-off"))),
    ].filter(Boolean));
    marks();
    const target = focusNext ?? key;
    focusNext = null;
    const node = target && root().querySelector(`[data-key="${CSS.escape(target)}"]`);
    if (node) {
      node.focus();
      if (sel && target === key && typeof node.setSelectionRange === "function") {
        try { node.setSelectionRange(...sel); } catch { /* not a text field */ }
      }
    }
  }

  /// Which card a plan check problem is about: its step or claim, or the line it names.
  function cardOf(p) {
    const step = p.step != null ? plan.steps.findIndex((s) => s.id === p.step) : -1;
    if (step >= 0) return cardKey("step", step);
    const claim = p.claim != null ? plan.claims.findIndex((c) => c.id === p.claim) : -1;
    if (claim >= 0) return cardKey("claim", claim);
    const line = /^line (\d+): /.exec(p.message);
    const item = line && lines[Number(line[1]) - 1];
    if (item?.step) return cardKey("step", plan.steps.findIndex((s) => s.id === item.step));
    if (item?.claim) return cardKey("claim", plan.claims.findIndex((c) => c.id === item.claim));
    return null;
  }

  /// The plan check's problems, one per line of a parse error.
  const problemsOf = (res) => (res?.problems ?? []).flatMap((p) =>
    /^line \d+: /.test(p.message) ? p.message.split("\n").map((message) => ({ ...p, message })) : [p]);

  /// The fields where the builder says what the plan check's coded problems say.
  const SAID_ON = { "todo-task": "todo", "todo-claim": "text", "unchecked-claim": "claims" };

  const itemOf = (kind, index) => (kind === "claim" ? plan.claims : plan.steps)[index];

  /// Shows what each card still needs (by field) and the plan check's problems on the
  /// card they are about; the rest, and how ready the plan is, below the builder.
  function marks() {
    if (!plan || !active()) return;
    const needs = planText.needs(plan);
    const byField = new Map(needs.map((n) => [`${cardKey(n.kind, n.index)}-${n.field}`, n]));
    for (const p of root().querySelectorAll("[data-need]")) {
      const n = byField.get(p.dataset.need);
      const been = n && fresh.get(itemOf(n.kind, n.index));
      const missing = n && ["target", "todo", "text"].includes(n.field);
      const show = Boolean(n && (!missing || showAll || !been || been.has(n.field)));
      p.hidden = !show;
      p.textContent = show ? n.message : "";
      p.classList.toggle("bad", show && showAll);
      root().querySelector(`[data-key="${CSS.escape(p.dataset.need)}"]`)?.toggleAttribute("aria-invalid", show);
    }
    const incomplete = new Set(needs.map((n) => cardKey(n.kind, n.index)));
    for (const b of root().querySelectorAll("[data-badge]")) b.hidden = !incomplete.has(b.dataset.badge);
    // The check's problems, on their cards; not those a card already says on a field,
    // nor parse errors about what a card still needs.
    const rest = [];
    const byCard = new Map();
    for (const p of problemsOf(check)) {
      const card = cardOf(p);
      if (!card) rest.push(p);
      else if (byField.has(`${card}-${SAID_ON[p.code]}`)) continue;
      else if (!(incomplete.has(card) && /^line \d+: /.test(p.message))) byCard.set(card, [...(byCard.get(card) ?? []), p]);
    }
    for (const box of root().querySelectorAll("[data-marks]")) {
      box.replaceChildren(...(byCard.get(box.dataset.marks) ?? []).map(problemLine));
    }
    const summary = $("#plan-editor-check");
    if (!editor().value.trim()) {
      summary.hidden = true;
      return;
    }
    summary.hidden = false;
    const cards = incomplete.size;
    const errors = problemsOf(check).filter((p) => p.error).length;
    const head = cards ? el("p", { class: "warn", text: `Not ready yet: ${nOf(cards, "card")} still ${cards === 1 ? "needs" : "need"} something.` })
      : !check ? el("p", { class: "muted", text: "Checking the plan…" })
        : !check.problems.length ? el("p", { class: "ok", text: `✓ Ready to run: ${nOf(check.steps, "step")}, ${nOf(check.claims, "claim")}` })
          : el("p", { class: errors ? "bad" : "warn", text: errors ? `Can't run yet: ${nOf(errors, "problem")}, marked where they are.` : "Ready to run, with warnings (marked where they are)." });
    summary.replaceChildren(...[head, rest.length ? el("ul", {}, ...rest.map((p) => el("li", { class: p.error ? "bad" : "warn" }, problemLine(p)))) : null].filter(Boolean));
  }

  function problemLine(p) {
    return el("p", { class: `pb-problem ${p.error ? "bad" : "warn"}` },
      el("strong", { text: p.error ? "Error: " : "Warning: " }), p.message.replace(/^line \d+: /, ""),
      p.fixes?.length ? el("span", { class: "fixes" }, ...p.fixes.map((f) => fixButton(f, editor(), $("#plan-editor-check")))) : null);
  }

  // ---------- the editor's views ----------
  function setView(view, { focus = true } = {}) {
    const body = $("#plan-editor-body");
    body.dataset.view = view;
    // Describe writes a plan: the editor opens again on the view plans are edited in.
    if (view !== "describe") localStorage.setItem(VIEW_KEY, view);
    for (const b of document.querySelectorAll("#plan-view [data-view]")) {
      b.setAttribute("aria-checked", String(b.dataset.view === view));
      b.tabIndex = b.dataset.view === view ? 0 : -1;
    }
    if (view === "describe") {
      $("#plan-editor-tip").textContent = "Say what the plan should do · your profile's model writes it · you review and run it";
      showPlanDescribe({ focus });
      return;
    }
    $("#plan-editor-tip").textContent = view === "builder"
      ? "Fill in the cards · the plan is checked as you go · Ctrl+Enter runs it"
      : "Plan settings, then claims and one step per line · Ctrl+Enter runs the plan";
    if (view === "builder") {
      load(editor().value);
      if (focus) {
        // The first thing still needed, else the first field.
        const n = planText.needs(plan)[0];
        const first = n && root().querySelector(`[data-key="${CSS.escape(`${cardKey(n.kind, n.index)}-${n.field}`)}"]`);
        (first ?? root().querySelector("input, select, textarea, button"))?.focus();
      }
    } else {
      renderPalette();
      if (focus) editor().focus();
    }
    checkPlan(editor().value, $("#plan-editor-check"), editor()).catch(() => {});
  }

  /// Draws the builder for plan `text` (when the editor opens, after the Text view or
  /// a fix).
  function load(text) {
    plan = planText.parsePlan(text);
    lines = sourceLines(text);
    render();
  }

  /// What each line of `text` is, by the order of the claims and steps parsed from it.
  function sourceLines(text) {
    let claim = 0;
    let step = 0;
    let last = null;
    return planText.lineKinds(text).map(({ kind }) => {
      if (kind === "text") return last;
      // An about: line's text is no card's.
      if (kind === "about") last = null;
      if (kind !== "claim" && kind !== "step") return null;
      return (last = kind === "claim" ? { claim: plan.claims[claim++]?.id } : { step: plan.steps[step++]?.id });
    });
  }

  /// Whether the plan can be run or scheduled as far as the builder can tell; if not,
  /// points out everything missing and goes to the first.
  function ready() {
    if (!active() || !plan) return true;
    const needs = planText.needs(plan);
    if (!needs.length) return true;
    showAll = true;
    marks();
    const first = needs[0];
    root().querySelector(`[data-key="${CSS.escape(`${cardKey(first.kind, first.index)}-${first.field}`)}"]`)?.focus();
    toast(`Fill in what's marked first: ${nOf(needs.length, "thing")}`, "error");
    return false;
  }

  /// The plan check's result, for the cards.
  function checked(res) {
    check = res;
    marks();
  }

  function reset() {
    fresh = new WeakMap();
    showAll = false;
    check = null;
    workiqAgents = null;
  }

  function init() {
    radioGroup($("#plan-view"), (btn) => setView(btn.dataset.view));
    // Leaving a field of a card added here: what it still needs shows from now on.
    root().addEventListener("focusout", (e) => {
      const m = /^([cs])(\d+)-(\w+)$/.exec(e.target.dataset?.key ?? "");
      const been = m && fresh.get(itemOf(m[1] === "c" ? "claim" : "step", Number(m[2])));
      if (been && !been.has(m[3])) {
        been.add(m[3]);
        marks();
      }
    });
    root().addEventListener("keydown", (e) => {
      if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
        e.preventDefault();
        $("#plan-editor-run").click();
      }
    });
  }

  return {
    init, load, ready, checked, reset, active,
    view: () => (localStorage.getItem(VIEW_KEY) === "text" ? "text" : "builder"),
    setView,
  };
})();

planBuilder.init();
