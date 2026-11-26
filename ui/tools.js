// Proofkin web UI: Plugins & tools. Import plugins (reviewed before anything is
// kept; importing runs nothing), offer MCP servers in shared folders, and add the
// profile's own skills and agents. Uses app.js helpers: $, el, api, toast,
// confirmDialog, radioGroup, openSkill, loadSkills, ACCESS_SHORT. Plugin and server
// text is rendered with textContent only.
"use strict";

const tools = {
  tab: "plugins",
  plugins: [],
  mcp: { shares: [], templates: [] },
  skills: [],
  agents: [],
  /** What was fetched for review: a plugin, or a marketplace to pick from. */
  review: null,
  /** The offered server being edited ("share/name"): lists wait until it's saved. */
  editing: null,
  /** Plugins whose contents are expanded. */
  open: new Set(),
  built: false,
  /** The offer form's fields. */
  f: null,
  from: "plugin",
  parsed: null,
};

const ACCESS_RANK = { "read-only": 0, "workspace-write": 1, full: 2 };
const plural = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;
const splitList = (text) => text.split(/[,\s]+/).map((t) => t.trim()).filter(Boolean);
const accessText = (a) => (ACCESS_SHORT[a] ?? a).toLowerCase();

let fieldSeq = 0;
/** A form field: its label names the control, its hint (if any) describes it. */
function field(label, control, hint) {
  if (!control.id) control.id = `tf-${++fieldSeq}`;
  const help = hint ? el("span", { class: "muted small hint-text", id: `${control.id}-hint`, text: hint }) : null;
  if (help) control.setAttribute("aria-describedby", help.id);
  return el("div", { class: "field" }, el("label", { for: control.id, text: label }), control, help);
}

async function loadTools() {
  const [plugins, mcp, skills] = await Promise.all([api("/api/plugins"), api("/api/mcp"), api("/api/skills")]);
  tools.plugins = plugins.plugins;
  tools.mcp = mcp;
  tools.skills = skills.skills;
  tools.agents = skills.agents ?? [];
  const offered = mcp.shares.reduce((n, s) => n + s.mcp.length, 0);
  $("#tools-summary").textContent =
    `${plural(tools.plugins.length, "plugin")} · ${plural(offered, "MCP server")} offered · ${plural(tools.skills.length, "skill")} · ${plural(tools.agents.length, "agent")}`;
  if ($("#tools-dialog").open) renderTools();
}

function openTools(tab) {
  buildTools();
  selectTab(tab ?? tools.tab);
  renderTools();
  $("#tools-dialog").showModal();
  $(`#tab-${tools.tab}`).focus();
  loadTools().catch((err) => toast(err.message, "error"));
}

function selectTab(name, focus = false) {
  tools.tab = name;
  for (const tab of document.querySelectorAll("#tools-dialog [role=tab]")) {
    const on = tab.dataset.tab === name;
    tab.setAttribute("aria-selected", String(on));
    tab.tabIndex = on ? 0 : -1;
    $(`#${tab.getAttribute("aria-controls")}`).hidden = !on;
    if (on && focus) tab.focus();
  }
}

function buildTools() {
  if (tools.built) return;
  tools.built = true;
  buildPluginsPanel();
  buildMcpPanel();
  buildSkillsPanel();
  buildAgentsPanel();
}

function renderTools() {
  renderPluginList();
  renderOffered();
  refreshOfferForm();
  renderSkillList();
  renderAgentList();
}

// ---------- plugins ----------
function buildPluginsPanel() {
  const source = el("input", {
    id: "plugin-source", type: "text", spellcheck: "false", autocomplete: "off",
    placeholder: "e.g. octo-org/agent-plugins:plugins/reviewer…",
  });
  const fetchBtn = el("button", { type: "submit", class: "btn primary" }, "Fetch for review");
  const form = el("form", {
    class: "tool-form", novalidate: true,
    onsubmit: (e) => {
      e.preventDefault();
      fetchPlugin({ source: source.value.trim() }, fetchBtn);
    },
  },
  field("Where is the plugin?", source,
    "A folder on this machine, a GitHub repository (owner/repo, or owner/repo:path/to/plugin) or a git URL. GitHub Copilot CLI, Claude Code and Agent Plugins layouts and marketplaces work; private GitHub repositories use this profile's GitHub sign-in."),
  el("p", { class: "form-error", id: "plugin-error", role: "alert" }),
  el("div", { class: "form-actions" }, fetchBtn));
  $("#panel-plugins").replaceChildren(
    el("section", { class: "tool-section" },
      el("h3", { text: "Import a plugin" }),
      el("p", {
        class: "muted small",
        text: "A plugin packages skills, agents and MCP servers. You see what it contains before anything is kept, and importing runs nothing: its skills and agents become usable in plan steps, and its MCP servers wait until you offer one in a folder.",
      }),
      form,
      el("div", { id: "plugin-review" })),
    el("section", { class: "tool-section" },
      el("h3", { id: "plugin-list-h", text: "Imported plugins" }),
      el("ul", { class: "tool-list", id: "plugin-list", "aria-labelledby": "plugin-list-h" })));
}

async function fetchPlugin(body, button) {
  const error = $("#plugin-error");
  error.textContent = "";
  if (!body.source && !body.token) {
    error.textContent = "Say where the plugin is.";
    $("#plugin-source").focus();
    return;
  }
  const label = button.textContent;
  button.disabled = true;
  button.textContent = "Fetching for review…";
  try {
    tools.review = await api("/api/plugin-preview", { method: "POST", body: JSON.stringify(body) });
    renderReview();
    $("#plugin-review").querySelector("button, input")?.focus();
  } catch (err) {
    error.textContent = err.message;
    if (!body.token) $("#plugin-source").focus();
  } finally {
    button.disabled = false;
    button.textContent = label;
  }
}

function discardReview() {
  tools.review = null;
  renderReview();
  $("#plugin-source").focus();
}

function renderReview() {
  const box = $("#plugin-review");
  const r = tools.review;
  if (!r) {
    box.replaceChildren();
    return;
  }
  if (r.kind === "marketplace") {
    box.replaceChildren(el("div", { class: "review-card", role: "region", "aria-label": "Marketplace" },
      el("div", { class: "task-top" },
        el("strong", { text: r.name ? `Marketplace ${r.name}` : "Marketplace" }),
        el("span", { class: "badge neutral", text: plural(r.plugins.length, "plugin") })),
      el("p", { class: "muted small", text: "Pick a plugin to see what it contains." }),
      el("ul", { class: "tool-list" }, ...r.plugins.map((p) => el("li", { class: "tool-item" },
        el("div", { class: "task-top" },
          el("span", { class: "task-title", text: p.name }),
          p.version ? el("span", { class: "badge neutral", text: p.version }) : null),
        p.description ? el("div", { class: "task-meta", text: p.description }) : null,
        el("div", { class: "task-actions" }, p.source.kind === "unsupported"
          ? el("span", { class: "muted small", text: `Can't be imported: ${p.source.value}` })
          : el("button", {
            type: "button", class: "btn small",
            onclick: (e) => fetchPlugin({ token: r.token, plugin: p.name }, e.currentTarget),
          }, `Review ${p.name}`))))),
      el("div", { class: "dialog-actions" }, el("button", { type: "button", class: "btn", onclick: discardReview }, "Close marketplace"))));
    return;
  }
  const p = r.plugin;
  const rename = el("input", { type: "text", id: "plugin-rename", value: p.name, maxlength: "64", spellcheck: "false", autocomplete: "off" });
  const importBtn = el("button", { type: "button", class: "btn primary" }, "Import plugin");
  importBtn.addEventListener("click", () => installPlugin(rename.value.trim(), importBtn));
  box.replaceChildren(el("div", { class: "review-card", role: "region", "aria-label": `Review of plugin ${p.name}` },
    el("div", { class: "task-top" },
      el("strong", { text: p.name }),
      p.version ? el("span", { class: "badge neutral", text: p.version }) : null,
      el("span", { class: "badge", text: p.format })),
    p.description ? el("p", { class: "review-desc", text: p.description }) : null,
    r.market ? el("p", { class: "muted small", text: `From the marketplace ${r.market}` }) : null,
    pluginContents(p, false),
    field("Import as", rename, r.exists
      ? `A plugin named ${p.name} is imported already: pick another name, or update that one instead.`
      : "The name its skills, agents and servers are known by here."),
    el("p", { class: "form-error", id: "plugin-import-error", role: "alert" }),
    el("div", { class: "dialog-actions" },
      el("button", { type: "button", class: "btn", onclick: discardReview }, "Discard"),
      importBtn)));
}

/** What a plugin contains: skills, agents, MCP servers (offerable once imported) and notes. */
function pluginContents(p, installed) {
  const parts = [el("h4", { text: `Skills (${p.skills.length})` })];
  parts.push(p.skills.length
    ? el("ul", { class: "plain-list" }, ...p.skills.map((s) => el("li", {},
      el("span", { class: "mono", text: installed ? `skill=${s.use}` : s.name }),
      s.description ? el("span", { class: "muted", text: ` · ${s.description}` }) : null)))
    : el("p", { class: "muted small", text: "None." }));
  const agents = p.agents ?? [];
  parts.push(el("h4", { text: `Agents (${agents.length})` }));
  parts.push(agents.length
    ? el("ul", { class: "plain-list" }, ...agents.map((a) => el("li", {},
      el("span", { class: "mono", text: installed ? `agent=${a.use}` : a.name }),
      a.description ? el("span", { class: "muted", text: ` · ${a.description}` }) : null,
      agentUses(a))))
    : el("p", { class: "muted small", text: "None." }));
  parts.push(el("h4", { text: `MCP servers (${p.mcp.length})` }));
  if (p.mcp.length) {
    parts.push(el("ul", { class: "plain-list" }, ...p.mcp.map((m) => el("li", {},
      el("div", {}, el("strong", { text: m.name }), " ", el("code", { class: "mono", text: m.describe })),
      m.inputs.length ? el("div", {
        class: "muted small",
        text: `Needs: ${m.inputs.map((i) => `${i.name}${i.secret ? " (secret)" : ""}${i.default ? ` [default ${i.default}]` : ""}`).join(", ")}`,
      }) : null,
      installed && m.offered.length ? el("div", {
        class: "muted small",
        text: `Offered in ${m.offered.map((o) => `${o.share} as ${o.name}${o.paused ? " (paused)" : ""}`).join(", ")}`,
      }) : null,
      installed && p.enabled ? el("button", { type: "button", class: "btn small", onclick: () => offerTemplate(p.name, m.name) }, "Offer in a folder…") : null))));
    if (!installed) parts.push(el("p", { class: "muted small", text: "Nothing starts when you import: you offer a server in a folder later, with the values it needs." }));
  } else {
    parts.push(el("p", { class: "muted small", text: "None." }));
  }
  for (const note of p.notes) parts.push(el("p", { class: "note", text: note }));
  return el("div", { class: "plugin-contents" }, ...parts);
}

async function installPlugin(rename, button) {
  const error = $("#plugin-import-error");
  error.textContent = "";
  if (!rename) {
    error.textContent = "Give the plugin a name.";
    $("#plugin-rename").focus();
    return;
  }
  button.disabled = true;
  button.textContent = "Importing…";
  try {
    const res = await api("/api/plugins", { method: "POST", body: JSON.stringify({ token: tools.review.token, rename }) });
    tools.review = null;
    renderReview();
    $("#plugin-source").value = "";
    tools.open.add(res.plugin.name);
    toast(`Imported plugin ${res.plugin.name}. Nothing in it ran.`);
    await loadTools();
  } catch (err) {
    error.textContent = err.message;
    button.disabled = false;
    button.textContent = "Import plugin";
  }
}

function renderPluginList() {
  const list = $("#plugin-list");
  if (!list) return;
  $("#plugin-list-h").textContent = `Imported plugins (${tools.plugins.length})`;
  if (!tools.plugins.length) {
    list.replaceChildren(el("li", { class: "empty", text: "No plugins yet. Import one above." }));
    return;
  }
  list.replaceChildren(...tools.plugins.map((p) => {
    const toggle = el("input", { type: "checkbox", role: "switch", checked: p.enabled, "aria-label": `Plugin ${p.name} is on` });
    toggle.addEventListener("change", () => togglePlugin(p, toggle));
    const details = el("details", { open: tools.open.has(p.name) }, el("summary", { text: "What it contains" }), pluginContents(p, true));
    details.addEventListener("toggle", () => (details.open ? tools.open.add(p.name) : tools.open.delete(p.name)));
    return el("li", { class: "tool-item" },
      el("div", { class: "task-top" },
        el("span", { class: "task-title", text: p.name }),
        p.version ? el("span", { class: "badge neutral", text: p.version }) : null,
        el("label", { class: "switch" }, toggle, el("span", { text: p.enabled ? "On" : "Off" }))),
      p.description ? el("div", { class: "task-meta", text: p.description }) : null,
      el("div", {
        class: "task-meta",
        text: `${p.format} · ${plural(p.skills.length, "skill")} · ${plural(p.mcp.length, "MCP server")} · from ${p.source}${p.commit ? ` at ${p.commit.slice(0, 7)}` : ""}`,
      }),
      details,
      el("div", { class: "task-actions" },
        el("button", { type: "button", class: "btn small", title: "Fetch it again from where it came from", onclick: (e) => updatePlugin(p, e.currentTarget) }, "Update"),
        el("button", { type: "button", class: "btn small danger", onclick: () => removePlugin(p) }, "Remove")));
  }));
}

async function togglePlugin(p, toggle) {
  toggle.disabled = true;
  try {
    await api(`/api/plugins/${encodeURIComponent(p.name)}`, { method: "PATCH", body: JSON.stringify({ enabled: toggle.checked }) });
    toast(toggle.checked ? `${p.name} is on` : `${p.name} is off: its skills, agents and MCP servers no longer apply`);
    await loadTools();
  } catch (err) {
    toggle.checked = !toggle.checked;
    toast(err.message, "error");
  } finally {
    toggle.disabled = false;
  }
}

async function updatePlugin(p, button) {
  button.disabled = true;
  button.textContent = "Updating…";
  try {
    const res = await api(`/api/plugins/${encodeURIComponent(p.name)}/update`, { method: "POST" });
    toast(`Updated ${p.name}: ${res.changes.join("; ")}`);
    await loadTools();
  } catch (err) {
    toast(err.message, "error");
    button.disabled = false;
    button.textContent = "Update";
  }
}

async function removePlugin(p) {
  const offered = p.mcp.flatMap((m) => m.offered.map((o) => `${o.share}/${o.name}`));
  const ok = await confirmDialog({
    title: `Remove plugin ${p.name}?`,
    body: `Its files and skills are deleted${offered.length ? `, and your folders stop offering its MCP servers (${offered.join(", ")})` : ""}. You can import it again later.`,
    confirm: "Remove plugin",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/plugins/${encodeURIComponent(p.name)}`, { method: "DELETE" });
    tools.open.delete(p.name);
    toast(`Removed plugin ${p.name}`);
    await loadTools();
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- MCP servers ----------
function buildMcpPanel() {
  $("#panel-mcp").replaceChildren(
    el("section", { class: "tool-section" },
      el("h3", { text: "Offered in your folders" }),
      el("p", {
        class: "muted small",
        text: "Peers' tasks in a folder may use the MCP servers it offers; they ask for one with mcp=NAME. Servers run on this machine with your rights. Peers learn only names and descriptions, never commands, keys or URLs, and tool results reach the model marked as untrusted content.",
      }),
      el("div", { id: "mcp-offered" })),
    el("section", { class: "tool-section", id: "offer-section" },
      el("h3", { text: "Offer a server" }),
      buildOfferForm()));
}

function accessSelect(value = "full") {
  const select = el("select", {},
    el("option", { value: "full", text: "Full: only tasks with full access (safest)" }),
    el("option", { value: "workspace-write", text: "Edit files: tasks that may edit files, or more" }),
    el("option", { value: "read-only", text: "Read only: any task in the folder" }));
  select.value = value;
  return select;
}

function renderOffered() {
  const box = $("#mcp-offered");
  if (!box || tools.editing) return;
  const shares = tools.mcp.shares;
  if (!shares.length) {
    box.replaceChildren(el("p", { class: "empty" }, "No shared folders yet. Share one with ",
      el("span", { class: "mono", text: "proofkin share add NAME PATH" }), " or link a GitHub repository, then offer servers in it."));
    return;
  }
  box.replaceChildren(...shares.map((sh) => el("div", { class: "share-block" },
    el("h4", {}, el("span", { class: "mono", text: sh.name }), " ",
      el("span", { class: "badge neutral", text: `peers' tasks: at most ${accessText(sh.max_access)}` })),
    el("div", { class: "muted small mono path", text: sh.path }),
    sh.mcp.length
      ? el("ul", { class: "tool-list" }, ...sh.mcp.map((s) => offeredItem(sh, s)))
      : el("p", { class: "muted small", text: "Offers no MCP servers." }))));
}

function offeredItem(sh, s) {
  const status = s.plugin_off ? ["skipped", "Plugin off"] : s.paused ? ["skipped", "Paused"] : ["done", "Offered"];
  const unusable = ACCESS_RANK[s.access] > ACCESS_RANK[sh.max_access];
  const path = `/api/mcp/${encodeURIComponent(sh.name)}/${encodeURIComponent(s.name)}`;
  const item = el("li", { class: "tool-item" });
  item.append(...[
    el("div", { class: "task-top" },
      el("span", { class: "task-title mono", text: s.name }),
      el("span", { class: `status ${status[0]}`, text: status[1] })),
    s.description ? el("div", { class: "task-meta", text: s.description }) : null,
    el("div", { class: "task-meta" }, el("code", { class: "mono", text: s.describe })),
    el("div", {
      class: "task-meta",
      text: `Needs ${accessText(s.access)} access · tools: ${s.tools.length ? s.tools.join(", ") : "all"}${s.plugin ? ` · from plugin ${s.plugin.plugin}` : ""}`,
    }),
    unusable ? el("div", {
      class: "task-meta warn-text",
      text: `No task can use it: this folder gives peers' tasks at most ${accessText(sh.max_access)} access. Lower the access it needs.`,
    }) : null,
    el("div", { class: "task-actions" },
      s.plugin_off ? el("span", { class: "muted small", text: `Turn plugin ${s.plugin?.plugin} on to offer it.` }) : el("button", {
        type: "button", class: "btn small",
        onclick: (e) => patchServer(path, { paused: !s.paused }, e.currentTarget, s.paused ? `${s.name} is offered again` : `${s.name} is paused`),
      }, s.paused ? "Resume" : "Pause"),
      el("button", { type: "button", class: "btn small", onclick: () => editServer(item, sh, s, path) }, "Edit"),
      el("button", { type: "button", class: "btn small danger", onclick: () => removeServer(sh, s, path) }, "Remove")),
  ].filter(Boolean));
  return item;
}

async function patchServer(path, body, button, done) {
  button.disabled = true;
  try {
    await api(path, { method: "PATCH", body: JSON.stringify(body) });
    toast(done);
    await loadTools();
  } catch (err) {
    toast(err.message, "error");
    button.disabled = false;
  }
}

function templateOf(s) {
  if (!s.plugin) return null;
  return tools.plugins.find((p) => p.name === s.plugin.plugin)?.mcp.find((m) => m.name === s.plugin.server) ?? null;
}

function editServer(item, sh, s, path) {
  tools.editing = `${sh.name}/${s.name}`;
  const access = accessSelect(s.access);
  const toolList = el("input", { type: "text", value: s.tools.join(", "), spellcheck: "false", autocomplete: "off", placeholder: "all of its tools…" });
  const description = el("input", { type: "text", value: s.description, maxlength: "200", autocomplete: "off" });
  const inputs = templateOf(s)?.inputs ?? [];
  const values = (s.plugin?.values ?? []).map((name) => {
    const secret = inputs.find((i) => i.name === name)?.secret ?? true;
    return [name, el("input", { type: secret ? "password" : "text", autocomplete: "off", spellcheck: "false", placeholder: "unchanged…" })];
  });
  const error = el("p", { class: "form-error", role: "alert" });
  const save = el("button", { type: "submit", class: "btn primary small" }, "Save changes");
  const cancel = () => {
    tools.editing = null;
    renderOffered();
  };
  const form = el("form", {
    class: "tool-form inline-edit", novalidate: true,
    onsubmit: async (e) => {
      e.preventDefault();
      const body = { access: access.value, tools: splitList(toolList.value), description: description.value };
      if (values.length) body.values = Object.fromEntries(values.map(([k, input]) => [k, input.value]));
      save.disabled = true;
      save.textContent = "Saving…";
      try {
        await api(path, { method: "PATCH", body: JSON.stringify(body) });
        tools.editing = null;
        toast(`Saved ${s.name}`);
        await loadTools();
      } catch (err) {
        error.textContent = err.message;
        save.disabled = false;
        save.textContent = "Save changes";
      }
    },
  },
  field("Least access a peer's task needs to use it", access),
  field("Tools tasks may call", toolList, "Separate with commas; blank allows all of its tools."),
  field("What it is for (peers see this)", description),
  ...values.map(([name, input]) => field(name, input, "Leave blank to keep the current value.")),
  error,
  el("div", { class: "dialog-actions" },
    el("button", { type: "button", class: "btn small", onclick: cancel }, "Cancel"),
    save));
  item.replaceChildren(
    el("div", { class: "task-top" },
      el("span", { class: "task-title mono", text: s.name }),
      el("span", { class: "muted small", text: `Editing, in ${sh.name}` })),
    form);
  access.focus();
}

async function removeServer(sh, s, path) {
  const ok = await confirmDialog({
    title: `Stop offering ${s.name} in ${sh.name}?`,
    body: `Peers' tasks in ${sh.name} can no longer use it.${s.plugin ? ` The plugin ${s.plugin.plugin} stays imported, so you can offer it again.` : " To offer it again, add it again."}`,
    confirm: "Stop offering",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(path, { method: "DELETE" });
    toast(`${sh.name} no longer offers ${s.name}`);
    await loadTools();
  } catch (err) {
    toast(err.message, "error");
  }
}

/** Fields for the values a server template needs (secrets typed hidden). */
function inputFields(box, inputs) {
  const sentence = (text) => (text && !/[.!?]$/.test(text) ? `${text}.` : text);
  box.replaceChildren(...inputs.map((i) => field(i.name,
    el("input", {
      type: i.secret ? "password" : "text", "data-input": i.name, autocomplete: "off", spellcheck: "false",
      placeholder: i.default ? `default: ${i.default}…` : "",
    }),
    [sentence(i.description), i.secret ? "A secret: it stays on this machine and is never shown again." : null]
      .filter(Boolean).join(" ") || null)));
}

function inputValues(box) {
  return Object.fromEntries([...box.querySelectorAll("[data-input]")].map((i) => [i.dataset.input, i.value]));
}

function buildOfferForm() {
  const f = {};
  tools.f = f;
  f.share = el("select", { id: "offer-share" });
  f.name = el("input", { type: "text", id: "offer-name", maxlength: "64", spellcheck: "false", autocomplete: "off", placeholder: "e.g. browser…" });
  f.from = el("div", { class: "segmented", role: "radiogroup", "aria-label": "Where the server comes from" },
    ...[["plugin", "From a plugin"], ["config", "From an MCP configuration"], ["hand", "By hand"]].map(([value, label]) =>
      el("button", { type: "button", role: "radio", "data-from": value, "aria-checked": String(value === tools.from), tabindex: value === tools.from ? "0" : "-1" }, label)));
  radioGroup(f.from, (btn) => {
    tools.from = btn.dataset.from;
    tools.fromChosen = true;
    syncOfferFrom();
  });

  f.template = el("select", { id: "offer-template" });
  f.template.addEventListener("change", onTemplateChange);
  f.pluginInputs = el("div", { class: "offer-inputs" });
  f.pluginBox = el("div", { class: "offer-from" },
    field("Server", f.template),
    el("code", { class: "mono template-desc", id: "offer-template-desc" }),
    f.pluginInputs);

  f.config = el("textarea", { rows: "5", class: "mono", spellcheck: "false", placeholder: '{"mcpServers": {"browser": {"command": "npx", "args": ["@playwright/mcp@latest"]}}}' });
  f.file = el("input", { type: "file", accept: ".json,application/json" });
  f.file.addEventListener("change", async () => {
    const file = f.file.files?.[0];
    if (!file) return;
    f.config.value = await file.text();
    readConfig();
  });
  f.readBtn = el("button", { type: "button", class: "btn small", onclick: readConfig }, "Read configuration");
  f.configServer = el("select", {});
  f.configServer.addEventListener("change", onConfigServerChange);
  f.configInputs = el("div", { class: "offer-inputs" });
  f.configNotes = el("div", {});
  f.configPicked = el("div", { hidden: true },
    field("Server", f.configServer),
    el("code", { class: "mono template-desc", id: "offer-config-desc" }),
    f.configNotes,
    f.configInputs);
  f.configBox = el("div", { class: "offer-from", hidden: true },
    field("MCP configuration (mcp.json, .mcp.json or .vscode/mcp.json)", f.config,
      "Paste it or load the file. ${workspaceFolder} becomes the shared folder; disabled and WebSocket servers are left out."),
    el("div", { class: "form-actions start" }, el("label", { class: "btn small file-btn" }, "Load a file…", f.file), f.readBtn),
    f.configPicked);

  f.kind = el("select", {}, el("option", { value: "local", text: "Runs on this machine (a command)" }), el("option", { value: "remote", text: "Remote (a URL)" }));
  f.command = el("input", { type: "text", class: "mono", spellcheck: "false", autocomplete: "off", placeholder: "e.g. npx @playwright/mcp@latest --headless…" });
  f.url = el("input", { type: "url", class: "mono", spellcheck: "false", autocomplete: "off", placeholder: "https://…" });
  f.sse = el("input", { type: "checkbox" });
  f.pairs = el("div", { class: "pair-rows" });
  f.addPair = el("button", { type: "button", class: "btn small", onclick: () => addPairRow(true) }, "Add a variable");
  f.pairsLabel = el("span", { class: "group-label", id: "offer-pairs-label", text: "Environment variables" });
  f.commandField = field("Command", f.command, "Quote arguments that contain spaces.");
  f.urlField = field("URL", f.url, "https://, or http:// on this machine.");
  f.urlField.hidden = true;
  f.sseField = el("label", { class: "check", hidden: true }, f.sse, el("span", { text: "It uses Server-Sent Events (older servers)" }));
  f.pairsField = el("div", { class: "field", role: "group", "aria-labelledby": "offer-pairs-label" },
    f.pairsLabel, f.pairs, el("div", { class: "form-actions start" }, f.addPair),
    el("span", { class: "muted small hint-text", text: "Values are typed hidden, stay on this machine and are never shown again." }));
  addPairRow(false);
  f.kind.addEventListener("change", syncHandKind);
  f.handBox = el("div", { class: "offer-from", hidden: true },
    field("Kind", f.kind),
    f.commandField, f.urlField, f.sseField, f.pairsField);

  f.access = accessSelect("full");
  f.tools = el("input", { type: "text", spellcheck: "false", autocomplete: "off", placeholder: "all of its tools…" });
  f.description = el("input", { type: "text", maxlength: "200", autocomplete: "off", placeholder: "e.g. A headless browser for checking pages…" });
  f.error = el("p", { class: "form-error", role: "alert" });
  f.submit = el("button", { type: "submit", class: "btn primary" }, "Offer server");
  const form = el("form", { id: "offer-form", class: "tool-form", novalidate: true, onsubmit: submitOffer },
    el("div", { class: "form-row" },
      field("Folder", f.share),
      field("Name tasks use", f.name)),
    el("div", { class: "field" }, el("span", { text: "Where it comes from" }), f.from),
    f.pluginBox, f.configBox, f.handBox,
    field("Least access a peer's task needs to use it", f.access,
      "A server runs with your rights and may reach the network: Full lets only the tasks you trust most use it."),
    field("Tools tasks may call", f.tools, "Separate with commas; blank allows all of its tools."),
    field("What it is for", f.description, "One line. Peers see it next to the name."),
    f.error,
    el("div", { class: "form-actions" }, f.submit));
  syncOfferFrom();
  return form;
}

function syncOfferFrom() {
  const f = tools.f;
  for (const btn of f.from.querySelectorAll("[role=radio]")) {
    const on = btn.dataset.from === tools.from;
    btn.setAttribute("aria-checked", String(on));
    btn.tabIndex = on ? 0 : -1;
  }
  f.pluginBox.hidden = tools.from !== "plugin";
  f.configBox.hidden = tools.from !== "config";
  f.handBox.hidden = tools.from !== "hand";
  f.error.textContent = "";
}

function syncHandKind() {
  const f = tools.f;
  const remote = f.kind.value === "remote";
  f.commandField.hidden = remote;
  f.urlField.hidden = !remote;
  f.sseField.hidden = !remote;
  f.pairsLabel.textContent = remote ? "Headers" : "Environment variables";
  f.addPair.textContent = remote ? "Add a header" : "Add a variable";
  for (const input of f.pairs.querySelectorAll("[data-pair=name]")) input.placeholder = remote ? "e.g. Authorization…" : "e.g. API_TOKEN…";
}

/** A name and a hidden value (an environment variable or a header). */
function addPairRow(focus) {
  const f = tools.f;
  const remote = f.kind.value === "remote";
  const name = el("input", {
    type: "text", class: "mono", "data-pair": "name", spellcheck: "false", autocomplete: "off",
    "aria-label": "Name", placeholder: remote ? "e.g. Authorization…" : "e.g. API_TOKEN…",
  });
  const value = el("input", { type: "password", "data-pair": "value", autocomplete: "off", "aria-label": "Value", placeholder: "value…" });
  name.addEventListener("input", () => value.setAttribute("aria-label", name.value ? `Value of ${name.value}` : "Value"));
  const row = el("div", { class: "pair-row" }, name, value);
  row.append(el("button", {
    type: "button", class: "icon-btn", "aria-label": "Remove this row", title: "Remove",
    onclick: () => {
      row.remove();
      if (!f.pairs.childElementCount) addPairRow(false);
    },
  }, icon(ICONS.trash)));
  f.pairs.append(row);
  if (focus) name.focus();
}

/** The name/value rows as an object; fails on a value without a name. */
function pairValues() {
  const pairs = {};
  for (const row of tools.f.pairs.querySelectorAll(".pair-row")) {
    const name = row.querySelector("[data-pair=name]");
    const value = row.querySelector("[data-pair=value]").value;
    if (!name.value.trim() && !value) continue;
    if (!name.value.trim()) {
      name.focus();
      throw new Error("Give each value a name.");
    }
    pairs[name.value.trim()] = value;
  }
  return pairs;
}

/** Keeps the offer form's choices (folders, plugin servers) current, keeping what's picked. */
function refreshOfferForm() {
  const f = tools.f;
  if (!f) return;
  const share = f.share.value;
  f.share.replaceChildren(...(tools.mcp.shares.length
    ? tools.mcp.shares.map((s) => el("option", { value: s.name, text: `${s.name} (peers' tasks: at most ${accessText(s.max_access)})` }))
    : [el("option", { value: "", text: "(no shared folders yet)" })]));
  if (tools.mcp.shares.some((s) => s.name === share)) f.share.value = share;
  const template = f.template.value;
  const templates = tools.mcp.templates;
  f.template.replaceChildren(...(templates.length
    ? templates.map((t) => el("option", { value: `${t.plugin}/${t.server}`, text: `${t.server} (plugin ${t.plugin})` }))
    : [el("option", { value: "", text: "(no plugin offers MCP servers: import one in Plugins)" })]));
  if (templates.some((t) => `${t.plugin}/${t.server}` === template)) f.template.value = template;
  if (f.template.value !== template) onTemplateChange();
  if (!tools.fromChosen) {
    tools.from = templates.length ? "plugin" : "config";
    syncOfferFrom();
  }
  $("#offer-section").hidden = !tools.mcp.shares.length;
  f.submit.disabled = !tools.mcp.shares.length;
}

function onTemplateChange() {
  const f = tools.f;
  const t = tools.mcp.templates.find((x) => `${x.plugin}/${x.server}` === f.template.value);
  $("#offer-template-desc").textContent = t?.describe ?? "";
  inputFields(f.pluginInputs, t?.inputs ?? []);
  if (t && !f.name.value) f.name.value = t.server;
}

/** Starts offering a plugin's server: the form, preselected. */
function offerTemplate(plugin, server) {
  selectTab("mcp");
  tools.from = "plugin";
  tools.fromChosen = true;
  syncOfferFrom();
  refreshOfferForm();
  tools.f.template.value = `${plugin}/${server}`;
  tools.f.name.value = server;
  onTemplateChange();
  $("#offer-section").scrollIntoView({ block: "start" });
  tools.f.share.focus();
}

async function readConfig() {
  const f = tools.f;
  f.error.textContent = "";
  if (!f.config.value.trim()) {
    f.error.textContent = "Paste an MCP configuration first.";
    f.config.focus();
    return;
  }
  try {
    tools.parsed = await api("/api/mcp/parse", { method: "POST", body: JSON.stringify({ json: f.config.value }) });
  } catch (err) {
    tools.parsed = null;
    f.configPicked.hidden = true;
    f.error.textContent = err.message;
    return;
  }
  const servers = tools.parsed.servers;
  f.configServer.replaceChildren(...servers.map((s) => el("option", { value: s.server, text: s.server })));
  f.configNotes.replaceChildren(...tools.parsed.notes.map((n) => el("p", { class: "note", text: n })));
  f.configPicked.hidden = !servers.length;
  if (!servers.length) f.error.textContent = "It has no servers Proofkin can offer.";
  onConfigServerChange();
  f.configServer.focus();
}

function onConfigServerChange() {
  const f = tools.f;
  const s = tools.parsed?.servers.find((x) => x.server === f.configServer.value);
  $("#offer-config-desc").textContent = s?.describe ?? "";
  inputFields(f.configInputs, s?.inputs ?? []);
  if (s) f.name.value = s.name;
}

async function submitOffer(e) {
  e.preventDefault();
  const f = tools.f;
  f.error.textContent = "";
  const body = {
    share: f.share.value,
    name: f.name.value.trim(),
    access: f.access.value,
    tools: splitList(f.tools.value),
    description: f.description.value.trim(),
  };
  const fail = (message, field) => {
    f.error.textContent = message;
    field?.focus();
  };
  if (!body.share) return fail("Share a folder first.", f.share);
  if (!body.name) return fail("Give the server the name tasks use.", f.name);
  try {
    if (tools.from === "plugin") {
      const [plugin, server] = f.template.value.split("/");
      if (!plugin) return fail("Import a plugin with MCP servers first, or pick another source.");
      Object.assign(body, { plugin, server, values: inputValues(f.pluginInputs) });
    } else if (tools.from === "config") {
      if (!tools.parsed || f.configPicked.hidden) return fail("Read the configuration first.", f.readBtn);
      Object.assign(body, { config: f.config.value, server: f.configServer.value, values: inputValues(f.configInputs) });
    } else if (f.kind.value === "remote") {
      if (!f.url.value.trim()) return fail("Give the server's URL.", f.url);
      Object.assign(body, { url: f.url.value.trim(), sse: f.sse.checked, headers: pairValues() });
    } else {
      if (!f.command.value.trim()) return fail("Give the command that starts the server.", f.command);
      Object.assign(body, { command_line: f.command.value.trim(), env: pairValues() });
    }
  } catch (err) {
    f.error.textContent = err.message;
    return;
  }
  f.submit.disabled = true;
  f.submit.textContent = "Offering…";
  try {
    await api("/api/mcp", { method: "POST", body: JSON.stringify(body) });
    toast(`${body.share} offers ${body.name} to peers' tasks with ${accessText(body.access)} access or more`);
    for (const field of [f.name, f.tools, f.description, f.command, f.url]) field.value = "";
    f.pairs.replaceChildren();
    addPairRow(false);
    for (const box of [f.pluginInputs, f.configInputs]) for (const input of box.querySelectorAll("input")) input.value = "";
    await loadTools();
    $("#mcp-offered").scrollIntoView({ block: "start" });
  } catch (err) {
    f.error.textContent = err.message;
  } finally {
    f.submit.disabled = !tools.mcp.shares.length;
    f.submit.textContent = "Offer server";
  }
}

// ---------- skills ----------
function buildSkillsPanel() {
  const name = el("input", { type: "text", id: "skill-name", maxlength: "64", spellcheck: "false", autocomplete: "off", placeholder: "taken from its front matter…" });
  const text = el("textarea", {
    id: "skill-text", rows: "8", class: "mono", spellcheck: "false",
    placeholder: "---\nname: house-style\ndescription: How we write user-facing text\n---\n# House style\n…",
  });
  const file = el("input", { type: "file", accept: ".md,text/markdown,text/plain" });
  file.addEventListener("change", async () => {
    const picked = file.files?.[0];
    if (!picked) return;
    text.value = await picked.text();
    if (!name.value && !/^---[\s\S]*?\nname:/m.test(text.value)) {
      name.value = picked.name === "SKILL.md" ? "" : picked.name.replace(/\.[^.]+$/, "");
    }
    text.focus();
  });
  const error = el("p", { class: "form-error", role: "alert" });
  const save = el("button", { type: "submit", class: "btn primary" }, "Save skill");
  const form = el("form", {
    class: "tool-form", novalidate: true,
    onsubmit: async (e) => {
      e.preventDefault();
      error.textContent = "";
      if (!text.value.trim()) {
        error.textContent = "Write or load the skill's instructions.";
        text.focus();
        return;
      }
      save.disabled = true;
      save.textContent = "Saving…";
      try {
        const res = await api("/api/skills", { method: "POST", body: JSON.stringify({ name: name.value.trim(), text: text.value }) });
        const replaced = { "built-in": " (it replaces the built-in one)", profile: " (it replaces your earlier one)", plugin: " (used instead of the plugin's)" }[res.replaced] ?? "";
        toast(`Saved skill ${res.name}${replaced}. Use it with skill=${res.name}`);
        name.value = "";
        text.value = "";
        file.value = "";
        await loadTools();
      } catch (err) {
        error.textContent = err.message;
      } finally {
        save.disabled = false;
        save.textContent = "Save skill";
      }
    },
  },
  el("div", { class: "form-actions start" }, el("label", { class: "btn small file-btn" }, "Load a SKILL.md or Markdown file…", file)),
  field("Name", name, "Optional when its front matter has a name: line. The same name as a built-in skill replaces it for this profile."),
  field("Instructions (Markdown)", text),
  error,
  el("div", { class: "form-actions" }, save));
  $("#panel-skills").replaceChildren(
    el("section", { class: "tool-section" },
      el("h3", { text: "Add a skill" }),
      el("p", { class: "muted small", text: "A skill is Markdown instructions agents follow in a plan step (skill=name), so every peer works the same way whichever provider runs it." }),
      form),
    el("section", { class: "tool-section" },
      el("h3", { id: "skill-list-h", text: "Skills" }),
      el("div", { id: "skill-groups" })));
}

function renderSkillList() {
  const box = $("#skill-groups");
  if (!box) return;
  $("#skill-list-h").textContent = `Skills (${tools.skills.length})`;
  const group = (title, skills, removable) => skills.length ? el("div", { class: "skill-group" },
    el("h4", { text: `${title} (${skills.length})` }),
    el("ul", { class: "tool-list compact" }, ...skills.map((s) => el("li", { class: "tool-item" },
      el("div", { class: "task-top" },
        el("button", { type: "button", class: "btn link", onclick: () => openSkill(s.name), "aria-label": `View skill ${s.name}` },
          el("span", { class: "mono", text: s.name })),
        s.plugin ? el("span", { class: "badge neutral", text: `plugin ${s.plugin}` }) : null,
        removable ? el("button", { type: "button", class: "btn small danger", onclick: () => removeSkill(s.name) }, "Remove") : null),
      s.description ? el("div", { class: "task-meta", text: s.description }) : null)))) : null;
  box.replaceChildren(...[
    group("Yours", tools.skills.filter((s) => s.source === "profile"), true),
    group("From plugins", tools.skills.filter((s) => s.source === "plugin"), false),
    group("Built in", tools.skills.filter((s) => s.source === "built-in"), false),
  ].filter(Boolean));
}

async function removeSkill(name) {
  const ok = await confirmDialog({
    title: `Remove skill ${name}?`,
    body: "Plans that name it can't run until it's back. If a built-in skill has the same name, that one applies again.",
    confirm: "Remove skill",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/skills/${encodeURIComponent(name)}`, { method: "DELETE" });
    toast(`Removed skill ${name}`);
    await loadTools();
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- agents ----------
/** What an agent follows and uses, in a line. */
function agentUses(a) {
  const parts = [a.skills.length ? `follows ${a.skills.join(", ")}` : "", a.mcp.length ? `uses MCP servers ${a.mcp.join(", ")}` : ""].filter(Boolean);
  return parts.length ? el("div", { class: "muted small", text: parts.join(" · ") }) : null;
}

function buildAgentsPanel() {
  const name = el("input", { type: "text", maxlength: "64", spellcheck: "false", autocomplete: "off", placeholder: "taken from its front matter…" });
  const text = el("textarea", {
    rows: "10", class: "mono", spellcheck: "false",
    placeholder: "---\nname: security-reviewer\ndescription: Reviews a change for security problems\nskills: [security-review, code-review]\nmcp-servers: [semgrep]\n---\nYou review changes the way a security engineer would…",
  });
  const file = el("input", { type: "file", accept: ".md,text/markdown,text/plain" });
  file.addEventListener("change", async () => {
    const picked = file.files?.[0];
    if (!picked) return;
    text.value = await picked.text();
    if (!name.value && !/^---[\s\S]*?\nname:/m.test(text.value)) name.value = picked.name.replace(/(\.agent)?\.md$/i, "");
    text.focus();
  });
  const error = el("p", { class: "form-error", role: "alert" });
  const save = el("button", { type: "submit", class: "btn primary" }, "Save agent");
  const form = el("form", {
    class: "tool-form", novalidate: true,
    onsubmit: async (e) => {
      e.preventDefault();
      error.textContent = "";
      if (!text.value.trim()) {
        error.textContent = "Write or load the agent: its front matter and instructions.";
        text.focus();
        return;
      }
      save.disabled = true;
      save.textContent = "Saving…";
      try {
        const res = await api("/api/agents", { method: "POST", body: JSON.stringify({ name: name.value.trim(), text: text.value }) });
        const replaced = { profile: " (it replaces your earlier one)", plugin: " (used instead of the plugin's)" }[res.replaced] ?? "";
        toast(`Saved agent ${res.agent.name}${replaced}. Use it with agent=${res.agent.name}`);
        for (const note of res.agent.notes ?? []) toast(note);
        name.value = "";
        text.value = "";
        file.value = "";
        await loadTools();
        await loadSkills();
      } catch (err) {
        error.textContent = err.message;
      } finally {
        save.disabled = false;
        save.textContent = "Save agent";
      }
    },
  },
  el("div", { class: "form-actions start" }, el("label", { class: "btn small file-btn" }, "Load an agent file (.md, .agent.md)…", file)),
  field("Name", name, "Optional when its front matter has a name: line."),
  field("Agent (front matter and instructions)", text,
    "The layout GitHub Copilot and Claude Code agents use. skills: and mcp-servers: name what it follows and uses; tools: and model: are left out, since the folder's owner sets what a step may do."),
  error,
  el("div", { class: "form-actions" }, save));
  $("#panel-agents").replaceChildren(
    el("section", { class: "tool-section" },
      el("h3", { text: "Add an agent" }),
      el("p", {
        class: "muted small",
        text: "An agent is a role a plan step takes (agent=name): its instructions come first, with the skills it follows and the MCP servers it uses. The step's folder must offer those servers, as for any step.",
      }),
      form),
    el("section", { class: "tool-section" },
      el("h3", { id: "agent-list-h", text: "Agents" }),
      el("div", { id: "agent-groups" })));
}

function renderAgentList() {
  const box = $("#agent-groups");
  if (!box) return;
  $("#agent-list-h").textContent = `Agents (${tools.agents.length})`;
  const item = (a, removable) => {
    const instructions = el("details", { class: "agent-text" },
      el("summary", { text: "Instructions" }),
      el("pre", { class: "mono", text: a.instructions }));
    return el("li", { class: "tool-item" },
      el("div", { class: "task-top" },
        el("span", { class: "mono", text: `agent=${a.name}` }),
        a.plugin ? el("span", { class: "badge neutral", text: `plugin ${a.plugin}` }) : null,
        a.version ? el("span", { class: "badge ok", title: "The factory's active release: plans use it", text: `release ${a.version}` }) : null,
        a.deprecated ? el("span", { class: "badge busy", text: `deprecated: use ${a.deprecated}` }) : null,
        removable ? el("button", { type: "button", class: "btn small danger", onclick: () => removeAgent(a.name) }, "Remove") : null),
      a.description ? el("div", { class: "task-meta", text: a.description }) : null,
      agentUses(a),
      ...(a.notes ?? []).map((n) => el("p", { class: "note", text: n })),
      instructions);
  };
  const group = (title, agents, removable) => agents.length ? el("div", { class: "skill-group" },
    el("h4", { text: `${title} (${agents.length})` }),
    el("ul", { class: "tool-list compact" }, ...agents.map((a) => item(a, removable)))) : null;
  const groups = [
    group("Yours", tools.agents.filter((a) => a.source === "profile"), true),
    group("From the factory", tools.agents.filter((a) => a.source === "factory"), false),
    group("From plugins", tools.agents.filter((a) => a.source === "plugin"), false),
  ].filter(Boolean);
  box.replaceChildren(...(groups.length ? groups
    : [el("p", { class: "muted small", text: "None yet: add one above, or import a plugin that has agents (agents/*.md)." })]));
}

async function removeAgent(name) {
  const ok = await confirmDialog({
    title: `Remove agent ${name}?`,
    body: "Plans that name it can't run until it's back. A plugin's agent of the same name applies again.",
    confirm: "Remove agent",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/agents/${encodeURIComponent(name)}`, { method: "DELETE" });
    toast(`Removed agent ${name}`);
    await loadTools();
    await loadSkills();
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- wiring ----------
(function initTools() {
  $("#tools-close").addEventListener("click", () => $("#tools-dialog").close());
  $("#tools-dialog").addEventListener("close", () => {
    tools.editing = null;
  });
  const tablist = $("#tools-dialog [role=tablist]");
  tablist.addEventListener("click", (e) => {
    const tab = e.target.closest("[role=tab]");
    if (tab) selectTab(tab.dataset.tab);
  });
  tablist.addEventListener("keydown", (e) => {
    const tabs = [...tablist.querySelectorAll("[role=tab]")];
    const i = tabs.findIndex((t) => t.dataset.tab === tools.tab);
    const next = { ArrowRight: i + 1, ArrowLeft: i - 1, Home: 0, End: tabs.length - 1 }[e.key];
    if (next === undefined) return;
    e.preventDefault();
    selectTab(tabs[(next + tabs.length) % tabs.length].dataset.tab, true);
  });
})();
