// Proofkin web UI. Plain JS, no build step. All peer-provided text is rendered
// with textContent (never innerHTML), so chat and task output can't inject markup.
"use strict";

const $ = (sel) => document.querySelector(sel);

function el(tag, attrs = {}, ...children) {
  const node = document.createElement(tag);
  for (const [key, value] of Object.entries(attrs)) {
    if (value === undefined || value === null || value === false) continue;
    if (key === "class") node.className = value;
    else if (key === "text") node.textContent = value;
    else if (key.startsWith("on")) node.addEventListener(key.slice(2), value);
    else node.setAttribute(key, value === true ? "" : value);
  }
  for (const child of children.flat()) {
    if (child === null || child === undefined || child === false) continue;
    node.append(child instanceof Node ? child : document.createTextNode(String(child)));
  }
  return node;
}

function icon(paths) {
  const svg = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  svg.setAttribute("viewBox", "0 0 24 24");
  svg.setAttribute("aria-hidden", "true");
  for (const d of paths) {
    const p = document.createElementNS("http://www.w3.org/2000/svg", "path");
    p.setAttribute("d", d);
    svg.append(p);
  }
  return svg;
}
const ICONS = {
  trash: ["M4 7h16", "M10 11v6M14 11v6", "M6 7l1 13h10l1-13", "M9 7V4h6v3"],
  copy: ["M9 9h11v11H9z", "M5 15V5a2 2 0 0 1 2-2h10"],
  stop: ["M6 6h12v12H6z"],
  peers: ["M8 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6z", "M16 11a3 3 0 1 0 0-6", "M2 20a6 6 0 0 1 12 0", "M15 14a6 6 0 0 1 7 6"],
};

// ---------- theme ----------
const THEME_KEY = "p2pagents-theme";
const media = window.matchMedia("(prefers-color-scheme: dark)");
function themeChoice() {
  const saved = localStorage.getItem(THEME_KEY);
  return ["light", "dark", "system"].includes(saved) ? saved : "system";
}
function applyTheme() {
  const choice = themeChoice();
  const effective = choice === "system" ? (media.matches ? "dark" : "light") : choice;
  document.documentElement.dataset.theme = effective;
  $("#theme-color")?.setAttribute("content", effective === "dark" ? "#0d1016" : "#f5f6f8");
  for (const btn of document.querySelectorAll("[data-theme-choice]")) {
    const on = btn.dataset.themeChoice === choice;
    btn.setAttribute("aria-checked", String(on));
    btn.tabIndex = on ? 0 : -1;
  }
}
media.addEventListener("change", applyTheme);

// ---------- dialogs ----------
/// Asks before something is lost, in the page (never window.confirm). Resolves
/// true when confirmed. `body` says what is lost and what stays.
function confirmDialog({ title, body, confirm = "Confirm", danger = false }) {
  const dialog = $("#confirm-dialog");
  const opener = document.activeElement;
  $("#confirm-h").textContent = title;
  $("#confirm-body").textContent = body;
  const ok = $("#confirm-ok");
  ok.textContent = confirm;
  ok.classList.toggle("danger-fill", danger);
  ok.classList.toggle("primary", !danger);
  dialog.returnValue = "";
  return new Promise((resolve) => {
    dialog.addEventListener("close", () => {
      resolve(dialog.returnValue === "ok");
      if (opener instanceof HTMLElement && opener.isConnected) opener.focus();
    }, { once: true });
    dialog.showModal();
    (danger ? $("#confirm-cancel") : ok).focus();
  });
}

/// A dialog closes on a backdrop click only while nothing was typed in it, so a
/// stray click never discards input. Escape and Cancel always close it (not every
/// browser closes a modal dialog on Escape by itself).
function guardDialog(dialog) {
  let dirty = false;
  dialog.addEventListener("input", () => { dirty = true; });
  dialog.addEventListener("close", () => { dirty = false; });
  dialog.addEventListener("click", (e) => {
    if (e.target === dialog && !dirty) dialog.close();
  });
  dialog.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && !e.defaultPrevented) {
      e.preventDefault();
      dialog.close();
    }
  });
}

// Radio groups: click and arrow keys (WAI-ARIA radiogroup pattern).
function radioGroup(container, onSelect) {
  const buttons = () => [...container.querySelectorAll('[role="radio"]')];
  container.addEventListener("click", (e) => {
    const btn = e.target.closest('[role="radio"]');
    if (btn) onSelect(btn, false);
  });
  container.addEventListener("keydown", (e) => {
    const keys = { ArrowRight: 1, ArrowDown: 1, ArrowLeft: -1, ArrowUp: -1 };
    if (!(e.key in keys)) return;
    e.preventDefault();
    const list = buttons();
    const i = list.indexOf(document.activeElement);
    const next = list[(i + keys[e.key] + list.length) % list.length];
    next.focus();
    onSelect(next, true);
  });
}

// Markdown rendering (renderMarkdown, inlineMd) lives in ui/markdown.js.

// ---------- API ----------
const hashToken = new URLSearchParams(location.hash.slice(1)).get("t");
if (hashToken) {
  sessionStorage.setItem("p2pagents-token", hashToken);
  history.replaceState(null, "", location.pathname);
}
const token = sessionStorage.getItem("p2pagents-token") || "";
// Opening a new link (#t=…) in an existing tab only changes the hash: adopt it.
window.addEventListener("hashchange", () => {
  const t = new URLSearchParams(location.hash.slice(1)).get("t");
  if (t) {
    sessionStorage.setItem("p2pagents-token", t);
    location.replace(location.pathname);
  }
});

let authLost = false;
function showAuthError() {
  if (authLost) return;
  authLost = true;
  events?.close();
  const pill = $("#net-status");
  pill.dataset.state = "offline";
  pill.querySelector(".label").textContent = "Session ended";
  $("#feed").replaceChildren(el("div", { class: "feed-empty" },
    el("h2", { text: "This page's session has ended" }),
    el("p", { text: "The agent was restarted, so its access link changed. Open the new link Proofkin printed (it ends with #t=…)." })));
  for (const c of document.querySelectorAll("#composer textarea, #composer button, #add-peer-btn")) c.disabled = true;
}

async function api(path, options = {}) {
  const res = await fetch(path, {
    ...options,
    headers: { "Content-Type": "application/json", Authorization: `Bearer ${token}`, ...(options.headers || {}) },
  });
  if (res.status === 401) {
    showAuthError();
    throw new Error("Session ended: open the new link printed by Proofkin");
  }
  const body = await res.json().catch(() => ({}));
  if (!res.ok) {
    const err = new Error(body.error || `${res.status} ${res.statusText}`);
    err.body = body;
    throw err;
  }
  return body;
}

// ---------- state ----------
const state = { me: null, peers: [], trusted: [], feed: [], tasks: [], mode: "chat", playbook: null };

function hue(text) {
  let h = 0;
  for (const ch of text) h = (h * 31 + ch.codePointAt(0)) % 360;
  return h;
}
function avatar(name, online) {
  const node = el("span", { class: "avatar", "aria-hidden": "true" },
    (name[0] || "?").toUpperCase(),
    online === undefined ? null : el("span", { class: `presence${online ? " on" : ""}` }));
  // CSSOM, not a style attribute: the page's CSP forbids inline style attributes.
  node.style.setProperty("--hue", String(hue(name)));
  return node;
}
function providerLabel(p) {
  return { copilot: "Copilot", claude: "Claude", codex: "Codex" }[p] || p;
}
function clock(ms) {
  return new Date(ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}
const dayWords = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
/** When something happened: its time, with its day unless it is today ("Yesterday, 9:25 PM",
 * "Nov 18, 9:25 PM"). Times to come use when(). */
function stamp(ms) {
  const d = new Date(ms);
  const now = new Date();
  const days = Math.round((new Date(now.getFullYear(), now.getMonth(), now.getDate())
    - new Date(d.getFullYear(), d.getMonth(), d.getDate())) / 86_400_000);
  if (days === 0) return clock(ms);
  if (Math.abs(days) === 1) {
    const word = dayWords.format(-days, "day");
    return `${word.charAt(0).toLocaleUpperCase()}${word.slice(1)}, ${clock(ms)}`;
  }
  const date = d.toLocaleDateString([], {
    month: "short", day: "numeric", ...(d.getFullYear() === now.getFullYear() ? {} : { year: "numeric" }),
  });
  return `${date}, ${clock(ms)}`;
}
function duration(ms) {
  const s = Math.max(0, Math.round(ms / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${s % 60}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}
// "2 done, 1 failed": the counts that aren't zero, named.
function counts(pairs) {
  return pairs.filter(([n]) => n).map(([n, what]) => `${n} ${what}`).join(", ");
}

function toast(text, kind = "") {
  const node = el("div", { class: `toast ${kind}`, role: kind === "error" ? "alert" : "status", text });
  $("#toasts").append(node);
  setTimeout(() => node.remove(), kind === "error" ? 6000 : 2600);
}
async function copy(text, what) {
  try {
    await navigator.clipboard.writeText(text);
    toast(`${what} copied`);
  } catch {
    toast(`Could not copy ${what.toLowerCase()}`, "error");
  }
}

// A UX prototype or a review pack opens in a new tab. The server sandboxes it
// (opaque origin, no network), and the tab is cut off from this page before it loads.
async function openPreview(body, button) {
  const tab = window.open("", "_blank");
  if (tab) tab.opener = null;
  if (button) button.disabled = true;
  try {
    const { url } = await api("/api/previews", { method: "POST", body: JSON.stringify(body) });
    if (tab) tab.location.replace(url);
    else window.open(url, "_blank", "noopener");
  } catch (err) {
    tab?.close();
    toast(err.message, "error");
  } finally {
    if (button) button.disabled = false;
  }
}

function previewLink(href, label = "Open prototype") {
  return el("a", { class: "btn small", href, target: "_blank", rel: "noopener noreferrer" }, label);
}

/// Button text for a preview: "prototype" or "review pack".
function previewText(label) {
  return label === "review pack" ? "Open review pack" : "Open prototype";
}

// ---------- rendering ----------
function renderMe() {
  const me = state.me;
  if (!me) return;
  document.title = `${me.name} · Proofkin`;
  $("#me-name").textContent = me.name;
  const idChip = $("#me-id");
  idChip.textContent = me.short;
  idChip.title = `Endpoint id ${me.id} (click to copy)`;
  $("#ticket").textContent = me.ticket;

  const pill = $("#net-status");
  const online = me.network === "local" || !!me.relay;
  pill.dataset.state = online ? "online" : "offline";
  pill.querySelector(".label").textContent =
    me.network === "local" ? "Local network" : me.relay ? "Online" : "Offline";
  pill.title = me.relay ? `Home relay: ${me.relay}` : me.network === "local" ? "Direct IP connections only" : "No relay reachable";

  const facts = $("#facts");
  facts.replaceChildren();
  const fact = (k, v) => facts.append(el("dt", { text: k }), el("dd", {}, v));
  fact("Profile", me.profile);
  fact("Network", me.network === "local" ? "Local (direct IPs)" : "Internet");
  if (me.providers.length) {
    fact("Providers", me.providers.map(providerLabel).join(", "));
    for (const a of me.accounts) fact(providerLabel(a.provider), a.account);
    // The model and reasoning effort each provider's tasks run at unless they ask for
    // others, and what peers may ask for.
    fact("Models", el("div", { class: "facts-models" },
      el("ul", { class: "share-list" }, ...me.providers.map((p) =>
        el("li", { text: `${providerLabel(p)}: ${modelsSummary(me.models?.[p])}` }))),
      el("button", {
        type: "button", class: "btn small", "aria-label": "Models of this agent",
        title: me.serving ? "Choose the models and reasoning levels peers may ask this agent for, and its defaults"
          : "Choose the models and reasoning levels your tasks here run with",
        onclick: (e) => openModelsDialog(null, e.currentTarget),
      }, "Models…")));
    fact("Serves peers", me.serving ? "Yes" : "No");
    fact("Your own tasks", el("span", {}, `${ACCESS_SHORT[me.access] ?? me.access} in `,
      el("span", { class: "mono", text: me.workspace })));
    if (me.serving) {
      fact("Shared folders", el("ul", { class: "share-list" },
        ...me.shares.map((s) => el("li", {},
          el("strong", { text: s.name }), ` · at most ${ACCESS_SHORT[s.max_access] ?? s.max_access}`,
          el("div", { class: "mono small", text: s.path }),
          (s.mcp ?? []).length ? el("div", { class: "small muted" }, "MCP servers: ",
            ...s.mcp.flatMap((m, i) => [i ? ", " : "", el("span", {
              class: "mono",
              title: `${m.describe}\nTools: ${m.tools.length ? m.tools.join(", ") : "all"}\nFor tasks with at least ${ACCESS_SHORT[m.access] ?? m.access} access`,
              text: m.name,
            })])) : null,
          s.preview ? el("button", {
            type: "button", class: "btn small link",
            title: s.preview === "review pack"
              ? "Preview this folder's review decks, design documents and prototype in a sandboxed tab"
              : "Preview prototype/index.html of this folder in a sandboxed tab",
            onclick: (e) => openPreview({ share: s.name }, e.currentTarget),
          }, s.preview === "review pack" ? "Preview review pack" : "Preview prototype") : null,
          s.name === "data" || !["workspace-write", "full"].includes(s.max_access) ? null : el("div", { class: "share-team" },
            el("button", {
              type: "button", class: "btn small", "aria-label": `Team plan for ${s.name}`,
              title: "Set what this agent's team does when a peer hands this folder a team step",
              onclick: (e) => openTeamDialog(s, e.currentTarget),
            }, "Team plan…"),
            teamLine(s) ? el("span", { class: "small muted", text: teamLine(s) }) : null),
          s.worktrees && ["workspace-write", "full"].includes(s.max_access) ? el("div", { class: "share-team" }, pullRequestsToggle(s)) : null))));
    }
  } else {
    fact("Providers", "None (delegates only)");
    // Its settings are its digital workers' (they run on its accounts) unless they set their own.
    const set = Object.entries(me.models ?? {}).map(([p, m]) => `${providerLabel(p)}: ${modelsSummary(m)}`);
    fact("Digital workers run", el("div", { class: "facts-models" },
      set.length ? el("ul", { class: "share-list" }, ...set.map((t) => el("li", { text: t })))
        : el("span", { class: "muted", text: "Each provider's defaults" }),
      el("button", {
        type: "button", class: "btn small", "aria-label": "Models of this agent",
        title: "Choose the models and reasoning levels your digital workers use, unless they have their own",
        onclick: (e) => openModelsDialog(null, e.currentTarget),
      }, "Models…")));
  }
}

/** How busy a peer says it is with its peers' tasks (newer peers say). */
function loadBadge(p) {
  const l = p.load;
  if (!l || !p.capabilities.length) return null;
  const title = `Tasks it runs for its peers now (${l.slots} at a time), and those waiting for a slot${l.yours ? `; ${l.yours} of them yours` : ""}`;
  if (!l.running && !l.queued) return el("span", { class: "badge neutral", title, text: "idle" });
  return el("span", { class: "badge busy", title,
    text: `${l.running}/${l.slots} busy${l.queued ? ` · ${l.queued} waiting` : ""}` });
}

function renderPeers() {
  const list = $("#peer-list");
  list.replaceChildren();
  $("#peer-count").textContent = state.peers.length;
  $("#peer-count").setAttribute("aria-label", `${state.peers.length} connected`);
  if (!state.peers.length) {
    list.append(el("li", { class: "empty" },
      state.trusted.length
        ? "Waiting for allowlisted peers to come online…"
        : el("span", {}, "No peers yet. ", el("strong", { text: "Add a peer" }), " to the allowlist and share your ticket.")));
  }
  for (const p of state.peers) {
    list.append(el("li", {},
      avatar(p.name, true),
      el("div", { class: "who" },
        el("span", { class: "name", text: p.name }),
        p.owner || p.role
          ? el("span", { class: "role", text: [p.owner ? `Digital worker of ${p.owner}` : null, p.role].filter(Boolean).join(" · ") })
          : null,
        el("span", { class: "sub" },
          el("span", { class: "mono", text: p.short }),
          p.capabilities.length
            ? p.capabilities.map((c) => el("span", { class: "badge", text: providerLabel(c) }))
            : el("span", { class: "badge neutral", text: "no providers" }),
          loadBadge(p)))));
  }

  const trust = $("#trust-list");
  trust.replaceChildren();
  if (!state.trusted.length) {
    trust.append(el("li", { class: "empty", text: "Only allowlisted peers can connect to this agent." }));
  }
  for (const t of state.trusted) {
    trust.append(el("li", {},
      avatar(t.name, t.connected),
      el("div", { class: "who" },
        el("span", { class: "name", text: t.name }),
        el("span", { class: "sub" },
          el("span", { text: t.connected ? "Connected" : "Offline" }),
          t.can_delegate
            ? [el("span", { class: "badge ok", title: "The access its tasks get here, and how many it may run an hour", text: `Tasks: ${ACCESS_SHORT[t.access] ?? "Read only"} · ${t.tasks_per_hour}/h` }),
              // The folders as text: in the pill they wrap into a blob in a narrow column.
              el("span", { class: "trust-shares", text: `in ${(t.shares ?? []).join(", ") || "no folder"}` })]
            : el("span", { class: "badge neutral", title: "It can connect and chat, but can't run tasks on this agent's accounts", text: "Chat only" }),
          t.m365
            ? el("span", {
              class: "trust-m365",
              title: `It may receive Microsoft 365 content with these sensitivity labels${t.m365.tools ? ", and its tasks may use this agent's Work IQ (read only)" : ""}`,
              text: `M365: ${(t.m365.labels ?? []).join(", ")}${t.m365.tools ? " · Work IQ tools" : ""}`,
            })
            : null,
          t.github
            ? el("span", { class: "trust-github", title: "Its GitHub account: pull request steps ask it to review", text: `GitHub ${t.github}` })
            : null)),
      el("div", { class: "row-actions" },
        el("button", {
          type: "button", class: "btn small", "aria-label": `Change what ${t.name} may do`,
          onclick: (e) => openGrantDialog(t, e.currentTarget),
        }, "Change…"),
        el("button", {
          type: "button", class: "icon-btn", title: `Remove ${t.name}`, "aria-label": `Remove ${t.name} from the allowlist`,
          onclick: () => removePeer(t),
        }, icon(ICONS.trash)))));
  }
  // The first steps are done once a peer is on the allowlist, online or not.
  if (state.trusted.length && $("#feed .get-started")) renderFeed();
  // Refused peers that are allowlisted now need no button to allowlist them.
  for (const b of document.querySelectorAll("#feed button[data-peer]")) {
    if (state.trusted.some((t) => t.id === b.dataset.peer)) b.remove();
  }
  renderTaskOptions();
}

function feedNode(item) {
  if (item.kind === "chat") {
    return el("div", { class: `msg${item.self ? " self" : ""}` },
      item.self ? null : avatar(item.from),
      el("div", {},
        el("div", { class: "meta" }, item.self ? "You" : item.from, " · ", el("time", { text: stamp(item.ts) })),
        el("div", { class: "bubble", text: item.text })));
  }
  return el("div", { class: "sys", "data-level": item.level },
    el("span", { text: item.text }), el("time", { text: stamp(item.ts) }),
    // A refused peer may be a teammate who allowlisted you first.
    item.peer && !state.trusted.some((t) => t.id === item.peer)
      ? el("button", {
        type: "button", class: "btn small", "data-peer": item.peer,
        title: "Open Add peer with its endpoint id. Only allowlist someone you know: compare it with what they sent you.",
        onclick: () => openAddDialog(item.peer),
      }, "Allowlist…")
      : null);
}

function renderFeed() {
  const feed = $("#feed");
  feed.replaceChildren();
  for (const item of state.feed) feed.append(feedNode(item));
  // A new agent's first steps (nobody on its allowlist yet, whatever else happened);
  // else, until something happens beyond this agent's own notices, what will show up
  // here. Below those notices, never instead of them.
  const start = getStartedNode();
  if (start) {
    feed.append(start);
  } else if (!state.feed.some((i) => i.kind === "chat" || i.level !== "info")) {
    feed.append(el("div", { class: "feed-empty" },
      icon(ICONS.peers),
      el("h2", { text: "No messages yet" }),
      el("p", { text: "Messages, peers joining and leaving, and tasks from peers show up here." })));
  }
  feed.scrollTop = feed.scrollHeight;
}

function appendFeed(item) {
  state.feed.push(item);
  const feed = $("#feed");
  const nearBottom = feed.scrollHeight - feed.scrollTop - feed.clientHeight < 80;
  const placeholder = feed.querySelector(".feed-empty, .get-started");
  if (placeholder?.classList.contains("feed-empty") && (item.kind === "chat" || item.level !== "info")) {
    renderFeed();
    return;
  }
  if (placeholder) feed.insertBefore(feedNode(item), placeholder);
  else feed.append(feedNode(item));
  if (nearBottom || item.self) feed.scrollTop = feed.scrollHeight;
}

// ---------- first steps ----------
const GET_STARTED_KEY = "proofkin-get-started";

/** First steps for a new agent (no peer on its allowlist yet): share the ticket, add a
 * teammate's, or create a digital worker; null once it has a peer or was hidden. */
function getStartedNode() {
  if (state.trusted.length || localStorage.getItem(GET_STARTED_KEY) === "hidden") return null;
  const canOwn = typeof workerState === "undefined" || !workerState.loaded || workerState.canOwn;
  const copied = localStorage.getItem(`${GET_STARTED_KEY}-ticket`) === "copied";
  const step = (n, done, title, text, button) => el("li", { "data-done": String(done) },
    el("span", { class: "step-mark", "aria-hidden": "true", text: done ? "✓" : String(n) }),
    el("span", { class: "step-text" }, el("strong", { text: title }), el("span", { text })),
    button);
  return el("section", { class: "get-started", "aria-labelledby": "get-started-h" },
    el("h2", { id: "get-started-h", text: "Get started: connect an agent to work with" }),
    el("ol", {},
      step(1, copied, "Share your ticket", "Teammates add it to their allowlist to work with you.",
        el("button", { type: "button", class: "btn small", onclick: copyTicket }, copied ? "Copy again" : "Copy ticket")),
      step(2, false, "Add a teammate's ticket", "Only peers on your allowlist connect and take your tasks.",
        el("button", { type: "button", class: "btn small primary", onclick: openAddDialog }, "Add peer")),
      canOwn ? step(3, false, "No teammate yet? Create a digital worker", "An agent of yours, running on your own Copilot sign-in.",
        el("button", { type: "button", class: "btn small", onclick: openWorkerDialog }, "New worker")) : null),
    el("p", { class: "muted small" }, "Then choose ", el("strong", { text: "Run task" }),
      " and pick the checks its runtime runs: the work is proven only if they pass, with a signed record."),
    el("button", {
      type: "button", class: "btn small dismiss",
      onclick: () => { localStorage.setItem(GET_STARTED_KEY, "hidden"); renderFeed(); },
    }, "Hide"));
}

function copyTicket() {
  if (!state.me) return;
  copy(state.me.ticket, "Ticket");
  localStorage.setItem(`${GET_STARTED_KEY}-ticket`, "copied");
  if ($("#feed .get-started")) renderFeed();
}

const openResults = new Map();
const expandedPrompts = new Set();
const PROMPT_LINES = 4;

function promptNode(t) {
  // A pull request step without a title of its own takes that of the work it opens.
  if (t.pull_request && !t.prompt.trim()) {
    return el("div", { class: "task-prompt muted", text: "Opens the pull request with the work before it, titled as that work" });
  }
  const text = el("div", { class: "task-prompt" }, renderMarkdown(t.prompt));
  const long = t.prompt.split("\n").length > PROMPT_LINES || t.prompt.length > PROMPT_LINES * 80;
  if (!long) return text;
  const open = expandedPrompts.has(t.id);
  text.classList.toggle("clamped", !open);
  const more = el("button", { type: "button", class: "btn small link", "aria-expanded": String(open) },
    open ? "Show less" : "Show all");
  const setOpen = (on) => {
    text.classList.toggle("clamped", !on);
    if (on) expandedPrompts.add(t.id); else expandedPrompts.delete(t.id);
    more.setAttribute("aria-expanded", String(on));
    more.textContent = on ? "Show less" : "Show all";
  };
  more.addEventListener("click", () => setOpen(more.getAttribute("aria-expanded") !== "true"));
  // Keyboard focus never stays on something clipped out of sight.
  text.addEventListener("focusin", () => setOpen(true));
  return el("div", { class: "task-prompt-box" }, text, more);
}
const openPlans = new Map();
const STATUS_TEXT = {
  running: "Running", done: "Done", failed: "Failed", cancelled: "Cancelled", waiting: "Waiting", skipped: "Skipped",
};

function statusNode(status) {
  return el("span", { class: `status ${status}` },
    status === "running" ? el("span", { class: "spinner", "aria-hidden": "true" }) : null, STATUS_TEXT[status] ?? status);
}

/** A team run's steps as its lead reports them: each one's status, who runs it and
 * what it is doing; a team step's own team run nested a level further down. */
function teamStepsNode(steps, label) {
  return el("ol", { class: "team-steps", "aria-label": label },
    ...steps.map((s) => el("li", { class: `team-step ${s.status}` },
      el("div", { class: "team-step-top" },
        statusNode(s.status),
        el("span", { class: "mono", text: s.id }),
        el("span", { class: "muted small", text: `on ${s.target}${s.signed ? " · signed ✓" : ""}` })),
      s.text ? el("div", { class: "team-step-text" }, renderMarkdown(s.text)) : null,
      s.steps?.length ? teamStepsNode(s.steps, `Team run of step ${s.id}`) : null)));
}

/** A team step's result without the summary of its team run at the top (its card
 * shows the run and its steps above it); Copy still copies all of it. */
function teamResult(t) {
  if (!t.team || !t.output.startsWith("Team run ")) return t.output;
  const at = t.output.indexOf("\n### ");
  return at < 0 ? t.output : t.output.slice(at + 1);
}

/** A progress line's text, with the pull request it names (a pull request step) as a link. */
function progressParts(text) {
  const parts = [];
  let at = 0;
  for (const m of text.matchAll(/https:\/\/[^\s()]+\/pull\/(\d+)/g)) {
    parts.push(text.slice(at, m.index), externalLink(m[0], m[0], `Pull request #${m[1]} on GitHub`));
    at = m.index + m[0].length;
  }
  parts.push(text.slice(at));
  return parts.filter((p) => p !== "");
}

/** `node` with the pull requests a pull request step's result names as links: its words are
 * its agent's, not a model's (the Markdown of results never links). */
function linkPullRequests(node) {
  const walker = document.createTreeWalker(node, NodeFilter.SHOW_TEXT);
  const texts = [];
  while (walker.nextNode()) texts.push(walker.currentNode);
  for (const text of texts) {
    if (text.parentElement?.closest("code")) continue;
    const parts = progressParts(text.textContent);
    if (parts.some((p) => typeof p !== "string")) text.replaceWith(...parts);
  }
  return node;
}

/** What work used, as its provider reported it, in the browser's locale: "12,345 tokens
 * in (10,000 cached), 678 tokens out, 1 premium request, $0.0123". Only what it reported. */
function usageText(u) {
  if (!u) return "";
  const n = (x) => x.toLocaleString();
  const parts = [];
  if (u.input_tokens != null) {
    parts.push(`${n(u.input_tokens)} tokens in${u.cached_input_tokens != null ? ` (${n(u.cached_input_tokens)} cached)` : ""}`);
  } else if (u.cached_input_tokens != null) {
    parts.push(`${n(u.cached_input_tokens)} cached tokens in`);
  }
  if (u.output_tokens != null) parts.push(`${n(u.output_tokens)} tokens out`);
  if (u.premium_requests != null) {
    const p = Number(u.premium_requests.toFixed(2));
    parts.push(`${n(p)} premium request${p === 1 ? "" : "s"}`);
  }
  if (u.cost_usd != null) {
    parts.push(new Intl.NumberFormat(undefined, {
      style: "currency", currency: "USD", maximumFractionDigits: u.cost_usd < 1 ? 4 : 2,
    }).format(u.cost_usd));
  }
  return parts.join(", ");
}

function taskNode(t) {
  const elapsed = (t.finished_ms || Date.now()) - t.started_ms;
  // The model that did the work, after the provider, once its provider (or peer) said.
  const runs = (label) => t.model ? `${label} · ${t.model}` : label;
  const title = t.step
    ? `Step ${t.step} · ${t.approval ? "Approval" : t.team_step ? "Team" : t.pull_request ? "Pull request" : runs(providerLabel(t.provider))} → ${t.target}`
    : `#${t.id} · ${runs(providerLabel(t.provider))} → ${t.target}${t.squad ? ` · #${t.squad}` : ""}`;
  // Started by another agent (over MCP or A2A), not in this UI; or a step of a team
  // run this agent leads for a peer.
  const via = t.via ? ` · asked by another agent (${t.via})` : t.for_peer ? ` · for ${t.for_peer}'s team step` : "";
  // A task the agent stopped in the middle (it restarted since) has no end time.
  const ran = t.status === "running" ? `running ${duration(elapsed)}` : t.finished_ms ? `took ${duration(elapsed)}` : null;
  const meta = t.status === "waiting"
    ? el("div", { class: "task-meta", text: (t.after?.length ? `Waits for: ${t.after.join(", ")}` : "Starting…") + via })
    : el("div", { class: "task-meta" }, `Started ${stamp(t.started_ms)}`,
      ran ? " · " : null,
      ran ? el("span", { class: "elapsed", "data-start": t.status === "running" ? t.started_ms : null }, ran) : null,
      t.usage ? " · " : null,
      t.usage ? el("span", { title: "What the work used, as its provider reported it" }, `used ${usageText(t.usage)}`) : null,
      t.after?.length ? ` · after ${t.after.join(", ")}` : null, via || null);
  const node = el("li", { class: `task ${t.status}`, "data-task": t.id, tabindex: "-1" },
    el("div", { class: "task-top" },
      el("span", { class: "task-title", text: title }),
      t.issue ? externalLink(t.issue.url, `#${t.issue.number}`, `Its issue, #${t.issue.number}, on GitHub`, { class: "issue-link" }) : null,
      t.evidence ? el("span", {
        class: `badge evidence ${t.evidence}`,
        title: t.evidence === "verified"
          ? "The result carries evidence signed by the peer that did the work"
          : "The evidence is NOT validly signed by that peer",
        text: t.evidence === "verified" ? "Signed ✓" : "Unverified ✗",
      }) : null,
      statusNode(t.status)),
    promptNode(t),
    meta);
  // What its peer last said it is doing (queued, attempt 2 of 3, running a check…).
  if (t.status === "running" && t.progress?.text) {
    node.append(el("p", { class: `task-progress ${t.progress.phase ?? ""}` }, ...progressParts(t.progress.text)));
  }
  // A team step: its lead's team, live, then as the team run ended.
  const team = t.status === "running" ? t.progress?.steps : t.team?.steps;
  if (team?.length) {
    node.append(el("div", { class: "team-run" },
      el("p", { class: "task-meta", text: t.team ? `${t.team.lead}'s team run ${t.team.run}: ${t.team.verdict}` : `${t.target}'s team:` }),
      teamStepsNode(team, `The team steps of ${t.step ? `step ${t.step}` : `task #${t.id}`}`)));
  }
  // Why it was cancelled or skipped (by you, a failed step, or the agent stopping).
  if (t.output && ["cancelled", "skipped"].includes(t.status)) {
    node.append(el("p", { class: "task-meta task-why", text: t.output }));
  }
  if (t.preview) {
    const pack = t.preview_label === "review pack";
    node.append(el("div", { class: "task-actions" },
      previewLink(t.preview, previewText(t.preview_label)),
      el("span", {
        class: "task-meta",
        text: pack
          ? "Sandboxed preview of the decks, design documents and prototype this step created"
          : "Sandboxed preview of the UX prototype this step created",
      })));
  }
  // A plan's steps are cancelled, and open their evidence run, from the plan.
  if (!t.plan && t.status === "running") {
    node.append(el("button", {
      type: "button", class: "btn small", "aria-label": `Cancel task #${t.id}`,
      onclick: (e) => cancelTask(t, e.currentTarget),
    }, icon(ICONS.stop), "Cancel"));
  }
  if (!t.plan && t.run) {
    node.append(el("div", { class: "task-actions" },
      el("button", {
        type: "button", class: "btn small", "aria-label": `Open the evidence run of task #${t.id}`,
        onclick: () => openRun(t.run),
      }, "Evidence run")));
  }
  if (t.output !== null && t.output !== undefined && !["cancelled", "skipped"].includes(t.status)) {
    const open = openResults.has(t.id) ? openResults.get(t.id) : true;
    const details = el("details", { open: open || undefined },
      el("summary", { text: t.status === "failed" ? "Error" : "Result" }),
      // A pull request step's ending is in its own words (its pull request was closed),
      // with the link: shown as written, not as raw text.
      t.status === "failed" && !t.pull_request
        ? el("pre", { text: t.output, tabindex: "0" })
        : t.pull_request
          ? linkPullRequests(el("div", { class: "result", tabindex: "0" }, renderMarkdown(t.output)))
          : el("div", { class: "result", tabindex: "0" }, renderMarkdown(teamResult(t))),
      el("button", { type: "button", class: "btn small", onclick: () => copy(t.output, "Result") }, icon(ICONS.copy), "Copy"));
    details.addEventListener("toggle", () => openResults.set(t.id, details.open));
    node.append(details);
  }
  return node;
}

/** The step of a plan, shown: its plan's steps open, and it in view and focused. */
function showStep(id) {
  const card = document.querySelector(`[data-task="${id}"]`);
  if (!card) return;
  const details = card.closest("details");
  if (details) details.open = true;
  card.scrollIntoView({ block: "nearest", behavior: "smooth" });
  card.focus({ preventScroll: true });
}

/** A plan run (here, or a team run this agent leads for a peer): how far it is, its
 * steps in the order they can run, then each step. */
function planNode(planId, steps) {
  const count = (status) => steps.filter((s) => s.status === status).length;
  const [done, running, waiting] = [count("done"), count("running"), count("waiting")];
  const ended = !running && !waiting;
  const lead = steps.find((s) => s.for_peer)?.for_peer;
  const words = [
    `${done} of ${steps.length} step${steps.length === 1 ? "" : "s"} done`,
    running ? `${running} running` : "", waiting ? `${waiting} waiting` : "",
    ...["failed", "skipped", "cancelled"].map((s) => (count(s) ? `${count(s)} ${s}` : "")),
  ].filter(Boolean).join(" · ");
  const bar = el("div", {
    class: "plan-bar", role: "progressbar", "aria-label": lead ? `Team run for ${lead}` : "Plan progress",
    "aria-valuemin": "0", "aria-valuemax": String(steps.length), "aria-valuenow": String(steps.length - running - waiting),
    "aria-valuetext": words,
  });
  for (const status of ["done", "failed", "skipped", "cancelled", "running", "waiting"]) {
    if (!count(status)) continue;
    const seg = el("span", { class: `seg ${status}` });
    seg.style.flexGrow = String(count(status));
    bar.append(seg);
  }
  // Columns by how long each step waits: those in a column can run side by side.
  const byStep = new Map(steps.map((s) => [s.step, s]));
  const depth = new Map();
  const depthOf = (s, path = new Set()) => {
    if (depth.has(s.step)) return depth.get(s.step);
    if (path.has(s.step)) return 0;
    path.add(s.step);
    const deps = (s.after ?? []).map((d) => byStep.get(d)).filter(Boolean);
    const d = deps.length ? 1 + Math.max(...deps.map((x) => depthOf(x, path))) : 0;
    depth.set(s.step, d);
    return d;
  };
  const columns = [];
  for (const s of steps) (columns[depthOf(s)] ??= []).push(s);
  const graph = el("ol", { class: "plan-graph", "aria-label": "Its steps in the order they can run: each column waits for the one before" },
    ...columns.map((col) => el("li", { class: "plan-col" },
      ...col.map((s) => el("button", {
        type: "button", class: `step-chip ${s.status}`,
        title: s.progress?.text || s.prompt,
        "aria-label": `Step ${s.step}: ${STATUS_TEXT[s.status]} on ${s.target}${s.status === "running" && s.progress?.text ? `, ${s.progress.text}` : ""}. Show it`,
        onclick: () => showStep(s.id),
      }, el("span", { class: "dot", "aria-hidden": "true" }), el("span", { class: "mono", text: s.step }),
      el("span", { class: "chip-who", text: s.target }))))));
  const active = steps.find((s) => s.status === "running" || s.status === "waiting");
  const runId = steps.find((s) => s.run)?.run;
  const planIssue = steps.find((s) => s.plan_issue)?.plan_issue;
  const head = el("div", { class: "plan-head" },
    el("span", { class: "task-title", text: lead ? `Team run for ${lead}'s team step` : `Plan · started ${stamp(steps[0].started_ms)}` }),
    el("span", { class: "head-actions" },
      planIssue ? externalLink(planIssue.url, `Issue #${planIssue.number}`, `The plan's issue, #${planIssue.number}, on GitHub`, { class: "btn small" }) : null,
      runId ? el("button", { type: "button", class: "btn small", "aria-label": "Open the plan's evidence run", onclick: () => openRun(runId) }, "Evidence run") : null,
      active && !lead ? el("button", {
        type: "button", class: "btn small", "aria-label": "Cancel the plan", onclick: (e) => cancelTask(active, e.currentTarget),
      }, icon(ICONS.stop), "Cancel plan") : null));
  const open = openPlans.has(planId) ? openPlans.get(planId) : !ended;
  const details = el("details", { class: "plan-steps", open: open || undefined },
    el("summary", { text: `Steps (${steps.length})` }),
    el("ol", { class: "task-list" }, ...steps.map(taskNode)));
  details.addEventListener("toggle", () => openPlans.set(planId, details.open));
  return el("li", { class: `plan-group${ended ? " ended" : ""}`, "data-plan": planId },
    head, el("p", { class: "task-meta", text: lead ? `${words}. Your team runs it; ${lead} sees each step as it goes.` : words }),
    bar, graph, details);
}

function renderTasks() {
  const list = $("#task-list");
  list.replaceChildren();
  $("#task-count").textContent = state.tasks.length;
  $("#clear-tasks-btn").hidden = !state.tasks.some((t) => !["running", "waiting"].includes(t.status));
  if (!state.tasks.length) {
    list.append(el("li", { class: "empty" }, "No tasks yet. Choose ", el("strong", { text: "Run task" }),
      " below to run a prompt on this agent or a peer."));
  }
  // Newest first; a plan's steps together, where its newest step is.
  const shown = new Set();
  for (const t of [...state.tasks].reverse()) {
    if (t.plan == null) list.append(taskNode(t));
    else if (!shown.has(t.plan)) {
      shown.add(t.plan);
      list.append(planNode(t.plan, state.tasks.filter((s) => s.plan === t.plan)));
    }
  }
  renderSquads();
}
async function clearTasks() {
  const ok = await confirmDialog({
    title: "Clear finished tasks?",
    body: "Done, failed, cancelled and skipped tasks leave this list (a plan's steps once the whole plan has ended). Running tasks stay, and so do evidence runs.",
    confirm: "Clear finished",
    danger: true,
  });
  if (!ok) return;
  try {
    const res = await api("/api/tasks", { method: "DELETE" });
    state.tasks = res.tasks;
    renderTasks();
    // The button is gone once nothing is left to clear: focus stays in the panel.
    if ($("#clear-tasks-btn").hidden) $("#tasks-h").focus();
    toast(`${res.cleared} task${res.cleared === 1 ? "" : "s"} cleared`);
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- squads ----------
function renderSquads() {
  const list = $("#squad-list");
  list.replaceChildren();
  const squads = state.squads ?? [];
  if (!squads.length) {
    list.append(el("li", { class: "empty" }, "Group peers into squads, then give a squad steps in a ",
      el("strong", { text: "Plan" }), "."));
  }
  for (const s of squads) {
    const online = s.members.filter((m) => m.online).length;
    // What each online member takes (it says in its hello); offline ones say when they connect.
    const takes = s.members.map((m) => {
      const peer = m.online ? state.peers.find((p) => p.name === m.name) : null;
      return peer ? { peer, about: squadMember(peer, providerLabel) } : null;
    });
    const none = online > 0 && takes.every((t) => !t?.about.takes);
    list.append(el("li", { class: "squad" },
      el("div", { class: "who" },
        el("span", { class: "name", text: `#${s.name}` }),
        el("span", { class: "sub", text: `${online}/${s.members.length} online` }),
        el("ul", { class: "squad-members" }, ...s.members.map((m, i) => {
          const busy = state.tasks.filter((t) => t.status === "running" && t.target === m.name).length;
          const known = takes[i];
          const runs = known?.about.takes
            ? ` · ${known.peer.capabilities.map(providerLabel).join(", ") || "team steps"}` : "";
          return el("li", {},
            el("span", { class: `dot${m.online ? " on" : ""}`, "aria-hidden": "true" }),
            el("span", { text: m.name }),
            el("span", { class: "muted small", text: m.online ? `${busy ? ` · ${busy} running` : " · idle"}${runs}` : " · offline" }),
            known && !known.about.takes ? el("span", { class: "warn-text small", text: ` · takes no steps: ${known.about.why}` }) : null);
        })),
        none ? el("span", { class: "warn-text small", text: "No online member can take its steps now." }) : null),
      el("div", { class: "row-actions" },
        el("button", {
          type: "button", class: "btn small", "aria-label": `Change squad ${s.name}`,
          onclick: (e) => openSquadDialog(s, e.currentTarget),
        }, "Change…"),
        el("button", {
          type: "button", class: "icon-btn", title: `Remove squad ${s.name}`, "aria-label": `Remove squad ${s.name}`,
          onclick: () => removeSquad(s.name),
        }, icon(ICONS.trash)))));
  }
}

/** The steps of a team plan: its lines that are neither blank, comments nor claims, nor the continuation of a step's text. */
function teamStepCount(plan) {
  return planText.lineKinds(plan).filter(({ kind }) => kind === "step").length;
}

let teamFolder = null;

function openTeamDialog(share, opener) {
  teamFolder = share.name;
  const me = state.me.name;
  $("#team-h").textContent = `Team plan for ${share.name}`;
  $("#team-intro").textContent = `Peers with edit access hand this folder team steps (name team @${me}:${share.name} what to do). `
    + "This agent's team runs the plan on its own peers, digital workers and squads.";
  const plan = $("#team-plan");
  plan.value = share.team ?? "";
  plan.placeholder = `claim: T-1 {task} passes the tests\nbuild @alice:${share.name} verify=test claims=T-1 {task}`;
  // It opens on what the folder does now: its plan, or its model writing one.
  const byModel = Boolean(share.team_model);
  $("#team-by-owner").checked = !byModel;
  $("#team-by-model").checked = byModel;
  $("#team-approve").checked = Boolean(share.team_approval);
  showTeamBy();
  $("#team-error").textContent = "";
  $("#team-remove").hidden = !share.team && !byModel;
  $("#team-dialog").addEventListener("close", () => {
    if (opener.isConnected) opener.focus();
  }, { once: true });
  $("#team-dialog").showModal();
  (share.team ? plan : $(byModel ? "#team-by-model" : "#team-by-owner")).focus();
}

/** Shows only the fields of who writes the team plan: the plan for "I do", the approval for the model. */
function showTeamBy() {
  const byModel = $("#team-by-model").checked;
  $("#team-plan-field").hidden = byModel;
  $("#team-approve-field").hidden = !byModel;
}

/** What shared folder `s` does with team steps, for its line in the shares list (none: nothing). */
function teamLine(s) {
  if (s.team_model) {
    return s.team_approval ? "Takes team steps (its model writes the plan, you approve each)" : "Takes team steps (its model writes the plan)";
  }
  if (!s.team) return null;
  const n = teamStepCount(s.team);
  return `Takes team steps (${n} ${n === 1 ? "step" : "steps"})`;
}

/** Whether peers may end a plan with a pull request step in shared folder `s`: this agent
 * then pushes their tasks' branch and opens the pull request with its GitHub account,
 * each one once you approve it if you tick that too. */
function pullRequestsToggle(s) {
  const save = async (body, done, inputs) => {
    for (const i of inputs) i.disabled = true;
    try {
      await api(`/api/shares/${encodeURIComponent(s.name)}/pull-requests`, { method: "PUT", body: JSON.stringify(body) });
      toast(done);
      await loadState();
    } catch (err) {
      toast(err.message, "error");
      await loadState();
    } finally {
      for (const i of inputs) i.disabled = false;
    }
  };
  const input = el("input", { type: "checkbox", checked: Boolean(s.pull_requests) });
  const approve = el("input", { type: "checkbox", checked: Boolean(s.pull_request_approval) });
  input.addEventListener("change", () => {
    const on = input.checked;
    save({ on }, on ? `${s.name} takes pull request steps` : `${s.name} no longer takes pull request steps`, [input, approve]);
  });
  approve.addEventListener("change", () => {
    const approval = approve.checked;
    save({ on: true, approval }, approval ? `You approve each pull request in ${s.name} before it is opened`
      : `${s.name} opens pull requests without asking you`, [input, approve]);
  });
  return el("span", { class: "pr-toggles" },
    el("label", {
      class: "check small",
      title: "Peers with edit access may end a plan with a pull request step here: this agent pushes the branch of their tasks' work and opens the pull request with your GitHub account, then follows it until it is merged",
    }, input, el("span", { text: "Takes pull request steps" })),
    s.pull_requests
      ? el("label", {
        class: "check small",
        title: "Each request waits under Approvals until you approve it; the pull request says you did",
      }, approve, el("span", { text: "Ask me to approve each one" }))
      : null);
}

/// Runs a team plan request with `button` disabled and saying `busy`; on success closes
/// the dialog and refreshes the card, on an error shows the server's words and keeps the text.
async function sendTeamPlan(button, busy, request, done) {
  const buttons = [...$("#team-form").querySelectorAll("button")];
  const label = button.textContent;
  $("#team-error").textContent = "";
  for (const b of buttons) b.disabled = true;
  button.textContent = busy;
  const folder = teamFolder;
  try {
    await request(`/api/shares/${encodeURIComponent(folder)}/team`);
    $("#team-dialog").close();
    toast(done(folder));
    await loadState();
    // The card was rebuilt: focus the new button of the same folder.
    [...document.querySelectorAll("button[aria-label]")]
      .find((b) => b.getAttribute("aria-label") === `Team plan for ${folder}`)?.focus();
  } catch (err) {
    $("#team-error").textContent = err.message;
  } finally {
    for (const b of buttons) b.disabled = false;
    button.textContent = label;
  }
}

function submitTeamPlan(e) {
  e.preventDefault();
  const byModel = $("#team-by-model").checked;
  const approve = $("#team-approve").checked;
  return sendTeamPlan($("#team-save"), "Saving…",
    (url) => api(url, { method: "PUT", body: JSON.stringify(byModel ? { model: true, approve } : { plan: $("#team-plan").value }) }),
    (folder) => (byModel ? `Your model writes the team plans for ${folder}${approve ? "; you approve each before it runs" : ""}`
      : `Saved the team plan for ${folder}`));
}

async function removeTeamPlan() {
  const folder = teamFolder;
  const ok = await confirmDialog({
    title: `Remove the team plan for ${folder}?`,
    body: "Peers can no longer hand this folder team steps. The folder, its checks and its other settings stay.",
    confirm: "Remove team plan",
    danger: true,
  });
  if (!ok) return;
  await sendTeamPlan($("#team-remove"), "Removing…", (url) => api(url, { method: "DELETE" }),
    (name) => `Removed the team plan for ${name}`);
}

// The squad the dialog changes (null: a new one), and the button that opened it.
let squadEditing = null;
let squadOpener = null;

function openSquadDialog(squad = null, opener = $("#add-squad-btn")) {
  squadEditing = squad;
  squadOpener = opener;
  $("#squad-form").reset();
  $("#squad-error").textContent = "";
  $("#squad-h").textContent = squad ? `Change squad #${squad.name}` : "New squad";
  const name = $("#squad-name");
  name.value = squad?.name ?? "";
  name.readOnly = !!squad;
  const members = new Set((squad?.members ?? []).map((m) => m.name));
  // Each with what it takes: what it runs and where, or why no step can go to it.
  const check = (value, label, about, warn, i) => el("label", { class: "check squad-check" },
    el("input", {
      type: "checkbox", value, "aria-labelledby": `squad-member-${i}`, "aria-describedby": `squad-member-about-${i}`,
      ...(members.has(value) ? { checked: "" } : {}),
    }),
    el("span", {},
      el("span", { id: `squad-member-${i}`, text: label }),
      el("span", { id: `squad-member-about-${i}`, class: `small ${warn ? "warn-text" : "muted"}`, text: about })));
  // Members that left the allowlist stay listed, so they can be dropped.
  const gone = [...members].filter((m) => !state.trusted.some((t) => t.name === m));
  const box = $("#squad-members");
  box.replaceChildren(...(state.trusted.length || gone.length
    ? [...state.trusted.map((t, i) => {
      const peer = state.peers.find((p) => p.id === t.id);
      const about = peer ? squadMember(peer, providerLabel) : null;
      return check(t.name, t.name, about?.text ?? "offline: what it runs shows when it connects", about && !about.takes, i);
    }),
    ...gone.map((m, i) => check(m, m, "not on the allowlist: it takes no steps", true, state.trusted.length + i))]
    : [el("span", { class: "muted small", text: "Add peers to the allowlist first." })]));
  $("#squad-dialog").showModal();
  (squad ? box.querySelector("input") ?? name : name).focus();
}

async function submitSquad(e) {
  e.preventDefault();
  const name = $("#squad-name").value.trim();
  const members = [...document.querySelectorAll("#squad-members input:checked")].map((i) => i.value);
  if (!name || !members.length) {
    $("#squad-error").textContent = "Enter a name and pick at least one member.";
    return;
  }
  if (!squadEditing && (state.squads ?? []).some((s) => s.name === name)) {
    $("#squad-error").textContent = `There is already a squad #${name}: use Change… on it instead.`;
    return;
  }
  // A squad no step can go to: every member is online and takes none (offline ones may).
  const idle = members.map((m) => state.peers.find((p) => p.name === m))
    .filter((p) => p && !squadMember(p).takes);
  if (idle.length === members.length) {
    $("#squad-error").textContent = `No step could go to it: ${idle.map((p) => p.name).join(", ")} `
      + `${idle.length === 1 ? "runs" : "run"} no models in a folder shared with you. Pick a member that does, or leads a team plan.`;
    return;
  }
  try {
    await api("/api/squads", { method: "POST", body: JSON.stringify({ name, members }) });
    $("#squad-dialog").close();
    toast(`Squad #${name} saved`);
  } catch (err) {
    $("#squad-error").textContent = err.message;
  }
}

async function removeSquad(name) {
  const ok = await confirmDialog({
    title: `Remove squad #${name}?`,
    body: "Plans that name it will no longer run. Its members stay on the allowlist.",
    confirm: "Remove squad",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/squads/${encodeURIComponent(name)}`, { method: "DELETE" });
    toast(`Squad #${name} removed`);
  } catch (err) {
    toast(err.message, "error");
  }
}

function upsertTask(task) {
  const i = state.tasks.findIndex((t) => t.id === task.id);
  const isNew = i < 0;
  if (isNew) state.tasks.push(task);
  else state.tasks[i] = task;
  renderTasks();
  if (!isNew && !["running", "waiting"].includes(task.status)) {
    const verb = { done: "finished", failed: "failed", cancelled: "cancelled", skipped: "skipped" }[task.status];
    const label = task.step ? `Step ${task.step}` : `Task #${task.id}`;
    toast(`${label} ${verb}`, task.status === "failed" ? "error" : "");
  }
}

async function cancelTask(t, button) {
  button.disabled = true;
  try {
    await api(`/api/tasks/${t.id}`, { method: "DELETE" });
  } catch (err) {
    button.disabled = false;
    toast(err.message, "error");
  }
}

// ---------- approvals ----------
function renderApprovals() {
  const list = $("#approvals-list");
  const approvals = state.approvals ?? [];
  $("#approvals-panel").hidden = !approvals.length;
  $("#approvals-count").textContent = approvals.length;
  list.replaceChildren(...approvals.map((a) => {
    const note = el("input", {
      type: "text", class: "approval-note", maxlength: "500", "aria-label": `Note for request ${a.id}`,
      placeholder: a.changes ? "Note (required to ask for changes or reject)" : "Note (required to reject)",
    });
    const error = el("p", { class: "form-error", role: "alert" });
    // approve: true, "changes" or false (reject).
    const answer = async (how, button) => {
      if (how !== true && !note.value.trim()) {
        error.textContent = how === "changes" ? "Say what to change." : "Say why you reject it.";
        note.focus();
        return;
      }
      button.disabled = true;
      try {
        await api(`/api/approvals/${a.id}`, {
          method: "POST",
          body: JSON.stringify({ approved: how === true, changes: how === "changes", note: note.value }),
        });
        toast(how === true ? "Approved" : how === "changes" ? "Asked for changes: the work is done again, then you're asked again" : "Rejected");
      } catch (err) {
        button.disabled = false;
        error.textContent = err.message;
      }
    };
    return el("li", { class: "task approval" },
      el("div", { class: "task-top" },
        el("span", { class: "task-title", text: `${a.title} · from ${a.from}` }),
        el("span", { class: "status waiting", text: "Needs you" })),
      el("details", {}, el("summary", { text: "What you're approving" }),
        el("div", { class: "result", tabindex: "0" }, renderMarkdown(a.text))),
      a.preview ? el("div", { class: "task-actions" },
        el("button", { type: "button", class: "btn small", onclick: (e) => openPreview({ approval: a.id }, e.currentTarget) }, previewText(a.preview)),
        el("span", {
          class: "task-meta",
          text: a.preview === "review pack"
            ? "Read the decks and design documents, and click through the prototype, before you answer"
            : "Click through the UX prototype before you answer",
        })) : null,
      note,
      error,
      el("div", { class: "approval-actions" },
        el("button", { type: "button", class: "btn primary small", onclick: (e) => answer(true, e.currentTarget) }, "Approve"),
        a.changes
          ? el("button", {
            type: "button", class: "btn small",
            title: "The work is done again with your note, then you're asked again",
            onclick: (e) => answer("changes", e.currentTarget),
          }, "Ask for changes")
          : null,
        el("button", { type: "button", class: "btn small danger", onclick: (e) => answer(false, e.currentTarget) }, "Reject")));
  }));
}

// ---------- evidence runs ----------
async function loadRuns() {
  const res = await api("/api/runs");
  const list = $("#runs-list");
  $("#runs-count").textContent = res.runs.length;
  list.replaceChildren();
  if (!res.runs.length) {
    list.append(el("li", { class: "empty" }, "No runs yet. Run a ", el("strong", { text: "Plan" }),
      " with claim: lines and verify= checks to collect evidence."));
  }
  for (const r of res.runs.slice(0, 20)) {
    const verdict = r.claims ? (r.passed ? "Pass" : "Fail") : "No claims";
    list.append(el("li", { class: "run-item" },
      el("button", { type: "button", class: "run-open", onclick: () => openRun(r.id), "aria-label": `Open evidence run ${r.id}: ${r.title}`, title: `Evidence run ${r.id}` },
        el("span", { class: "task-title", text: r.title || r.id }),
        el("span", { class: `status ${r.passed ? "done" : r.claims ? "failed" : "skipped"}`, text: verdict })),
      el("div", { class: "task-meta", text: `${stamp(r.started_unix * 1000)} · ${r.proven}/${r.claims} claims proven · ${r.steps_done}/${r.steps} steps done` })));
  }
}

// ---------- schedules ----------
const relative = new Intl.RelativeTimeFormat(undefined, { numeric: "auto" });
const andList = new Intl.ListFormat(undefined, { type: "conjunction" });
const DAY_KEYS = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
const LAST_STATUS = {
  running: ["running", "Running"],
  passed: ["done", "Passed"],
  failed: ["failed", "Failed"],
  "no-claims": ["skipped", "No claims"],
  "not-run": ["cancelled", "Didn't start"],
  missed: ["failed", "Missed"],
  cancelled: ["cancelled", "Cancelled"],
};

/// An absolute time in the browser's locale: "today 09:00", "Tue, Mar 4 09:00".
function when(unix) {
  if (!unix) return "";
  const d = new Date(unix * 1000);
  const time = d.toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
  if (d.toDateString() === new Date().toDateString()) return `today ${time}`;
  return `${d.toLocaleDateString([], { weekday: "short", month: "short", day: "numeric" })} ${time}`;
}

/// A time relative to now: "in 3 hours", "5 minutes ago".
function fromNow(unix) {
  const secs = unix - Date.now() / 1000;
  const abs = Math.abs(secs);
  const [value, unit] = abs < 45 ? [secs, "second"] : abs < 2700 ? [secs / 60, "minute"]
    : abs < 64800 ? [secs / 3600, "hour"] : [secs / 86400, "day"];
  return relative.format(Math.round(value), unit);
}

function timeEl(unix, text) {
  return el("time", { datetime: new Date(unix * 1000).toISOString(), title: when(unix), text });
}

function dayName(key, style = "short") {
  // 1 January 2024 was a Monday.
  return new Date(2024, 0, 1 + DAY_KEYS.indexOf(key)).toLocaleDateString([], { weekday: style });
}

function timeText(hhmm) {
  const [h, m] = hhmm.split(":").map(Number);
  return new Date(2024, 0, 1, h, m).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" });
}

/// When a schedule runs, in plain words: "Every 2 hours", "Weekdays at 08:30".
function describeWhen(parts, raw) {
  if (!parts) return raw;
  if (parts.every_minutes) {
    const m = parts.every_minutes;
    const [n, unit] = m % 1440 === 0 ? [m / 1440, "day"] : m % 60 === 0 ? [m / 60, "hour"] : [m, "minute"];
    return n === 1 ? `Every ${unit}` : `Every ${n} ${unit}s`;
  }
  const days = [...(parts.days ?? [])].sort((a, b) => DAY_KEYS.indexOf(a) - DAY_KEYS.indexOf(b));
  const at = timeText(parts.time);
  const key = days.join(",");
  if (!days.length) return `Every day at ${at}`;
  if (key === "mon,tue,wed,thu,fri") return `Weekdays at ${at}`;
  if (key === "sat,sun") return `Weekends at ${at}`;
  return `${andList.format(days.map((d) => dayName(d, "long")))} at ${at}`;
}

async function loadSchedules() {
  renderSchedules((await api("/api/schedules")).schedules);
}

function renderSchedules(schedules) {
  const list = $("#schedule-list");
  $("#schedules-count").textContent = schedules.length;
  list.replaceChildren();
  if (!schedules.length) {
    list.append(el("li", { class: "empty" }, "None yet. Write or pick a plan in ", el("strong", { text: "Plan" }),
      " mode, then press ", el("strong", { text: "Schedule…" }), " to run it on its own, e.g. every weekday at 09:00."));
  }
  const act = async (button, path, init, done) => {
    button.disabled = true;
    try {
      renderSchedules((await api(path, init)).schedules);
      if (done) toast(done);
    } catch (err) {
      button.disabled = false;
      toast(err.message, "error");
    }
  };
  for (const s of schedules) {
    const status = s.running ? ["running", "Running"] : s.enabled ? ["done", "Active"] : ["skipped", "Paused"];
    const path = `/api/schedules/${encodeURIComponent(s.name)}`;
    const meta = [describeWhen(s.when_parts, s.when), `${s.steps} step${s.steps === 1 ? "" : "s"}`, s.source || null]
      .filter(Boolean).join(" · ");
    const next = s.enabled && s.next_unix && !s.running
      ? el("div", { class: "task-meta" }, "Next run ", timeEl(s.next_unix, fromNow(s.next_unix)), ` · ${when(s.next_unix)}`)
      : null;
    let last = null;
    let why = null;
    if (s.last_status && s.last_status !== "running") {
      const [cls, label] = LAST_STATUS[s.last_status] ?? ["skipped", s.last_status];
      last = el("div", { class: "last-run" },
        el("span", { class: "muted small", text: "Last run" }),
        el("span", { class: `status ${cls}`, text: label }),
        s.last_run_unix ? el("span", { class: "muted small" }, timeEl(s.last_run_unix, fromNow(s.last_run_unix))) : null,
        s.last_run_id ? el("button", { type: "button", class: "btn link small", onclick: () => openRun(s.last_run_id) }, "Open run") : null);
      const detail = (s.last_result ?? "").replace(/^(not run yet \(trying again\)|missed|cancelled):\s*/, "");
      why = s.retry_unix
        ? el("div", { class: "task-meta warn-text" }, "Trying again ", timeEl(s.retry_unix, fromNow(s.retry_unix)), `: ${detail}`)
        : detail ? el("div", { class: "task-meta", text: detail }) : null;
    }
    list.append(el("li", { class: "task schedule" },
      el("div", { class: "task-top" },
        el("span", { class: "task-title", text: s.name }),
        el("span", { class: `status ${status[0]}`, text: status[1] })),
      el("div", { class: "task-meta", text: meta }),
      next, last, why,
      el("details", {}, el("summary", { text: "Plan" }), el("pre", { text: s.plan })),
      el("div", { class: "task-actions" },
        el("button", {
          type: "button", class: "btn small", disabled: s.running, title: "Run it now, once",
          onclick: (e) => act(e.currentTarget, `${path}/run`, { method: "POST" }, `Started '${s.name}'`),
        }, "Run now"),
        el("button", {
          type: "button", class: "btn small",
          title: s.enabled ? "Stop running it until you resume it" : "Run it again; its time counts from now",
          onclick: (e) => act(e.currentTarget, path, { method: "PATCH", body: JSON.stringify({ enabled: !s.enabled }) },
            s.enabled ? `Paused '${s.name}'` : `Resumed '${s.name}'`),
        }, s.enabled ? "Pause" : "Resume"),
        el("button", { type: "button", class: "btn small", onclick: () => openScheduleDialog(s) }, "Edit"),
        el("button", {
          type: "button", class: "btn small danger",
          onclick: async (e) => {
            const button = e.currentTarget;
            const ok = await confirmDialog({
              title: `Delete schedule ${s.name}?`,
              body: "It stops running. Its past runs stay under Evidence runs.",
              confirm: "Delete schedule",
              danger: true,
            });
            if (ok) act(button, path, { method: "DELETE" }, `Deleted '${s.name}'`);
          },
        }, "Delete"))));
  }
}

let scheduleEditing = null;

function buildDayToggles() {
  const box = $("#when-days");
  if (box.childElementCount) return;
  box.append(...DAY_KEYS.map((d) => el("label", { class: "day", title: dayName(d, "long") },
    el("input", { type: "checkbox", value: d }), el("span", { text: dayName(d) }))));
}

function syncWhenPicker() {
  const repeat = $("#when-repeat").value;
  $("#when-time-field").hidden = repeat === "interval" || repeat === "custom";
  $("#when-interval-field").hidden = repeat !== "interval";
  $("#when-days").hidden = repeat !== "days";
  $("#when-custom-field").hidden = repeat !== "custom";
  previewWhen();
}

/// The schedule's `when` from the picker ("" while incomplete).
function whenFromPicker() {
  const repeat = $("#when-repeat").value;
  const time = $("#when-time").value;
  if (repeat === "interval") {
    const n = Number($("#when-every").value);
    return Number.isInteger(n) && n > 0 ? `every ${n}${$("#when-unit").value}` : "";
  }
  if (repeat === "custom") return $("#when-custom").value.trim();
  if (repeat === "days") {
    const days = [...$("#when-days").querySelectorAll("input:checked")].map((i) => i.value);
    return days.length && time ? `${days.join(",")} ${time}` : "";
  }
  return time ? `${repeat} ${time}` : "";
}

/// Sets the picker from a schedule's `when` and its parts, as the agent read them.
function setWhenPicker(text, parts) {
  buildDayToggles();
  for (const box of $("#when-days").querySelectorAll("input")) box.checked = false;
  $("#when-custom").value = text ?? "";
  if (!parts) {
    $("#when-repeat").value = text ? "custom" : "daily";
  } else if (parts.every_minutes) {
    const m = parts.every_minutes;
    const [n, unit] = m % 1440 === 0 ? [m / 1440, "d"] : m % 60 === 0 ? [m / 60, "h"] : [m, "m"];
    $("#when-repeat").value = "interval";
    $("#when-every").value = String(n);
    $("#when-unit").value = unit;
  } else {
    const key = parts.days.join(",");
    $("#when-time").value = parts.time;
    $("#when-repeat").value = !parts.days.length ? "daily" : key === "mon,tue,wed,thu,fri" ? "weekdays"
      : key === "sat,sun" ? "weekends" : "days";
    for (const box of $("#when-days").querySelectorAll("input")) box.checked = parts.days.includes(box.value);
  }
  syncWhenPicker();
}

let whenTimer = null;
/// Shows, as the picker changes, what the schedule means and its next runs.
function previewWhen() {
  clearTimeout(whenTimer);
  const out = $("#when-preview");
  const text = whenFromPicker();
  if (!text) {
    out.dataset.state = "bad";
    out.textContent = $("#when-repeat").value === "days" ? "Pick at least one day." : "";
    return;
  }
  whenTimer = setTimeout(async () => {
    try {
      const res = await api("/api/schedule-preview", { method: "POST", body: JSON.stringify({ when: text }) });
      if (whenFromPicker() !== text) return;
      out.dataset.state = res.ok ? "ok" : "bad";
      out.textContent = res.ok
        ? `${describeWhen(res.parts, text)}. Next runs: ${res.next.map(when).join(" · ")}`
        : res.error;
    } catch (err) {
      out.dataset.state = "bad";
      out.textContent = err.message;
    }
  }, 250);
}

/// Schedules the plan in the composer, or (`edit`) changes an existing schedule.
function openScheduleDialog(edit = null) {
  const text = $("#composer-input").value.trim();
  if (!edit && !text) {
    toast("Write or pick a plan first, then schedule it", "error");
    $("#composer-input").focus();
    return;
  }
  scheduleEditing = edit;
  $("#schedule-error").textContent = "";
  $("#schedule-h").textContent = edit ? `Edit schedule ${edit.name}` : "Schedule this plan";
  $("#schedule-submit").textContent = edit ? "Save changes" : "Schedule";
  const name = $("#schedule-name");
  if (edit) {
    name.value = edit.name;
    name.dataset.edit = "1";
  } else if (name.dataset.edit) {
    name.value = "";
    delete name.dataset.edit;
  }
  name.disabled = Boolean(edit);
  if (!edit && !name.value && state.playbook) name.value = state.playbook;
  $("#schedule-replan-field").hidden = !(edit && text && text !== edit.plan.trim());
  $("#schedule-replan").checked = false;
  if (edit) setWhenPicker(edit.when, edit.when_parts);
  else {
    buildDayToggles();
    syncWhenPicker();
  }
  $("#schedule-dialog").showModal();
  (edit || name.value ? $("#when-repeat") : name).focus();
}

async function submitSchedule(e) {
  e.preventDefault();
  const error = $("#schedule-error");
  const name = $("#schedule-name").value.trim();
  const at = whenFromPicker();
  if (!scheduleEditing && !name) {
    error.textContent = "Give the schedule a name.";
    $("#schedule-name").focus();
    return;
  }
  if (!at) {
    error.textContent = $("#when-repeat").value === "days" ? "Pick at least one day." : "Say when it runs.";
    $("#when-repeat").focus();
    return;
  }
  const button = $("#schedule-submit");
  button.disabled = true;
  try {
    const editing = scheduleEditing;
    let res;
    if (editing) {
      const body = { when: at };
      if ($("#schedule-replan").checked) body.plan = $("#composer-input").value;
      res = await api(`/api/schedules/${encodeURIComponent(editing.name)}`, { method: "PUT", body: JSON.stringify(body) });
    } else {
      const book = state.playbook;
      res = await api("/api/schedules", {
        method: "POST",
        body: JSON.stringify({ name, when: at, plan: $("#composer-input").value, source: book ? `playbook ${book}` : "" }),
      });
    }
    renderSchedules(res.schedules);
    const saved = res.schedules.find((s) => s.name === (editing?.name ?? name));
    $("#schedule-dialog").close();
    toast(`${editing ? "Saved" : "Scheduled"} '${saved?.name ?? name}'${saved?.next_unix ? `; next run ${when(saved.next_unix)}` : ""}`);
  } catch (err) {
    error.textContent = err.message;
  } finally {
    button.disabled = false;
  }
}

async function openRun(id) {
  try {
    const run = await api(`/api/runs/${encodeURIComponent(id)}`);
    $("#run-h").textContent = `Evidence run ${id}`;
    $("#run-download").hidden = false;
    $("#run-download").onclick = (e) => downloadRun(id, e.currentTarget);
    $("#run-verdict").textContent = run.verdict;
    $("#run-verdict").dataset.pass = String(run.summary.passed);
    const tracked = run.tracked;
    $("#run-tracked").hidden = !tracked?.issue;
    $("#run-tracked").replaceChildren(...(tracked?.issue ? [
      "Tracked on GitHub as ",
      externalLink(tracked.issue.url, `issue #${tracked.issue.number}`),
      ...Object.entries(tracked.steps).flatMap(([step, issue], i) => [i ? ", " : ", its steps as ",
        externalLink(issue.url, `#${issue.number}`, `Step ${step}'s issue, #${issue.number}`), ` (${step})`]),
      tracked.problems?.length ? ` · ${nOf(tracked.problems.length, "tracking problem")}: ${tracked.problems.join("; ")}` : "",
    ] : []));
    $("#run-signatures").replaceChildren(...run.signatures.map((s) => el("li", { class: s.ok ? "ok" : "bad" },
      el("strong", { text: `${s.ok ? "✓" : "✗"} ${s.step}: ` }), s.text)));
    $("#run-prototypes").replaceChildren(...(run.previews ?? []).map((p) =>
      el("button", { type: "button", class: "btn small", onclick: (e) => openPreview({ run: id, step: p.step }, e.currentTarget) },
        `${previewText(p.label)} of step ${p.step}`)));
    renderRunPulls(id, run.pull_requests ?? []);
    renderRunWorkiq(run.workiq ?? []);
    $("#run-report").replaceChildren(renderMarkdown(run.report));
    $("#run-dialog").showModal();
    $("#run-close").focus();
  } catch (err) {
    toast(err.message, "error");
  }
}

/** Saves run `id`'s run.json: anyone can re-check its signatures with
 * `proofkin runs verify --file`, without trusting this agent. */
async function downloadRun(id, button) {
  button.disabled = true;
  try {
    const res = await fetch(`/api/runs/${encodeURIComponent(id)}/file`, { headers: { Authorization: `Bearer ${token}` } });
    if (res.status === 401) {
      showAuthError();
      throw new Error("Session ended: open the new link printed by Proofkin");
    }
    if (!res.ok) throw new Error((await res.json().catch(() => ({}))).error || `${res.status} ${res.statusText}`);
    const url = URL.createObjectURL(await res.blob());
    const link = el("a", { href: url, download: `${id}.json`, hidden: true });
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 10_000);
    toast(`Saved ${id}.json; anyone can check it with: proofkin runs verify --file ${id}.json`);
  } catch (err) {
    toast(err.message, "error");
  } finally {
    button.disabled = false;
  }
}

/** The work of run `runId` a pull request can be opened from (`works`, see
 * GET /api/runs/ID): a button for each, or why it can't be now, and one to go on from it. */
function renderRunPulls(runId, works) {
  const box = $("#run-pulls");
  box.hidden = !works.length;
  box.replaceChildren(...(works.length ? [el("h3", { text: "Its work" })] : []),
    ...works.map((w) => el("div", { class: "run-pull" },
      el("button", {
        type: "button", class: "btn small", disabled: Boolean(w.why),
        "aria-label": `Open a pull request of step ${w.step}'s work`,
        onclick: (e) => openPullDialog(runId, w, e.currentTarget),
      }, "Open pull request…"),
      el("button", {
        type: "button", class: "btn small",
        "aria-label": `Plan new work that starts from step ${w.step}'s work`,
        title: "A plan whose step starts from this work, merged or not (base=)",
        onclick: () => planFromWork(runId, w),
      }, "Build on it…"),
      el("span", { class: "muted small" }, `Step ${w.step}: branch `, el("span", { class: "mono", text: w.branch }),
        ` of ${w.peer}'s clone of `, el("span", { class: "mono", text: w.folder }),
        w.why ? `; no pull request now: ${w.why}.` : w.approval ? `; ${w.peer} approves a pull request first.` : "."))));
}

/** How many Microsoft 365 results were kept, with their labels, and how many were withheld
 * ("3 results kept (General), 2 withheld"). */
function workiqCounts(kept, labels, withheld) {
  const named = labels?.length ? ` (${labels.join(", ")})` : "";
  return `${nOf(kept, "result")} kept${named}, ${withheld ? `${withheld} withheld` : "none withheld"}`;
}

/** `link` if it is a web address (http or https), else null: a source's link is what Work IQ
 * said, so nothing else (a script, or a Microsoft Graph path) becomes a link. */
function webAddressOf(link) {
  if (!link) return null;
  try {
    const url = new URL(link);
    return url.protocol === "https:" || url.protocol === "http:" ? url.href : null;
  } catch {
    return null;
  }
}

/** Where the Microsoft 365 work context of a run's steps came from (`steps`, see
 * GET /api/runs/ID): for each step that had some, how many results were kept, with their
 * labels, and withheld, and its sources. Never what they said: the run doesn't answer it. */
function renderRunWorkiq(steps) {
  const box = $("#run-workiq");
  box.hidden = !steps.length;
  box.replaceChildren(...(steps.length ? [
    el("h3", { text: "Microsoft 365 work context" }),
    el("p", { class: "muted small", text: "Where each step's work context came from. What it said isn't shown here." }),
  ] : []), ...steps.map((w) => el("div", { class: "run-workiq-step" },
    el("p", { class: "small" }, el("strong", { text: `Step ${w.step}` }),
      ` · Microsoft 365: ${workiqCounts(w.kept, w.labels, w.withheld)}`),
    w.sources?.length ? el("ul", { class: "run-workiq-sources", "aria-label": `Step ${w.step}'s sources` },
      ...w.sources.map((s) => {
        const title = s.title || "Untitled";
        const href = webAddressOf(s.link);
        const about = [s.kind, s.label, href ? null : s.link].filter(Boolean).join(" · ");
        return el("li", {},
          href ? externalLink(href, title, `${title} in Microsoft 365`, { rel: "noopener noreferrer" }) : el("span", { text: title }),
          about ? el("span", { class: "muted", text: ` · ${about}` }) : null);
      })) : null)));
}

/** A plan whose step starts from the work of step `work.step` of run `runId` (base=),
 * merged or not: you say what to do. */
function planFromWork(runId, work) {
  const todo = "What to do";
  const line = `next @${work.peer}:${work.folder} base=${runId}:${work.step} ${todo}`;
  $("#run-dialog").close();
  document.querySelector('[data-mode="plan"]')?.click();
  const input = $("#composer-input");
  input.value = line;
  state.playbook = null;
  renderPlanExplain();
  schedulePlanCheck();
  input.focus();
  input.setSelectionRange(line.length - todo.length, line.length);
  toast(`A plan that starts from step ${work.step}'s work: say what to do next, then run it`);
}

// The run and its work the pull request dialog asks to open, and what opened it.
let pullAsk = null;

function openPullDialog(runId, work, opener) {
  pullAsk = { runId, work };
  $("#pull-form").reset();
  $("#pull-h").textContent = `Open a pull request of step ${work.step}`;
  $("#pull-intro").replaceChildren(`${work.peer}'s agent pushes branch `, el("span", { class: "mono", text: work.branch }),
    " (commit ", el("span", { class: "mono", text: work.commit }), ") of its clone of ", el("span", { class: "mono", text: work.folder }),
    ` and opens the pull request with ${work.peer}'s GitHub account, with the run's sign-offs on the work`
    + `${work.approval ? `, once ${work.peer} approves it` : ""}. You follow it under Tasks until it is merged.`);
  $("#pull-title").value = work.title;
  $("#pull-error").textContent = "";
  $("#pull-dialog").addEventListener("close", () => {
    if (opener.isConnected) opener.focus();
  }, { once: true });
  $("#pull-dialog").showModal();
  $("#pull-title").focus();
}

async function submitPull(e) {
  e.preventDefault();
  if (!pullAsk) return;
  const { runId, work } = pullAsk;
  const title = $("#pull-title").value.trim();
  if (!title) {
    $("#pull-error").textContent = "Give the pull request a title.";
    $("#pull-title").focus();
    return;
  }
  const button = $("#pull-submit");
  button.disabled = true;
  try {
    const res = await api(`/api/runs/${encodeURIComponent(runId)}/pull-request`, {
      method: "POST",
      body: JSON.stringify({ step: work.step, title, reviewers: $("#pull-reviewers").value.trim() || null }),
    });
    $("#pull-dialog").close();
    $("#run-dialog").close();
    const asked = res.reviewers?.length ? `; it asks ${res.reviewers.join(", ")} to review it` : "";
    toast(`Asked ${work.peer} to open the pull request${res.approval ? ` once ${work.peer} approves it` : ""}${asked}. Follow it under Tasks.`);
  } catch (err) {
    $("#pull-error").textContent = err.message;
  } finally {
    button.disabled = false;
  }
}

// ---------- playbooks ----------
let playbooks = [];
/** The playbook picker: the playbook picked, and its step (pick, then fill in). */
const playbookPicker = { name: null, step: "pick" };

async function loadPlaybooks() {
  playbooks = (await api("/api/playbooks")).playbooks;
  renderPlanExplain();
}

const nOf = (n, word) => `${n} ${word}${n === 1 ? "" : "s"}`;

const PLAN_INTRO = [
  "**A plan runs several agent tasks on your peers, in order, and keeps evidence of what was proven.**",
  "",
  "Press **Start from a playbook…** for a ready-made one, or write your own, one step per line:",
  "",
  "- `claim: C-1 The parser handles empty input`: what must be true at the end",
  "- `fix @alice:app skill=implement-change verify=test claims=C-1 Make the parser accept empty input`: step `fix` runs on peer alice in her `app` folder; afterwards alice's runtime runs her `test` check, and the step only passes if it does",
  "- `review after fix approve @me claims=C-1 Approve the fix`: after `fix`, you sign off",
  "",
  "Other options: `attempts=3` (retry with the failures), `apply=fix` (another peer checks the commit independently), `context=` / `save=` (knowledge), `#squad` (any member of a squad). A claim is proven only by passing checks or a sign-off; the run is saved under **Evidence runs**.",
].join("\n");

/// What a playbook does, proves and needs, as Markdown.
function playbookText(p) {
  const o = p.outline;
  return [
    `**${p.name}: ${p.description}**`,
    "",
    "What it does, step by step:",
    ...o.steps.map((s, i) => `${i + 1}. ${s}`),
    "",
    "What it proves (only with passing checks or a sign-off):",
    ...o.claims.map(([id, claim]) => `- ${id}: ${claim}`),
    "",
    "What you need:",
    ...o.needs.map((n) => `- ${n}`),
  ].join("\n");
}

/// Explains the playbook the plan came from (or what a plan is) above the plan box.
function renderPlanExplain() {
  const box = $("#plan-explain");
  if (state.mode !== "plan") {
    box.hidden = true;
    return;
  }
  const p = playbooks.find((x) => x.name === state.playbook);
  const text = p ? playbookText(p) : !$("#composer-input").value.trim() ? PLAN_INTRO : null;
  box.hidden = !text;
  if (text) {
    // Collapsed unless its owner opened it (remembered): the plan box stays in view.
    const details = el("details", { open: localStorage.getItem(EXPLAIN_KEY) === "open" },
      el("summary", { text: p ? `From the ${p.name} playbook: what it does` : "What is a plan?" }), renderMarkdown(text));
    details.addEventListener("toggle", () => localStorage.setItem(EXPLAIN_KEY, details.open ? "open" : "closed"));
    box.replaceChildren(details);
  }
}
const EXPLAIN_KEY = "p2pagents-plan-explain";

const CHECK_VARS = new Set(["build", "test", "lint", "secscan", "licenses", "secrets"]);
const APPROVER_VARS = new Set(["approver", "privacy_approver", "product_owner", "tech_lead", "security_champion", "leader"]);
const VAR_HELP = {
  peer: "The peer that does the work (connected peers that run tasks)",
  folder: "A folder that peer shares with you",
  reviewers: "A squad whose members review independently",
  approver: "Who signs off",
  privacy_approver: "Who accepts the privacy review (data dictionary and data flows)",
  product_owner: "Who accepts the spec, the prototype and the product review",
  tech_lead: "Who accepts the API contract and the technical review",
  security_champion: "Who accepts the threat model",
  leader: "Who accepts the plan in the leadership review",
  change: "What to change, in a sentence",
  change_id: "A short id for this change (letters, digits, -)",
  project: "The project's name",
  spec: "The feature's folder on this computer: specs/NNN-name in a Spec Kit project, or openspec/changes/<id> (a project folder with one feature works too)",
};

/// The playbook picker's step: "pick" (the list and what each does) or "fill" (who and where).
function setPlaybookStep(step) {
  playbookPicker.step = step;
  const p = playbooks.find((x) => x.name === playbookPicker.name);
  $("#playbook-step-pick").hidden = step !== "pick";
  $("#playbook-step-fill").hidden = step !== "fill";
  $("#playbook-back").hidden = step !== "fill";
  $("#playbook-submit").textContent = step === "pick" ? "Next: who and where" : "Fill in the plan";
  $("#playbook-h").textContent = step === "pick" ? "Start from a playbook" : `${p?.name ?? "Playbook"}: who and where`;
  $("#playbook-desc").textContent = step === "pick"
    ? "A playbook is a ready-made plan for one stage of the lifecycle. Pick one to see what it does, then say who does the work and where."
    : `${p?.description ?? ""}. The plan is checked before it runs.`;
  $("#playbook-error").textContent = "";
}

/// Shows what playbook `name` does, proves and needs, beside the list.
function showPlaybookDetail(name) {
  const p = playbooks.find((x) => x.name === name);
  if (!p) return;
  playbookPicker.name = name;
  const o = p.outline;
  $("#playbook-detail").replaceChildren(
    el("h3", { text: p.name }),
    el("p", { class: "muted", text: p.description }),
    el("h4", { text: "What it does" }),
    el("ol", {}, ...o.steps.map((s) => el("li", {}, inlineMd(s)))),
    el("h4", { text: "What it proves" }),
    el("ul", {}, ...o.claims.map(([id, claim]) => el("li", {}, el("strong", { text: `${id} ` }), inlineMd(claim)))),
    el("h4", { text: "What you need" }),
    el("ul", {}, ...o.needs.map((n) => el("li", {}, inlineMd(n)))));
}

/// Opens the playbook picker: every playbook with what it is for, and the details of
/// the one picked; next, who does the work and where.
function openPlaybookPicker() {
  if (!playbooks.length) {
    toast("The playbooks are still loading; try again in a moment", "error");
    return;
  }
  const want = playbooks.some((x) => x.name === state.playbook) ? state.playbook
    : playbooks.some((x) => x.name === playbookPicker.name) ? playbookPicker.name : playbooks[0].name;
  $("#playbook-choices").replaceChildren(...playbooks.map((p) => {
    const radio = el("input", { type: "radio", name: "playbook", value: p.name, checked: p.name === want });
    radio.addEventListener("change", () => showPlaybookDetail(p.name));
    return el("label", { class: "pick-card" }, radio,
      el("span", { class: "pick-body" },
        el("span", { class: "pick-name", text: p.name }),
        el("span", { class: "pick-desc", text: p.description }),
        el("span", { class: "pick-meta", text: `${nOf(p.outline.steps.length, "step")} · proves ${nOf(p.outline.claims.length, "claim")}` })));
  }));
  showPlaybookDetail(want);
  setPlaybookStep("pick");
  $("#playbook-dialog").returnValue = "";
  $("#playbook-dialog").showModal();
  $("#playbook-choices input:checked")?.focus();
}

/// The values playbook `name` needs: peers, their folders and checks, approvers, squads.
function renderPlaybookVars(name) {
  const p = playbooks.find((x) => x.name === name);
  playbookPicker.name = name;
  $("#playbook-replaces").hidden = !$("#composer-input").value.trim();
  const workers = state.peers.filter((x) => x.capabilities.length);
  const select = (name, options, value) => {
    const s = el("select", { name }, ...options.map(([v, label]) => el("option", { value: v, text: label })));
    if (options.some(([v]) => v === value)) s.value = value;
    return s;
  };
  const fields = {};
  const box = $("#playbook-vars");
  box.replaceChildren();
  const peerShares = () => workers.find((w) => w.name === fields.peer?.value)?.shares ?? [];
  const folderChecks = () => peerShares().find((s) => s.name === fields.folder?.value)?.checks ?? [];
  const refreshFolder = () => {
    if (!(fields.folder instanceof HTMLSelectElement)) return;
    const shares = peerShares();
    const want = fields.folder.value || p.vars.find((v) => v.name === "folder")?.default;
    fields.folder.replaceChildren(...(shares.length
      ? shares.map((s) => el("option", { value: s.name, text: `${s.name} (${ACCESS_SHORT[s.access] ?? s.access}${s.worktrees ? ", worktrees" : ""}${s.checks?.length ? `, checks: ${s.checks.join(", ")}` : ""}${s.mcp?.length ? `, MCP: ${s.mcp.join(", ")}` : ""})` }))
      : [el("option", { value: "", text: "(this peer shares no folder with you)" })]));
    if (shares.some((s) => s.name === want)) fields.folder.value = want;
    refreshChecks();
  };
  const refreshChecks = () => {
    const checks = folderChecks();
    for (const [name, field] of Object.entries(fields)) {
      if (!CHECK_VARS.has(name)) continue;
      const want = checks.includes(name) ? name : field.value;
      field.replaceChildren(...(checks.length
        ? checks.map((c) => el("option", { value: c, text: c }))
        : [el("option", { value: name, text: `${name} (the folder has no checks yet)` })]));
      field.value = checks.includes(want) ? want : checks[0] ?? name;
    }
  };
  for (const v of p.vars) {
    let field;
    if (v.name === "peer") {
      field = select("peer", workers.length ? workers.map((w) => [w.name, `${w.name} (${w.capabilities.map(providerLabel).join(", ")})`]) : [["", "(no connected peer runs tasks)"]], v.default);
      field.addEventListener("change", refreshFolder);
    } else if (v.name === "folder") {
      field = el("select", { name: "folder" });
      field.addEventListener("change", refreshChecks);
    } else if (CHECK_VARS.has(v.name)) {
      field = el("select", { name: v.name }, el("option", { value: v.default, text: v.default }));
    } else if (APPROVER_VARS.has(v.name)) {
      field = select(v.name, [["me", "me (you, in this UI)"], ...state.trusted.map((t) => [t.name, t.name])], v.default);
    } else if (v.name === "reviewers" && (state.squads ?? []).length) {
      field = select("reviewers", state.squads.map((s) => [s.name, `#${s.name} (${s.members.map((m) => m.name).join(", ")})`]), v.default);
    } else {
      field = el("input", { type: "text", name: v.name, value: v.default ?? "", spellcheck: "false", autocomplete: "off" });
      if (v.name === "spec") field.placeholder = "e.g. D:\\src\\shop\\specs\\001-photo-albums";
    }
    fields[v.name] = field;
    field.id = `pb-var-${v.name}`;
    field.required = v.default == null;
    const noWorker = v.name === "peer" && !workers.length;
    const help = noWorker ? NO_WORKER
      : CHECK_VARS.has(v.name) ? `The folder's check that runs as "${v.name}"` : VAR_HELP[v.name];
    if (help) field.setAttribute("aria-describedby", `${field.id}-hint`);
    box.append(el("div", { class: "field" },
      el("label", { for: field.id, text: field.required ? `${v.name} (required)` : v.name }),
      field,
      help ? el("span", { class: `small hint-text ${noWorker ? "warn-text" : "muted"}`, id: `${field.id}-hint`, text: help }) : null));
  }
  refreshFolder();
}

const NO_WORKER = "None of your connected peers runs tasks yet. Add one under Allowlist (Add peer); its agent needs Copilot, Claude or Codex signed in.";

/// Flags the fill step's empty required values; shows what to do and focuses the
/// first. True if every required value is there.
function checkPlaybookVars() {
  const fields = [...document.querySelectorAll("#playbook-vars [name]")];
  const missing = fields.filter((f) => f.required && !f.value.trim());
  for (const f of fields) {
    if (missing.includes(f)) f.setAttribute("aria-invalid", "true");
    else f.removeAttribute("aria-invalid");
  }
  if (!missing.length) return true;
  $("#playbook-error").textContent = missing[0].name === "peer" && !missing[0].value
    && !state.peers.some((x) => x.capabilities.length)
    ? "No peer can do this work yet: see the note under peer."
    : `Fill in ${missing.map((f) => f.name).join(", ")}.`;
  missing[0].focus();
  return false;
}

async function submitPlaybook(e) {
  e.preventDefault();
  const error = $("#playbook-error");
  if (playbookPicker.step === "pick") {
    const picked = $("#playbook-choices input:checked")?.value;
    if (!picked) {
      error.textContent = "Pick a playbook.";
      return;
    }
    renderPlaybookVars(picked);
    setPlaybookStep("fill");
    $("#playbook-vars").querySelector("select, input")?.focus();
    return;
  }
  const name = playbookPicker.name;
  if (!checkPlaybookVars()) return;
  const vars = Object.fromEntries([...document.querySelectorAll("#playbook-vars [name]")].map((i) => [i.name, i.value]));
  const button = $("#playbook-submit");
  button.disabled = true;
  button.textContent = "Filling in…";
  try {
    const res = await api(`/api/playbooks/${encodeURIComponent(name)}`, { method: "POST", body: JSON.stringify({ vars }) });
    $("#composer-input").value = res.text;
    state.playbook = name;
    $("#playbook-dialog").close("filled");
    renderPlanExplain();
    const ok = await checkPlan(res.text);
    // An imported spec: what was found (its warnings are at the top of the plan).
    const imported = res.notes?.length ? `${res.notes[0]} ` : "";
    toast(ok ? `${imported}Playbook filled in and checked: review it, then Ctrl+Enter to run` : `${imported}Playbook filled in, but it has problems (see under the plan)`, ok ? "" : "error");
  } catch (err) {
    const candidates = err.body?.candidates ?? [];
    const spec = $("#pb-var-spec");
    if (candidates.length && spec) {
      error.replaceChildren(`That folder has ${candidates.length} specs. Pick one:`,
        el("span", { class: "spec-picks" }, ...candidates.map((c) => el("button", {
          type: "button", class: "btn small", title: c.path,
          onclick: () => {
            spec.value = c.path;
            error.textContent = "";
            $("#playbook-submit").focus();
          },
        }, `${c.id}: ${c.title}`))));
    } else {
      error.textContent = err.message;
    }
  } finally {
    button.disabled = false;
    button.textContent = playbookPicker.step === "pick" ? "Next: who and where" : "Fill in the plan";
  }
}

/// A button that makes one of the plan check's fixes in the plan `input` (the composer
/// or the plan editor), then checks the plan again into `box`.
function fixButton(f, input, box) {
  return el("button", {
    type: "button", class: "btn small", title: "Change the plan this way",
    onclick: () => {
      input.value = planText.applyFix(input.value, f.edits);
      const building = input.id === "plan-editor-text" && planBuilder.active();
      if (building) planBuilder.load(input.value);
      input.dispatchEvent(new Event("input", { bubbles: true }));
      if (!building) input.focus();
      checkPlan(input.value, box, input).catch(() => {});
    },
  }, f.label);
}

/// Checks the plan against connected peers, folders, checks, squads, knowledge and
/// skills; shows the problems in `box` (under the plan's `input`), with buttons for
/// the fixes the check offers (in the plan builder, on the cards); true if it can run.
async function checkPlan(text, box = $("#plan-problems"), input = $("#composer-input")) {
  const building = input.id === "plan-editor-text" && planBuilder.active();
  if (!text.trim()) {
    box.hidden = true;
    if (building) planBuilder.checked(null);
    return false;
  }
  const res = await api("/api/plans/check", { method: "POST", body: JSON.stringify({ text }) });
  if (building) {
    planBuilder.checked(res);
    return res.ok;
  }
  box.hidden = false;
  if (!res.problems.length) {
    box.replaceChildren(el("p", { class: "ok", text: `✓ Ready to run: ${nOf(res.steps, "step")}, ${nOf(res.claims, "claim")}` }));
    return true;
  }
  box.replaceChildren(
    el("p", { class: res.ok ? "warn" : "bad", text: res.ok ? "Ready to run, with warnings:" : "Can't run yet:" }),
    el("ul", {}, ...res.problems.map((p) => el("li", { class: p.error ? "bad" : "warn" },
      el("strong", { text: `${p.error ? "Error" : "Warning"}${p.step ? ` (step ${p.step})` : ""}: ` }), p.message,
      p.fixes?.length ? el("span", { class: "fixes" }, ...p.fixes.map((f) => fixButton(f, input, box))) : null))));
  return res.ok;
}

let planCheckTimer = null;
function schedulePlanCheck() {
  clearTimeout(planCheckTimer);
  if (state.mode !== "plan") return;
  planCheckTimer = setTimeout(() => checkPlan($("#composer-input").value).catch(() => {}), 900);
}

// ---------- how a plan's run is tracked ----------
/// Issues looked up for the "Existing issue" choice, by number, with when: shared by the
/// composer's and the plan editor's choice, and looked up again after a minute.
const trackLookups = new Map();
const TRACK_LOOKUP_KEPT_MS = 60_000;

/// The choice beside Run plan (in the composer and the plan editor) of how a run is
/// tracked on GitHub, when a tracker is linked: a new issue, an issue it works on, or
/// not at all, with a line under it saying what running the plan does there (it looks
/// the issue up as you type its number). It reads and writes the plan's track: line in
/// `input`, then `changed()`; `shown()` says when it applies. Returns `sync`, which
/// brings it up to date with the text, and `ready`, which says whether the plan can run
/// as chosen (an existing issue needs its number) and, if not, goes to the number.
function trackChoice(box, input, changed, shown) {
  const noteId = `${box.id}-note`;
  const kind = el("select", { id: `${box.id}-kind`, "aria-describedby": noteId },
    el("option", { value: "new", text: "New issue" }),
    el("option", { value: "issue", text: "Existing issue…" }),
    el("option", { value: "off", text: "Don't track" }));
  const number = el("input", {
    type: "text", inputmode: "numeric", placeholder: "#12", size: "6", spellcheck: "false", autocomplete: "off",
    "aria-label": "Issue number", "aria-describedby": noteId,
  });
  const note = el("p", { id: noteId, class: "plan-track-note", "aria-live": "polite" });
  box.replaceChildren(el("label", { class: "small", for: kind.id }, "Track on GitHub"), kind, number, note);
  // "Existing issue" picked, its number not typed yet: the text has no line for it.
  let picking = false;
  // The plan was run (or scheduled) without the number: the note says it's needed.
  let asked = false;
  // The number being looked up, and the last lookup that failed: { n, error }.
  let pending = null;
  let failed = null;
  let timer = null;
  // What the note says now.
  let said = "";
  const write = (value) => {
    input.value = planText.setTrack(input.value, value);
    changed();
  };
  const lookUp = (n) => {
    pending = n;
    clearTimeout(timer);
    timer = setTimeout(async () => {
      try {
        const { issue } = await api(`/api/tracker/issues/${n}`);
        trackLookups.set(n, { issue, at: Date.now() });
        if (failed?.n === n) failed = null;
      } catch (err) {
        failed = { n, error: err.message, at: Date.now() };
      }
      pending = null;
      sync();
    }, 400);
  };
  kind.addEventListener("change", () => {
    picking = kind.value === "issue";
    asked = false;
    if (kind.value === "off") write("off");
    else if (kind.value === "new") write(null);
    else {
      number.hidden = false;
      number.focus();
      const n = Number(number.value.trim().replace(/^#/, ""));
      if (Number.isInteger(n) && n > 0) write(`#${n}`);
      else sync();
    }
  });
  number.addEventListener("input", () => {
    const n = Number(number.value.trim().replace(/^#/, ""));
    const ok = Boolean(number.value.trim()) && Number.isInteger(n) && n > 0;
    picking = !ok;
    failed = null;
    write(ok ? `#${n}` : null);
  });
  const sync = () => {
    box.hidden = !tracker?.linked || !tracker.tracker || !shown();
    if (box.hidden) return;
    const now = planText.trackOf(input.value);
    const track = picking && now.kind === "new" ? { kind: "issue", number: null } : now;
    kind.value = track.kind;
    number.hidden = track.kind !== "issue";
    if (track.number && document.activeElement !== number) number.value = `#${track.number}`;
    const n = track.kind === "issue" ? track.number : null;
    const seen = n ? trackLookups.get(n) : null;
    const found = !n ? null
      // A failed lookup is tried again a little later.
      : failed?.n === n && Date.now() - failed.at < 10_000 ? { error: failed.error }
        : seen && Date.now() - seen.at < TRACK_LOOKUP_KEPT_MS ? (seen.issue ? { issue: seen.issue } : { missing: true })
          : null;
    if (n && !found && pending !== n) lookUp(n);
    const { parts, warn } = planText.trackNote(track, tracker.tracker, found);
    // Only a change is drawn (and so read out): typing in the plan redraws nothing.
    const next = JSON.stringify(parts);
    if (said !== next) {
      said = next;
      note.replaceChildren(...parts.map((p) => (typeof p === "string" ? p : externalLink(p.href, p.text, p.label))));
    }
    const needed = asked && track.kind === "issue" && !track.number;
    note.classList.toggle("warn", warn || needed);
    number.setAttribute("aria-invalid", String(Boolean(found?.missing) || needed));
  };
  input.addEventListener("input", sync);
  const ready = () => {
    if (box.hidden || !picking || planText.trackOf(input.value).kind !== "new") return true;
    asked = true;
    sync();
    number.focus();
    return false;
  };
  return { sync, ready };
}
let planTrack = { sync() {}, ready: () => true };
let editorTrack = { sync() {}, ready: () => true };

// ---------- plan editor ----------
/// Opens the plan in a large editor: as forms (the builder) or as text with what can go
/// into it beside it, whichever was used last.
function openPlanEditor() {
  const editor = $("#plan-editor-text");
  editor.value = $("#composer-input").value;
  planBuilder.reset();
  $("#plan-editor").showModal();
  planBuilder.setView(planBuilder.view());
  editorTrack.sync();
}

/// Makes a palette change in the plan editor (see ui/plan-text.js) and selects what
/// to type next.
function applyPlanEdit(change) {
  const editor = $("#plan-editor-text");
  editor.focus();
  if (change.error) {
    toast(change.error, "error");
    return;
  }
  editor.setRangeText(change.insert, change.from, change.to);
  editor.setSelectionRange(...change.select);
  editor.dispatchEvent(new Event("input", { bubbles: true }));
}

function renderPalette() {
  const editor = $("#plan-editor-text");
  const text = editor.value;
  const workers = state.peers.filter((p) => p.capabilities.length);
  const shares = workers.flatMap((p) => (p.shares ?? []).map((s) => ({ peer: p.name, ...s })));
  const firstShare = shares.find((s) => s.name !== "data");
  const defaults = {
    target: firstShare ? `@${firstShare.peer}:${firstShare.name}` : workers[0] ? `@${workers[0].name}` : "@peer:folder",
    check: firstShare?.checks?.[0],
  };
  // Changes are worked out on click, from the plan as it is then.
  const chip = (label, title, edit) => el("button", {
    type: "button", class: "insert", title, onclick: () => applyPlanEdit(edit(editor.value, editor.selectionStart)),
  }, label);
  const line = (label, what, title) => chip(label, title, (t, pos) => planText.addLine(t, pos, what, defaults));
  const part = (value, title) => chip(value, title, (t, pos) => planText.addPart(t, pos, value, defaults));
  const section = (title, hint, items) => items.length
    ? el("section", { class: "palette-section" }, el("h4", { text: title }),
      hint ? el("p", { class: "muted small", text: hint }) : null,
      el("div", { class: "chips" }, ...items))
    : null;
  const steps = planText.stepIds(text);
  const claims = planText.claimIds(text);
  const about = (name) => shares.find((s) => s.mcp_about?.[name])?.mcp_about?.[name];
  const sections = [
    section("Add a line", null, [
      line("Claim", "claim", "Something the run must prove (goes with the other claims)"),
      line("Step", "step", "A new step after the cursor's line: who does what, and how it is checked"),
      line("Sign-off", "signoff", "You sign off on the step at the cursor (or the last step); your answer is signed evidence"),
      tracker?.linked ? line("Track on an issue", "track", "The run works on this issue of your tracker, its steps as sub-issues (type its number)") : null,
    ].filter(Boolean)),
    section("Who runs it", "Peers, the folders they share with you, and squads", [
      ...workers.flatMap((p) => [
        part(`@${p.name}`, `${p.name}, in its data folder`),
        ...(p.shares ?? []).filter((s) => s.name !== "data").map((s) =>
          part(`@${p.name}:${s.name}`, `${p.name}'s ${s.name} folder (${ACCESS_SHORT[s.access] ?? s.access})`)),
      ]),
      ...(state.squads ?? []).map((s) => part(`#${s.name}`, `Any online member of squad ${s.name}`)),
      part("@workiq", "Your agent asks Microsoft 365 (Work IQ) itself: the answer is context, never proof"),
    ]),
    section("Checks and MCP servers", "What those folders' owners offer; workiq= searches your Microsoft 365 for the step's own words when it is sent", [
      ...[...new Set(shares.flatMap((s) => s.checks ?? []))].map((c) =>
        part(`verify=${c}`, `After the step, the peer's runtime runs its ${c} check`)),
      ...[...new Set(shares.flatMap((s) => s.mcp ?? []))].map((m) =>
        part(`mcp=${m}`, about(m) ? `${m}: ${about(m)}` : `Let the step use MCP server ${m}`)),
      ...planText.WORKIQ_SOURCES.map(([name, what]) => part(`workiq=${name}`, `Searches ${what}`)),
    ]),
    section("Agents", "A role the step takes, with its instructions, skills and MCP servers", (state.agents ?? []).map((a) =>
      part(`agent=${a.name}`, [a.description, a.skills.length ? `skills: ${a.skills.join(", ")}` : "", a.mcp.length ? `MCP servers: ${a.mcp.join(", ")}` : ""].filter(Boolean).join(" · ") || a.name))),
    section("Skills", "Instructions the step follows; pick several to combine them", (state.skills ?? []).map((s) =>
      part(`skill=${s.name}`, s.description || s.name))),
    section("Knowledge", "What agents learned, given to the step", (state.knowledge ?? []).map((k) =>
      part(`context=${k.name}`, `Give the step the topic ${k.name}`))),
    section("Order and options", null, [
      ...steps.map((id) => part(`after ${id}`, `Run once step ${id} is done`)),
      ...claims.map((id) => part(`claims=${id}`, `This step proves ${id}`)),
      part("attempts=3", "Retry a failing step with its failures, up to 3 times"),
      part("save=topic", "Save what the step learned under a knowledge topic (type its name)"),
    ]),
  ].filter(Boolean);
  $("#plan-palette").replaceChildren(...sections);
}

let editorTimer = null;
function onPlanEditorInput() {
  clearTimeout(editorTimer);
  editorTimer = setTimeout(() => {
    renderPalette();
    checkPlan($("#plan-editor-text").value, $("#plan-editor-check"), $("#plan-editor-text")).catch(() => {});
  }, 600);
}

// ---------- health and knowledge ----------
const INCIDENT_TEXT = { opened: "Incident opened", tsg: "TSG", assessed: "Debugging agent", learned: "Learned a TSG", escalated: "Needs a person", resolved: "Resolved" };

async function loadOps() {
  const ops = await api("/api/ops");
  state.ops = ops;
  $("#incident-count").textContent = ops.open_incidents;
  $("#incident-count").classList.toggle("alert", ops.open_incidents > 0);
  const health = $("#health");
  health.replaceChildren();
  if (!ops.monitors.length) {
    health.append(el("p", { class: "muted small" }, "No monitors yet. ", el("strong", { text: "Add monitor" }),
      " runs a check of one of this agent's folders again and again, measures its availability and heals incidents with troubleshooting guides."));
  }
  const list = el("ul", { class: "monitor-list" });
  for (const m of ops.monitors) {
    const look = m.problem ? "misconfigured" : m.open_incident ? "incident" : m.last_ok === false ? "failing" : m.last_ok ? "up" : "unknown";
    const [cls, label] = {
      up: ["done", "Up"], failing: ["failed", "Failing"], incident: ["failed", "Incident"],
      unknown: ["skipped", "Unknown"], misconfigured: ["cancelled", "Can't run"],
    }[look];
    const what = m.check ? `${m.share}:${m.check} ${everyText(m.every_secs)}` : null;
    const numbers = m.problem ? "not checked"
      : `${m.availability == null ? "no data yet" : `${m.availability.toFixed(2)}%`} · SLO ${m.slo}%${m.meets_slo === false ? " missed" : ""}${m.p95_ms != null ? ` · p95 ${m.p95_ms} ms` : ""}`;
    list.append(el("li", { class: `monitor ${look}` },
      el("span", { class: "dot", "aria-hidden": "true" }),
      el("span", { class: "name", text: m.name }),
      el("span", { class: "muted small", text: [what, numbers].filter(Boolean).join(" · ") }),
      el("span", { class: `status ${cls}`, text: label }),
      el("button", {
        type: "button", class: "icon-btn", title: `Remove monitor ${m.name}`, "aria-label": `Remove monitor ${m.name}`,
        onclick: () => removeMonitor(m.name),
      }, icon(ICONS.trash)),
      m.problem ? el("span", { class: "problem", text: `${m.problem[0].toUpperCase()}${m.problem.slice(1)}. Give the folder its check back, or remove the monitor.` }) : null));
  }
  health.append(list);
  const t = ops.tasks;
  const r = ops.runs;
  const tasks = t.total
    ? `${t.total} peer task${t.total === 1 ? "" : "s"} (${counts([[t.done, "done"], [t.failed, "failed"], [t.refused, "refused"]]) || "none finished"})${t.avg_secs != null ? `, ${duration(t.avg_secs * 1000)} on average` : ""}`
    : "no peer tasks";
  const runs = r.total
    ? `evidence runs: ${counts([[r.passed, "passed"], [r.failed, "failed"], [r.unclaimed, "without claims"]])}`
    : "no evidence runs";
  health.append(el("p", { class: "task-meta", text: `Last 24 h: ${tasks} · ${runs}` }));
  if (ops.incidents.length) {
    health.append(el("details", {}, el("summary", { text: `Incident log (${ops.incidents.length})` }),
      el("ol", { class: "incident-log" }, ...ops.incidents.map((e) => el("li", { class: `incident ${e.kind}` },
        el("strong", { text: `${stamp(e.ts * 1000)} ${e.monitor} · ${INCIDENT_TEXT[e.kind] ?? e.kind}: ` }),
        e.text.split("\n")[0])))));
  }
  const kb = $("#kb-list");
  state.knowledge = ops.knowledge;
  $("#kb-count").textContent = ops.knowledge.length;
  kb.replaceChildren(...(ops.knowledge.length ? ops.knowledge.map((k) => el("li", {},
    el("button", { type: "button", class: "run-open", onclick: () => openTopic(k.name), "aria-label": `Open knowledge topic ${k.name}` },
      el("span", { class: "task-title", text: k.name }),
      k.tsg ? el("span", { class: `badge ${k.tsg.status === "active" ? "ok" : "neutral"}`, text: `TSG · ${k.tsg.status}` })
        : el("span", { class: "muted small", text: `${k.chars} chars` })))) :
    [el("li", { class: "empty" }, "Nothing yet. Try the ", el("strong", { text: "discover" }), " playbook on an existing project.")]));
}

/** How often a monitor runs its check ("every minute", "every 5 min"). */
function everyText(secs) {
  if (secs % 3600 === 0) return secs === 3600 ? "every hour" : `every ${secs / 3600} h`;
  if (secs % 60 === 0) return secs === 60 ? "every minute" : `every ${secs / 60} min`;
  return `every ${secs} s`;
}

async function removeMonitor(name) {
  const ok = await confirmDialog({
    title: `Remove monitor ${name}?`,
    body: "This agent stops running its check, and closes its open incidents. Its history stays in the incident log and ops reports.",
    confirm: "Remove monitor",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/monitors/${encodeURIComponent(name)}`, { method: "DELETE" });
    await loadOps();
    // Its row (and the button that removed it) is gone: focus stays in the panel.
    $("#add-monitor-btn").focus();
    toast(`Monitor ${name} removed`);
  } catch (err) {
    toast(err.message, "error");
  }
}

/** The folder picked in the monitor dialog, with its checks (name → command). */
function monitorFolder() {
  return (state.ops?.checks ?? []).find((f) => f.folder === $("#monitor-share").value);
}

async function openMonitorDialog() {
  // Fresh: a check may have been added in a terminal since.
  await loadOps().catch(() => {});
  $("#monitor-form").reset();
  $("#monitor-error").textContent = "";
  $("#monitor-name").removeAttribute("aria-invalid");
  $("#monitor-slo").removeAttribute("aria-invalid");
  delete $("#monitor-name").dataset.typed;
  const folders = state.ops?.checks ?? [];
  // Monitors run a folder's check: without one there is nothing to pick yet.
  $("#monitor-none").hidden = folders.length > 0;
  $("#monitor-fields").hidden = !folders.length;
  $("#monitor-submit").hidden = !folders.length;
  $("#monitor-cancel").textContent = folders.length ? "Cancel" : "Close";
  $("#monitor-share").replaceChildren(...folders.map((f) => el("option", { value: f.folder, text: f.folder })));
  renderMonitorChecks();
  $("#monitor-dialog").showModal();
  (folders.length ? $("#monitor-name") : $("#monitor-cancel")).focus();
}

function renderMonitorChecks() {
  const names = Object.keys(monitorFolder()?.checks ?? {});
  $("#monitor-check").replaceChildren(...names.map((n) => el("option", { value: n, text: n })));
  // A health check is the usual thing to watch.
  const health = names.find((n) => /health|ping|smoke|status/i.test(n));
  if (health) $("#monitor-check").value = health;
  renderMonitorCommand();
}

function renderMonitorCommand() {
  const command = monitorFolder()?.checks?.[$("#monitor-check").value];
  $("#monitor-command").textContent = command ? `Runs: ${command}` : "";
  // Until a name is typed, it follows what is checked.
  const name = $("#monitor-name");
  if (!name.dataset.typed && command) name.value = `${$("#monitor-share").value}-${$("#monitor-check").value}`.slice(0, 64);
}

async function submitMonitor(e) {
  e.preventDefault();
  const nameInput = $("#monitor-name");
  const sloInput = $("#monitor-slo");
  nameInput.removeAttribute("aria-invalid");
  sloInput.removeAttribute("aria-invalid");
  const name = nameInput.value.trim();
  const slo = Number(sloInput.value);
  if (!/^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$/.test(name)) {
    nameInput.setAttribute("aria-invalid", "true");
    $("#monitor-error").textContent = "Name the monitor with letters, digits, '-', '_' and '.' (not starting with '.').";
    nameInput.focus();
    return;
  }
  if (sloInput.value === "" || !(slo >= 0 && slo <= 100)) {
    sloInput.setAttribute("aria-invalid", "true");
    $("#monitor-error").textContent = "The availability objective is a percentage from 0 to 100, e.g. 99.5.";
    sloInput.focus();
    return;
  }
  const submit = $("#monitor-submit");
  submit.disabled = true;
  try {
    await api("/api/monitors", {
      method: "POST",
      body: JSON.stringify({
        name, share: $("#monitor-share").value, check: $("#monitor-check").value,
        every_secs: Number($("#monitor-every").value), fail_after: Number($("#monitor-fail-after").value), slo,
      }),
    });
    $("#monitor-dialog").close();
    toast(`Monitor ${name} added: its first check runs within seconds`);
  } catch (err) {
    $("#monitor-error").textContent = err.message;
  } finally {
    submit.disabled = false;
  }
}

// Whether the last import from Microsoft 365 worked: the dialog then opens empty, else with
// what was typed.
let kbImported = false;

/** The import's sources, as a step's workiq= takes them: all of Microsoft 365 (the default),
 * or some of it. */
function renderKbImportSources() {
  $("#kb-import-sources").replaceChildren(...planText.WORKIQ_SOURCES.map(([name, what]) =>
    el("label", { class: "check" },
      el("input", { type: "checkbox", value: name, checked: name === "all" }),
      el("span", { text: `${what[0].toUpperCase()}${what.slice(1)}` }))));
}

/** Checking all clears the other sources, and checking another clears all (as in workiq=). */
function onKbImportSource(e) {
  const boxes = [...$("#kb-import-sources").querySelectorAll("input")];
  const have = boxes.filter((b) => b !== e.target && b.checked).map((b) => b.value);
  const now = planText.toggleSource(have, e.target.value, e.target.checked);
  for (const b of boxes) b.checked = now.includes(b.value);
}

function openKbImport() {
  if (kbImported) $("#kb-import-form").reset();
  kbImported = false;
  $("#kb-import-result").replaceChildren();
  $("#kb-import-error").textContent = "";
  $("#kb-import-cancel").textContent = "Cancel";
  for (const id of ["#kb-import-topic", "#kb-import-query"]) $(id).removeAttribute("aria-invalid");
  $("#kb-import-dialog").showModal();
  $("#kb-import-topic").focus();
}

/** Imports what Work IQ finds in Microsoft 365 as a knowledge topic, marked with where it came
 * from, and says how many results were kept, with their labels, and withheld. */
async function submitKbImport(e) {
  e.preventDefault();
  const button = $("#kb-import-submit");
  if (button.disabled) return;
  const dialog = $("#kb-import-dialog");
  const error = $("#kb-import-error");
  const result = $("#kb-import-result");
  const topicInput = $("#kb-import-topic");
  const queryInput = $("#kb-import-query");
  kbImported = false;
  error.textContent = "";
  result.replaceChildren();
  topicInput.removeAttribute("aria-invalid");
  queryInput.removeAttribute("aria-invalid");
  const topic = topicInput.value.trim();
  const query = queryInput.value.trim();
  const from = [...$("#kb-import-sources").querySelectorAll("input:checked")].map((b) => b.value);
  const replace = $("#kb-import-replace").checked;
  const refuse = (input, text, invalid = true) => {
    error.textContent = text;
    if (invalid) input.setAttribute("aria-invalid", "true");
    input.focus();
  };
  if (!/^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$/.test(topic)) {
    refuse(topicInput, "Name the topic with letters, digits, '-', '_' and '.' (not starting with '.').");
    return;
  }
  if (!replace && (state.knowledge ?? []).some((k) => k.name === topic)) {
    refuse(topicInput, `There is a topic ${topic} already: check "Replace the topic if it exists" to overwrite it, or name another one.`);
    return;
  }
  if (!query) {
    refuse(queryInput, "Say what to look for in your Microsoft 365.");
    return;
  }
  if (!from.length) {
    refuse($("#kb-import-sources input"), "Check where to look: all of your Microsoft 365, or some of it.", false);
    return;
  }
  button.disabled = true;
  button.textContent = "Importing…";
  let imported = false;
  try {
    const res = await api("/api/kb/import/workiq", {
      method: "POST",
      body: JSON.stringify({ topic, query, from: from.join(","), replace }),
    });
    imported = true;
    kbImported = true;
    const counts = workiqCounts(res.kept, res.labels, res.withheld);
    if (dialog.open) {
      result.replaceChildren(el("strong", { text: `Imported ${res.topic}` }), `: ${counts}. Plan steps use it with `,
        el("span", { class: "mono", text: `context=${res.topic}` }), ".");
      $("#kb-import-cancel").textContent = "Close";
    } else {
      toast(`Imported ${res.topic} from Microsoft 365: ${counts}`);
    }
    await loadOps().catch((err) => toast(err.message, "error"));
  } catch (err) {
    // Closed while it ran: the error is the toast's.
    if (dialog.open) error.textContent = err.message;
    else toast(err.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = "Import topic";
    if (dialog.open && focusLost()) (imported ? $("#kb-import-cancel") : button).focus();
  }
}

async function loadSkills() {
  const res = await api("/api/skills");
  state.skills = res.skills;
  state.agents = res.agents ?? [];
  $("#skills-summary").textContent = `Skills (${res.skills.length})`;
  $("#skill-list").replaceChildren(...res.skills.map((s) => el("li", {},
    el("button", { type: "button", class: "run-open", onclick: () => openSkill(s.name), "aria-label": `Open skill ${s.name}` },
      el("span", { class: "task-title", text: s.name }),
      el("span", { class: `badge ${s.source === "profile" ? "ok" : "neutral"}`, text: s.source })),
    el("div", { class: "muted small", text: s.description }))));
}

/** The run dialog showing other content (a skill or a topic): nothing of a run stays. */
function runDialogFor(title) {
  $("#run-h").textContent = title;
  $("#run-verdict").textContent = "";
  $("#run-signatures").replaceChildren();
  $("#run-prototypes").replaceChildren();
  $("#run-workiq").hidden = true;
  $("#run-workiq").replaceChildren();
  $("#run-download").hidden = true;
}

async function openSkill(name) {
  try {
    const res = await api(`/api/skills/${encodeURIComponent(name)}`);
    runDialogFor(`Skill: ${name}`);
    $("#run-report").replaceChildren(renderMarkdown(res.text.replace(/^---[\s\S]*?\n---\n/, "")));
    $("#run-dialog").showModal();
    $("#run-close").focus();
  } catch (err) {
    toast(err.message, "error");
  }
}

async function openTopic(topic) {
  try {
    const res = await api(`/api/kb/${encodeURIComponent(topic)}`);
    runDialogFor(`Knowledge: ${topic}`);
    $("#run-report").replaceChildren(renderMarkdown(res.text.replace(/^<!--.*-->\n/, "")));
    $("#run-dialog").showModal();
    $("#run-close").focus();
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- GitHub ----------
let github = null;
async function loadGithub() {
  github = await api("/api/github");
  const box = $("#github");
  box.replaceChildren();
  const signedIn = github.account && (github.account.user || github.account.owner);
  $("#link-repo-btn").hidden = !signedIn;
  if (!signedIn) {
    box.append(el("p", { class: "muted small", text: "Sign in to clone your repositories, let agents work on them on their own branches, and open pull requests." }),
      el("button", { type: "button", class: "btn small primary", onclick: openGithubLogin }, "Sign in to GitHub"));
    return;
  }
  box.append(el("p", { class: "small" }, github.account.user
    ? ["Signed in as ", el("strong", { text: `${github.account.user}@${github.account.host}` }), " ",
      el("button", { type: "button", class: "btn small", onclick: openGithubLogin }, "Change account")]
    : `Uses ${github.account.owner}'s GitHub account (digital worker)`));
  if (github.account.user) box.append(el("div", { id: "github-scopes" }));
  renderGithubScopes();
  // A sign-in still waits for its code (e.g. the page was reloaded): show it again, unless
  // it is the one just closed here (it reads as waiting until gh is switched back).
  if (github.flow?.state === "waiting" && !$("#gh-flow-dialog").open && github.flow.code !== ghFlow?.code) {
    showGhFlow(github.flow);
  }
  if (!github.serving) box.append(el("p", { class: "muted small", text: "This agent doesn't run peers' tasks (start it with --serve) so linked repositories are for your own use." }));
  if (!github.repos.length) box.append(el("p", { class: "muted small", text: "No linked repositories yet." }));
  for (const r of github.repos) {
    const branches = r.branches.slice(0, 5).map((b) => el("li", { class: "branch", "data-branch": b.name },
      el("span", { class: "mono small", text: b.name }),
      el("span", { class: "muted small", text: ` ${stamp(b.unix * 1000)} · ${b.subject}` }),
      b.issues?.length ? el("span", { class: "muted small", text: ` · closes ${shortRefs(b.issues, r.slug).join(", ")}` }) : null,
      el("span", { class: "small branch-state" }),
      el("span", { class: "branch-actions" }, ...branchActions(r, b, null))));
    const more = r.branches.length - branches.length;
    if (more > 0) branches.push(el("li", { class: "muted small", text: `${more} older task branch${more === 1 ? "" : "es"} not shown` }));
    const list = branches.length ? el("ul", { class: "branch-list" }, ...branches) : null;
    box.append(el("div", { class: "repo" },
      el("div", { class: "task-top" },
        el("span", { class: "task-title", text: `${r.name} · ${r.slug}` }),
        el("span", { class: "badge neutral", text: r.branch })),
      el("div", { class: "repo-actions" },
        el("button", { type: "button", class: "btn small", onclick: (e) => syncRepo(r, e.currentTarget) }, "Sync"),
        el("button", { type: "button", class: "btn small danger", onclick: () => unlinkRepo(r) }, "Unlink")),
      list ?? el("p", { class: "muted small", text: "No task branches yet: agents' editing tasks in this folder commit to p2p/… branches." })));
    if (list) loadBranchStates(r, list);
  }
}

/// Says where each listed task branch's work is (a pull request open or merged, its
/// commit or its changes on the repository's branch already, or what isn't there yet),
/// and offers what fits it (see branchButtons): only Delete branch once its work is
/// there, Review changes and Delete branch once its pull request or its issues are
/// closed, and Open PR while there is work to merge and no pull request is open.
async function loadBranchStates(r, list) {
  let res;
  try {
    res = await api(`/api/repos/${encodeURIComponent(r.name)}/branches`);
  } catch {
    return;
  }
  for (const li of list.querySelectorAll("li[data-branch]")) {
    const b = r.branches.find((x) => x.name === li.dataset.branch);
    const st = res.states?.[b?.name];
    if (!st) continue;
    taskBranchStates.set(branchKey(r, b.name), st);
    const shown = branchStatusNodes(r, b);
    li.querySelector(".branch-state").replaceChildren(" · ", ...shown.nodes);
    li.querySelector(".branch-actions").replaceChildren(...branchActions(r, b, shown.status));
  }
  if (res.error) list.append(el("li", { class: "muted small", text: `GitHub can't say where their work is now: ${res.error}` }));
  if (res.fetch_error) {
    list.append(el("li", { class: "muted small", text: `As of this clone's last fetch: ${r.branch} couldn't be fetched (${res.fetch_error}).` }));
  }
}

/** A task branch's buttons for `status` (null: not known yet), as branchButtons says:
 * Review changes while there is something to review, Open PR while there is work to merge
 * and no pull request is open, Delete branch once it is done with (its confirmation says
 * what that loses). */
function branchActions(r, b, status) {
  const make = {
    review: () => el("button", {
      type: "button", class: "btn small", "aria-label": `Review changes on ${b.name}`,
      onclick: (e) => openBranchReview(r, b, e.currentTarget),
    }, "Review changes"),
    pr: () => el("button", { type: "button", class: "btn small", "aria-label": `Open a pull request from ${b.name}`, onclick: () => openPr(r, b) }, "Open PR"),
    delete: () => el("button", {
      type: "button", class: "btn small danger", "aria-label": `Delete branch ${b.name}`,
      onclick: () => deleteBranch(r, b),
    }, "Delete branch"),
  };
  return branchButtons(status).map((name) => make[name]());
}

/** Deletes task branch `b` of `r` once confirmed, saying what that loses; whether it did. */
async function deleteBranch(r, b) {
  const ok = await confirmDialog({
    title: `Delete ${b.name}?`,
    body: branchDeleteNote(taskBranchStates.get(branchKey(r, b.name)) ?? null, r.branch, r.slug),
    confirm: "Delete branch",
    danger: true,
  });
  if (!ok) return false;
  try {
    await api(`/api/repos/${encodeURIComponent(r.name)}/branches?branch=${encodeURIComponent(b.name)}`, { method: "DELETE" });
    taskBranchStates.delete(branchKey(r, b.name));
    toast(`Deleted ${b.name}`);
    await loadGithub();
    return true;
  } catch (err) {
    toast(err.message, "error");
    return false;
  }
}

function openGithubLogin() {
  $("#github-error").textContent = "";
  const accounts = github?.accounts ?? [];
  // Preselect one: the profile's account, else gh's active one on github.com, else the first.
  const current = github?.account?.user
    ? accounts.findIndex((a) => a.login === github.account.user && a.host === github.account.host) : -1;
  const active = accounts.findIndex((a) => a.active && a.host === "github.com");
  const picked = current >= 0 ? current : active >= 0 ? active : 0;
  $("#github-accounts").replaceChildren(...(accounts.length
    ? accounts.map((a, i) => el("label", { class: "check" },
      el("input", { type: "radio", name: "gh-account", value: `${a.login}@${a.host}`, checked: i === picked || undefined }),
      el("span", { text: `${a.login} on ${a.host}${a.active ? " (active)" : ""}` })))
    : [el("span", { class: "muted small", text: "No GitHub CLI account on this machine yet: sign one in below." })]));
  $("#github-new-host").value = github?.account?.host || "github.com";
  $("#github-dialog").showModal();
}

// ---------- GitHub token scopes and sign-ins without a terminal ----------
/** What each scope of the GitHub account's token is for, in Proofkin. */
const SCOPE_USES = {
  repo: "clone, push branches and open pull requests",
  workflow: "push changes to GitHub Actions workflows",
  project: "the tracker's board",
};

/** The GitHub card's account details: its token's scopes, a Grant access button when
 * the linked tracker's board needs one it lacks, and gh's active account if another. */
function renderGithubScopes() {
  const box = $("#github-scopes");
  if (!box || !github?.account?.user) return;
  const same = (a, b) => a?.toLowerCase() === b?.toLowerCase();
  const mine = github.accounts.find((a) => same(a.login, github.account.user) && a.host === github.account.host);
  const active = github.accounts.find((a) => a.active && a.host === github.account.host);
  const parts = [];
  if (mine) {
    const known = Object.keys(SCOPE_USES).filter((s) => mine.scopes.includes(s));
    parts.push(el("p", { class: "muted small" }, known.length ? "Its token may: " : "Its token has none of the scopes Proofkin uses.",
      ...known.flatMap((s, i) => [i ? "; " : "", el("span", { class: "mono", text: s }), ` (${SCOPE_USES[s]})`]),
      known.length ? "." : ""));
    // The Tracker card says so too; here it's next to the account it is about.
    const need = tracker?.linked ? tracker.tracker?.board_scope : null;
    if (need && !mine.scopes.includes(need)) {
      parts.push(el("p", { class: "small" }, "It lacks ", el("span", { class: "mono", text: need }),
        ", which the tracker's board needs."),
      el("p", {}, el("button", { type: "button", class: "btn small", onclick: () => grantScope(need) }, "Grant access")));
    }
  }
  if (active && !same(active.login, github.account.user)) {
    parts.push(el("p", { class: "muted small", text: `The GitHub CLI's active account here is ${active.login}; Proofkin uses ${github.account.user}'s token without switching to it.` }));
  }
  box.replaceChildren(...parts);
}

let ghFlow = null;
let ghFlowTimer = null;

/** Gives the profile's GitHub account `scope` (gh's device flow, shown in a dialog). */
async function grantScope(scope) {
  try {
    const res = await api("/api/github/grant", { method: "POST", body: JSON.stringify({ scopes: [scope] }) });
    showGhFlow(res.flow);
  } catch (err) {
    toast(err.message, "error");
  }
}

/** Signs another account in to the GitHub CLI for this agent (from the Change account dialog). */
async function signInAnother() {
  const scopes = [["#github-new-project", "project"], ["#github-new-workflow", "workflow"]]
    .filter(([id]) => $(id).checked).map(([, scope]) => scope);
  const button = $("#github-new-btn");
  button.disabled = true;
  $("#github-error").textContent = "";
  try {
    const res = await api("/api/github/login/new", {
      method: "POST", body: JSON.stringify({ host: $("#github-new-host").value.trim() || "github.com", scopes }),
    });
    $("#github-dialog").close();
    showGhFlow(res.flow);
  } catch (err) {
    $("#github-error").textContent = err.message;
  } finally {
    button.disabled = false;
  }
}

function showGhFlow(flow) {
  ghFlow = flow;
  const grant = flow.kind === "grant";
  $("#gh-flow-h").textContent = grant ? "Grant access on GitHub" : "Sign in to GitHub";
  $("#gh-flow-what").textContent = grant
    ? `Enter this code on GitHub to give ${flow.login}'s token the ${flow.scopes.join(", ")} scope.`
    : "Enter this code on GitHub while signed in there as the account to add.";
  $("#gh-flow-code").textContent = flow.code;
  const open = $("#gh-flow-open");
  open.href = flow.url;
  open.setAttribute("aria-label", `Open ${flow.url} (opens a new tab)`);
  const back = flow.active_before;
  $("#gh-flow-note").textContent = grant
    ? (back && back.toLowerCase() !== flow.login.toLowerCase()
      ? `Until you finish, the GitHub CLI's active account here is ${flow.login}; then it is ${back} again.` : "")
    : `When you finish, this agent uses the new account${back ? `, and the GitHub CLI's active account here is ${back} again` : ""}.`;
  setGhFlowStatus(flow);
  const dialog = $("#gh-flow-dialog");
  if (!dialog.open) dialog.showModal();
  if (flow.state === "waiting") pollGhFlow();
}

function setGhFlowStatus(flow) {
  const waiting = flow.state === "waiting";
  const status = $("#gh-flow-status");
  status.replaceChildren(...(waiting
    ? [el("span", { class: "spinner", "aria-hidden": "true" }), "Waiting for you to approve on GitHub…"]
    : flow.state === "failed" ? [`It didn't work: ${flow.reason}`] : []));
  status.classList.toggle("warn-text", flow.state === "failed");
  $("#gh-flow-cancel").textContent = waiting ? "Cancel" : "Close";
}

function pollGhFlow() {
  clearTimeout(ghFlowTimer);
  ghFlowTimer = setTimeout(async () => {
    let flow;
    try {
      flow = (await api("/api/github/flow")).flow;
    } catch {
      pollGhFlow();
      return;
    }
    if (!flow || flow.code !== ghFlow?.code) return;
    ghFlow = flow;
    if (flow.state === "waiting") pollGhFlow();
    else ghFlowEnded(flow);
  }, 2000);
}

async function ghFlowEnded(flow) {
  clearTimeout(ghFlowTimer);
  const dialog = $("#gh-flow-dialog");
  if (flow.state === "done") {
    if (dialog.open) dialog.close();
    toast(flow.kind === "grant"
      ? `${flow.login}'s token now has the ${flow.scopes.join(", ")} scope`
      : `Signed in to GitHub as ${flow.login}`);
  } else if (flow.state === "cancelled") {
    if (dialog.open) dialog.close();
  } else {
    setGhFlowStatus(flow);
  }
  // A new account is the profile's once GitHub accepts its token: look until it is.
  for (let i = 0; i < 5; i++) {
    await loadGithub().catch(() => {});
    if (flow.kind === "grant" || flow.state !== "done" || github?.account?.user?.toLowerCase() === flow.login?.toLowerCase()) break;
    await new Promise((resolve) => setTimeout(resolve, 1000));
  }
  loadTracker().catch(() => {});
}

/** Closing the dialog while the sign-in waits cancels it, so gh is switched back now. */
async function ghFlowClosed() {
  clearTimeout(ghFlowTimer);
  if (ghFlow?.state !== "waiting") return;
  ghFlow = { ...ghFlow, state: "cancelled" };
  try {
    await api("/api/github/flow", { method: "DELETE" });
  } catch {
    // It ended meanwhile.
  }
  loadGithub().catch(() => {});
}

async function submitGithubLogin(e) {
  e.preventDefault();
  const picked = document.querySelector("input[name=gh-account]:checked")?.value;
  if (!picked) return;
  const at = picked.lastIndexOf("@");
  try {
    await api("/api/github/login", { method: "POST", body: JSON.stringify({ user: picked.slice(0, at), host: picked.slice(at + 1) }) });
    $("#github-dialog").close();
    toast("Signed in to GitHub");
    await loadGithub();
    loadTracker().catch(() => {});
  } catch (err) {
    $("#github-error").textContent = err.message;
  }
}

function openRepoDialog() {
  $("#repo-form").reset();
  $("#repo-error").textContent = "";
  const peers = state.trusted.filter((t) => t.can_delegate);
  $("#repo-grant").replaceChildren(...(peers.length
    ? peers.map((t) => el("label", { class: "check" }, el("input", { type: "checkbox", value: t.name }), el("span", { text: `${t.name} (${ACCESS_SHORT[t.access] ?? t.access})` })))
    : [el("span", { class: "muted small", text: "No peer may delegate tasks to this agent yet; grant the folder later." })]));
  $("#repo-dialog").showModal();
  $("#repo-slug").focus();
}

async function submitRepo(e) {
  e.preventDefault();
  const button = $("#repo-submit");
  button.disabled = true;
  $("#repo-error").textContent = "Cloning…";
  try {
    const res = await api("/api/repos", {
      method: "POST",
      body: JSON.stringify({
        repo: $("#repo-slug").value.trim(),
        name: $("#repo-name").value.trim() || null,
        access: $("#repo-access").value,
        checks: $("#repo-checks").value.split("\n"),
        grant: [...document.querySelectorAll("#repo-grant input:checked")].map((i) => i.value),
      }),
    });
    $("#repo-dialog").close();
    toast(`Linked as folder ${res.name} (${res.branch})`);
    await loadGithub();
  } catch (err) {
    $("#repo-error").textContent = err.message;
  } finally {
    button.disabled = false;
  }
}

async function syncRepo(r, button) {
  button.disabled = true;
  try {
    const res = await api(`/api/repos/${encodeURIComponent(r.name)}/sync`, { method: "POST" });
    toast(`${r.name}: ${res.branch} at ${res.head}`);
  } catch (err) {
    toast(err.message, "error");
  } finally {
    button.disabled = false;
  }
}

async function unlinkRepo(r) {
  const ok = await confirmDialog({
    title: `Stop sharing ${r.slug}?`,
    body: `Peers can no longer use the folder ${r.name}. The clone and its branches stay on this machine.`,
    confirm: "Stop sharing",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/repos/${encodeURIComponent(r.name)}`, { method: "DELETE" });
    await loadGithub();
  } catch (err) {
    toast(err.message, "error");
  }
}

let prTarget = null;
async function openPr(r, b) {
  prTarget = { repo: r, branch: b.name };
  $("#pr-desc").textContent = `${b.name} → ${r.branch} in ${r.slug}`;
  // What it would change, and where to look at it first.
  const st = taskBranchStates.get(branchKey(r, b.name));
  const changes = st ? branchChangeText(st) : null;
  $("#pr-changes").replaceChildren(...(changes ? [`It changes ${changes}. `,
    el("button", { type: "button", class: "btn link", onclick: () => { $("#pr-dialog").close(); openBranchReview(r, b, null); } }, "Review changes")] : []));
  $("#pr-changes").hidden = !changes;
  $("#pr-title").value = b.subject.replace(/^(proofkin|p2pagents): /, "");
  // The issues the commit is for: the pull request closes them when it merges.
  const closes = shortRefs(b.issues ?? [], r.slug).map((ref) => `Closes ${ref}`);
  $("#pr-body").value = [...closes, ...(closes.length ? [""] : []), "Made with Proofkin."].join("\n");
  $("#pr-error").textContent = "";
  // The proof comes from the commit's signed record, checked by the server.
  $("#pr-proof").checked = false;
  $("#pr-proof").disabled = true;
  $("#pr-proof-status").textContent = "Checking the commit's signed record…";
  $("#pr-dialog").showModal();
  $("#pr-title").focus();
  try {
    const res = await api(`/api/repos/${encodeURIComponent(r.name)}/proof?branch=${encodeURIComponent(b.name)}`);
    if (prTarget?.branch !== b.name) return;
    $("#pr-proof").disabled = !res.proof;
    $("#pr-proof").checked = Boolean(res.proof);
    $("#pr-proof-status").textContent = res.proof ? res.proof.summary : `No proof to add: ${res.reason}`;
  } catch (err) {
    $("#pr-proof-status").textContent = `No proof to add: ${err.message}`;
  }
}

async function submitPr(e) {
  e.preventDefault();
  if (!prTarget) return;
  const button = $("#pr-submit");
  button.disabled = true;
  $("#pr-error").textContent = "Pushing…";
  try {
    const res = await api(`/api/repos/${encodeURIComponent(prTarget.repo.name)}/pr`, {
      method: "POST",
      body: JSON.stringify({
        branch: prTarget.branch, title: $("#pr-title").value, body: $("#pr-body").value, proof: $("#pr-proof").checked,
      }),
    });
    $("#pr-dialog").close();
    toast(`Pull request opened${res.proof ? " with its proof" : ""}: ${res.url}`);
    window.open(res.url, "_blank", "noopener");
  } catch (err) {
    $("#pr-error").textContent = err.message;
  } finally {
    button.disabled = false;
  }
}

/** Issue references as the repository `slug` writes them: `#N` for its own. */
function shortRefs(refs, slug) {
  return refs.map((r) => {
    const [repo, n] = r.split("#");
    return repo.toLowerCase() === slug.toLowerCase() ? `#${n}` : r;
  });
}

/** A link that leaves Proofkin (to GitHub): it opens a new tab, and says so. */
function externalLink(href, text, label = text, attrs = {}) {
  return el("a", { href, target: "_blank", rel: "noopener", "aria-label": `${label} (opens a new tab)`, ...attrs }, text);
}

// ---------- Work IQ ----------
let workiq = null;
// What the doctor's checks said last (and when), and whether they are running now.
let workiqReport = null;
let workiqChecking = false;
const WORKIQ_RESULT = { ok: ["done", "Passed"], failed: ["failed", "Failed"], skipped: ["skipped", "Skipped"] };
const WORKIQ_FIXER = { user: "you", admin: "admin", support: "support" };

/** The Work IQ card: the account this agent uses for Microsoft 365 work context (a digital
 * worker may use its owner's), how it connects, and today's calls against its budgets. */
async function loadWorkiq() {
  workiq = await api("/api/workiq");
  renderWorkiq();
}

function renderWorkiq() {
  const w = workiq;
  const box = $("#workiq");
  // Rebuilt, the card keeps focus on the button that had it (or the one in its place).
  const focused = box.contains(document.activeElement) ? document.activeElement.dataset.action ?? "" : null;
  const button = (action, label, onclick, kind = "") => el("button", {
    type: "button", class: `btn small${kind ? ` ${kind}` : ""}`, "data-action": action, onclick,
  }, label);
  const check = () => el("button", {
    type: "button", class: "btn small", "data-action": "check", disabled: workiqChecking,
    title: "Checks the connection step by step; the last check is one real Work IQ call, counted against today's budget",
    onclick: (e) => runWorkiqDoctor(e.currentTarget),
  }, workiqChecking ? "Checking…" : "Check");
  let nodes;
  if (w.not_used) {
    nodes = [el("p", { class: "small", text: `${w.not_used[0].toUpperCase()}${w.not_used.slice(1)}` })];
  } else if (!w.own && !w.signed_in) {
    nodes = [el("p", { class: "small", text: `Uses ${w.holder}'s Work IQ account, which isn't signed in. Sign it in on ${w.holder}'s Work IQ card, or in a terminal:` }),
      workiqCommand(`${w.hint} --cli`)];
  } else if (!w.own) {
    nodes = [el("p", { class: "small", text: `Uses ${w.holder}'s account: ${w.line}` }), workiqCliPath(w),
      el("div", { class: "workiq-actions" }, check())];
  } else if (!w.signed_in) {
    nodes = [el("p", { class: "muted small", text: "Microsoft 365 work context for plans: through Microsoft's Work IQ CLI, or your organization's own app." }),
      button("sign-in", "Sign in…", openWorkiqSignIn, "primary")];
  } else {
    nodes = [el("p", { class: "small", text: `Signed in: ${w.line}` }), workiqCliPath(w), workiqUsage(w.usage),
      el("div", { class: "workiq-actions" }, check(), button("budget", "Budget…", openWorkiqBudget),
        button("sign-out", "Sign out", signOutWorkiq, "danger"))];
  }
  box.replaceChildren(...nodes.flat().filter(Boolean));
  if (focused !== null) focusWorkiq(focused);
}

/** Today's Work IQ calls against the budgets, and a warning once the day's are used up. */
function workiqUsage(usage) {
  if (!usage) return el("p", { class: "warn-text small", text: "Today's Work IQ calls are unknown: the profile's Work IQ audit file can't be read." });
  const line = el("p", { class: "small", text: `Today: ${usage.today} of ${nOf(usage.per_day, "Work IQ call")} (at most ${usage.per_run} per run)` });
  return usage.today < usage.per_day ? line
    : [line, el("p", { class: "warn-text small", text: "Today's budget is used up: Work IQ calls are refused until tomorrow, unless you raise it (Budget…)." })];
}

/** Where the Work IQ CLI the account runs is, when the sign-in found it. */
function workiqCliPath(w) {
  return w.cli?.path ? el("p", { class: "muted small workiq-path" }, "Work IQ CLI at ", el("span", { class: "mono", text: w.cli.path })) : null;
}

/** A command to run in a terminal, with a button that copies it. */
function workiqCommand(command) {
  return el("p", { class: "workiq-command small" }, el("code", { class: "mono", text: command }),
    el("button", { type: "button", class: "btn small", onclick: () => copy(command, "Command") }, "Copy command"));
}

/** Focus goes to the card's `action` button, else its first, else its heading. */
function focusWorkiq(action) {
  const box = $("#workiq");
  (box.querySelector(`button[data-action="${action}"]`) ?? box.querySelector("button") ?? $("#workiq-h")).focus();
}

/** Whether focus went nowhere (the control that had it was disabled or removed). */
function focusLost() {
  return !document.activeElement || document.activeElement === document.body;
}

/** What the doctor's checks said last, under the card: each check's result and detail, and
 * for the one that failed, who makes the fix and how. */
function renderWorkiqChecks() {
  const box = $("#workiq-checks");
  const report = workiqReport;
  if (!report) {
    box.replaceChildren();
    return;
  }
  box.replaceChildren(
    el("p", { class: "small" }, el("strong", {
      text: report.ok ? "Work IQ is ready." : "Work IQ is not ready: make the fix for the failed check, then check again.",
    })),
    el("p", { class: "task-meta", text: `Checked: ${stamp(report.at)}` }),
    el("ul", { class: "workiq-check-list" }, ...report.checks.map((c) => {
      const [cls, label] = WORKIQ_RESULT[c.result] ?? ["skipped", c.result];
      return el("li", { class: "workiq-check" },
        el("span", { class: `status ${cls}`, text: label }),
        el("span", { class: "mono", text: c.name }),
        el("span", { class: "workiq-detail", text: c.detail }),
        c.fix ? el("span", { class: "workiq-fix", text: `Fix (${WORKIQ_FIXER[c.fix.who] ?? c.fix.who}): ${c.fix.action}` }) : null);
    })));
}

/** Runs the doctor's checks (the last is one real Work IQ call) and lists what they say. */
async function runWorkiqDoctor(button) {
  if (workiqChecking) return;
  const refocus = document.activeElement === button;
  workiqChecking = true;
  button.disabled = true;
  button.textContent = "Checking…";
  try {
    workiqReport = { ...(await api("/api/workiq/doctor", { method: "POST" })), at: Date.now() };
    renderWorkiqChecks();
  } catch (err) {
    toast(err.message, "error");
  } finally {
    workiqChecking = false;
  }
  // Its call counts against today's budget.
  await loadWorkiq().catch((err) => {
    renderWorkiq();
    toast(err.message, "error");
  });
  if (refocus && focusLost()) focusWorkiq("check");
}

/** The command that signs this agent in with the organization's own app, in a terminal. */
function workiqHostedCommand() {
  const login = workiq?.hint ?? "proofkin auth login workiq --profile <name>";
  return `${login} --tenant <id-or-domain> --client-id <app-id> --device-code`;
}

function openWorkiqSignIn() {
  // What was typed stays (a sign-in that failed is tried again as it was).
  $("#workiq-error").textContent = "";
  $("#workiq-account").removeAttribute("aria-invalid");
  $("#workiq-hosted-command").textContent = workiqHostedCommand();
  $("#workiq-dialog").showModal();
  $("#workiq-account").focus();
}

/** Signs this agent in through the Work IQ CLI, which Work IQ checks right away. */
async function submitWorkiqSignIn(e) {
  e.preventDefault();
  const button = $("#workiq-submit");
  if (button.disabled) return;
  const dialog = $("#workiq-dialog");
  const error = $("#workiq-error");
  const account = $("#workiq-account");
  error.textContent = "";
  account.removeAttribute("aria-invalid");
  if (account.validity.typeMismatch) {
    error.textContent = "Enter the account as an email address, e.g. ada@contoso.com, or leave it empty for the CLI's default account.";
    account.setAttribute("aria-invalid", "true");
    account.focus();
    return;
  }
  button.disabled = true;
  button.textContent = "Signing in…";
  try {
    const res = await api("/api/workiq/sign-in", {
      method: "POST",
      body: JSON.stringify({ account: account.value.trim(), cli_path: $("#workiq-cli-path").value.trim() }),
    });
    workiqReport = null;
    renderWorkiqChecks();
    await loadWorkiq().catch((err) => toast(err.message, "error"));
    if (dialog.open) dialog.close();
    toast(`Signed in to Work IQ: ${res.line ?? res.who}`);
  } catch (err) {
    // Closed while it ran: the error is the toast's.
    if (dialog.open) error.textContent = err.message;
    else toast(err.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = "Sign in";
    if (dialog.open && focusLost()) button.focus();
  }
}

function openWorkiqBudget() {
  const usage = workiq?.usage;
  $("#workiq-budget-error").textContent = "";
  for (const [id, value] of [["#workiq-per-day", usage?.per_day], ["#workiq-per-run", usage?.per_run]]) {
    $(id).value = value ?? "";
    $(id).removeAttribute("aria-invalid");
  }
  $("#workiq-budget-dialog").showModal();
  $("#workiq-per-day").focus();
}

/** What is wrong with a budget as typed, if anything: it is a whole number of calls from 1. */
function workiqCallsProblem(text) {
  if (!/^\d+$/.test(text) || Number(text) < 1) return "enter a whole number of calls, at least 1";
  if (Number(text) > 4294967295) return `enter at most ${(4294967295).toLocaleString()} calls`;
  return null;
}

/** Saves the Work IQ budgets: calls a day and per run. */
async function submitWorkiqBudget(e) {
  e.preventDefault();
  const button = $("#workiq-budget-submit");
  if (button.disabled) return;
  const dialog = $("#workiq-budget-dialog");
  const error = $("#workiq-budget-error");
  const fields = [[$("#workiq-per-day"), "Calls a day"], [$("#workiq-per-run"), "Calls per run"]];
  error.textContent = "";
  for (const [input] of fields) input.removeAttribute("aria-invalid");
  for (const [input, label] of fields) {
    const problem = workiqCallsProblem(input.value.trim());
    if (!problem) continue;
    error.textContent = `${label}: ${problem}.`;
    input.setAttribute("aria-invalid", "true");
    input.focus();
    return;
  }
  const [perDay, perRun] = fields.map(([input]) => Number(input.value.trim()));
  button.disabled = true;
  button.textContent = "Saving…";
  try {
    const res = await api("/api/workiq/budget", { method: "PUT", body: JSON.stringify({ per_day: perDay, per_run: perRun }) });
    if (dialog.open) dialog.close();
    toast(`Work IQ budget saved: ${nOf(res.per_day, "call")} a day, at most ${res.per_run} per run`);
    await loadWorkiq().catch((err) => toast(err.message, "error"));
  } catch (err) {
    if (dialog.open) error.textContent = err.message;
    else toast(err.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = "Save budget";
    if (dialog.open && focusLost()) button.focus();
  }
}

/** Signs this agent out of Work IQ once confirmed, saying what that stops and what stays. */
async function signOutWorkiq() {
  const ok = await confirmDialog({
    title: "Sign out of Work IQ?",
    body: "Plans on this agent, and digital workers that use its account, can't use Microsoft 365 work context until you sign in again. "
      + (workiq?.connection === "hosted" ? "Its token is removed from this machine." : "The Work IQ CLI itself stays signed in.")
      + " The budgets and the audit file stay.",
    confirm: "Sign out",
    danger: true,
  });
  if (!ok) return;
  try {
    await api("/api/workiq", { method: "DELETE" });
    workiqReport = null;
    renderWorkiqChecks();
    toast("Signed out of Work IQ");
    await loadWorkiq();
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- tracker ----------
let tracker = null;
async function loadTracker() {
  tracker = await api("/api/tracker");
  planTrack.sync();
  editorTrack.sync();
  const box = $("#tracker");
  box.replaceChildren();
  const btn = $("#tracker-btn");
  btn.hidden = !tracker.signed_in;
  btn.textContent = tracker.linked ? "Change" : "Link";
  btn.setAttribute("aria-label", tracker.linked ? "Change the tracker" : "Link a tracker");
  if (!tracker.linked) {
    box.append(el("p", { class: "muted small", text: "Track each plan run as a GitHub issue with a sub-issue per step, on your project's board: their status follows the run, and pull requests from the steps' branches close them." }));
    if (!tracker.signed_in) box.append(el("p", { class: "muted small", text: "Sign in to GitHub first (above)." }));
    return;
  }
  const t = tracker.tracker;
  const link = (href, text) => externalLink(href, text);
  box.append(el("p", { class: "small" }, "Issues in ", link(t.repo_url || `https://${t.host}/${t.repo}`, t.repo),
    t.login ? `, assigned to ${t.login}` : ""));
  if (tracker.error) {
    box.append(el("p", { class: "warn-text small", text: `GitHub can't be reached with it now: ${tracker.error}` }));
  } else if (t.board) {
    box.append(el("p", { class: "small" }, "On the board ", link(t.board.url, t.board.title),
      t.board.statuses.length ? ` (${t.board.statuses.join(", ")})` : ""));
  } else if (t.board_problem) {
    box.append(boardProblem(t));
  } else {
    box.append(el("p", { class: "muted small", text: "No board: issues only. Choose Change to add your project's address." }));
  }
  if (tracker.waiting_error) {
    box.append(el("p", { class: "warn-text small", text: `What the open runs wait for can't be read now: ${tracker.waiting_error}` }));
  }
  if (tracker.waiting?.length) box.append(waitingRuns(tracker.waiting));
  box.append(el("div", { class: "repo-actions" },
    el("button", { type: "button", class: "btn small", onclick: openTrackerItems }, t.board ? "Board items" : "Open issues"),
    t.board ? el("button", { type: "button", class: "btn small", onclick: (e) => addAllToBoard(t, e.currentTarget) }, "Add existing issues") : null,
    el("button", { type: "button", class: "btn small danger", onclick: unlinkTracker }, "Stop tracking")));
  // The GitHub card offers Grant access only when a linked tracker needs it.
  renderGithubScopes();
}

/** The run issues left open, what each waits for, and the button that checks now. */
function waitingRuns(runs) {
  return el("div", { class: "tracker-waiting small" },
    el("p", { class: "tracker-waiting-h", text: "Left open until their sub-issues close" }),
    el("ul", { class: "tracker-waiting-list" }, ...runs.map((run) => {
      const open = run.sub_issues.filter((s) => s.open);
      return el("li", {},
        externalLink(run.url, `#${run.number} ${run.title}`),
        `: ${run.sub_issues.length - open.length} of ${nOf(run.sub_issues.length, "sub-issue")} closed`,
        open.length ? el("ul", {}, ...open.map((s) => el("li", {},
          externalLink(s.url, `#${s.number} ${s.title}`),
          s.pull_requests.length ? `: pull request ${s.pull_requests.map((n) => `#${n}`).join(", ")}` : ": no pull request yet"))) : null);
    })),
    el("p", { class: "muted small", text: "Proofkin closes each one when all its sub-issues are closed. It looks every 10 minutes; Check now looks at once." }),
    el("button", { type: "button", class: "btn small", onclick: (e) => syncTracker(e.currentTarget) }, "Check now"));
}

/** Puts every issue and pull request of the tracker's repository on its board. */
async function addAllToBoard(t, button) {
  const ok = await confirmDialog({
    title: `Put ${t.repo}'s issues on the board?`,
    body: "Every issue and pull request goes on it: open issues to do, open pull requests in review, closed and merged ones done. Statuses already set stay.",
    confirm: "Put them on the board",
  });
  if (!ok) return;
  button.disabled = true;
  try {
    const res = await api("/api/tracker/add", { method: "POST", body: JSON.stringify({ open: true, closed: true }) });
    const added = res.added.filter((a) => !a.already).length;
    toast(`Put ${added} on ${t.board.title}; ${res.added.length - added} were on it already${res.problems.length ? ` (${nOf(res.problems.length, "problem")}: see the activity feed)` : ""}`);
  } catch (err) {
    toast(err.message, "error");
  } finally {
    button.disabled = false;
  }
}

/** Why the board isn't updated. A missing scope comes with Grant access (and the gh
 * commands to run in a terminal instead); anything else with the command it names. */
function boardProblem(t) {
  if (t.board_scope) {
    const commands = el("pre", { class: "mono" });
    // Filled in when opened: by then the GitHub card knows gh's active account.
    const terminal = el("details", {
      ontoggle: (e) => {
        if (!e.currentTarget.open) return;
        const active = github?.accounts?.find((a) => a.active && a.host === t.host)?.login;
        const away = !active || active.toLowerCase() !== t.login.toLowerCase();
        const lines = [
          ...(away ? [`gh auth switch --hostname ${t.host} --user ${t.login}`] : []),
          `gh auth refresh --hostname ${t.host} --scopes ${t.board_scope}`,
          ...(away ? [`gh auth switch --hostname ${t.host} --user ${active ?? "<the account you use>"}`] : []),
        ];
        commands.textContent = lines.join("\n");
      },
    }, el("summary", { text: "Or in a terminal" }), commands,
    el("button", { type: "button", class: "btn small", onclick: () => copy(commands.textContent, "Commands") }, icon(ICONS.copy), "Copy"));
    return el("div", { class: "tracker-problem small", role: "status" },
      el("p", {}, "The board isn't updated: ", el("strong", { text: t.login }), "'s GitHub token lacks the ",
        el("span", { class: "mono", text: t.board_scope }), " scope."),
      el("p", { class: "command" },
        el("button", { type: "button", class: "btn small primary", onclick: () => grantScope(t.board_scope) }, "Grant access"),
        el("span", { class: "muted", text: "You approve it on GitHub with a code." })),
      terminal);
  }
  const problem = t.board_problem;
  const command = problem.match(/`([^`]+)`/)?.[1];
  return el("div", { class: "tracker-problem small", role: "status" },
    el("p", { text: `The board isn't updated: ${command ? problem.replace(/`[^`]+`/, "the command below") : problem}` }),
    command ? el("p", { class: "command" }, el("code", { class: "mono", text: command }),
      el("button", { type: "button", class: "btn small", onclick: () => copy(command, "Command") }, icon(ICONS.copy), "Copy")) : null);
}

function openTrackerDialog() {
  $("#tracker-error").textContent = "";
  const t = tracker?.linked ? tracker.tracker : null;
  $("#tracker-repo").value = t?.repo ?? github?.repos?.[0]?.slug ?? "";
  $("#tracker-project").value = t?.board?.url ?? (t?.project ? `https://${t.host}/users/${t.project.owner}/projects/${t.project.number}` : "");
  $("#tracker-dialog").showModal();
  $("#tracker-repo").focus();
}

async function submitTracker(e) {
  e.preventDefault();
  const button = $("#tracker-submit");
  button.disabled = true;
  $("#tracker-error").textContent = "Checking with GitHub…";
  try {
    const res = await api("/api/tracker", {
      method: "PUT",
      body: JSON.stringify({ repo: $("#tracker-repo").value.trim(), project: $("#tracker-project").value.trim() || null }),
    });
    $("#tracker-dialog").close();
    toast(res.tracker.board ? `Plan runs are tracked on ${res.tracker.board.title}` : `Plan runs are tracked as issues of ${res.tracker.repo}`);
    await loadTracker();
  } catch (err) {
    $("#tracker-error").textContent = err.message;
  } finally {
    button.disabled = false;
  }
}

async function unlinkTracker() {
  const ok = await confirmDialog({
    title: "Stop tracking plan runs?",
    body: "New plan runs won't get issues. The issues made so far stay on GitHub.",
    confirm: "Stop tracking",
    danger: true,
  });
  if (!ok) return;
  try {
    await api("/api/tracker", { method: "DELETE" });
    await loadTracker();
  } catch (err) {
    toast(err.message, "error");
  }
}

async function syncTracker(button) {
  button.disabled = true;
  try {
    const res = await api("/api/tracker/sync", { method: "POST" });
    const stillOpen = res.waiting.reduce((n, run) => n + run.sub_issues.filter((s) => s.open).length, 0);
    toast(res.closed.length
      ? `Closed ${res.closed.map((n) => `#${n}`).join(", ")}: all their sub-issues are closed`
      : `Nothing could be closed yet: ${nOf(stillOpen, "sub-issue")} still open`);
    await loadTracker();
  } catch (err) {
    toast(err.message, "error");
  } finally {
    button.disabled = false;
  }
}

async function openTrackerItems() {
  const list = $("#tracker-items");
  list.replaceChildren(el("li", { class: "muted small", text: "Loading…" }));
  $("#tracker-items-desc").textContent = "";
  $("#tracker-items-dialog").showModal();
  $("#tracker-items-close").focus();
  try {
    const res = await api("/api/tracker/items");
    const open = res.items.filter((i) => i.open);
    $("#tracker-items-h").textContent = res.board ? "Board items" : "Open issues";
    $("#tracker-items-desc").textContent = res.board
      ? `${nOf(open.length, "open item")} on ${res.board.title}. Plan it starts a plan that works on the issue, a step per open sub-issue.`
      : `${nOf(open.length, "open issue")} of ${res.repo} (no board). Plan it starts a plan that works on the issue, a step per open sub-issue.`;
    const groups = new Map();
    for (const i of open) {
      const key = res.board ? i.status || "No status" : "Open";
      if (!groups.has(key)) groups.set(key, []);
      groups.get(key).push(i);
    }
    list.replaceChildren(...(groups.size ? [...groups].map(([status, items]) => el("li", { class: "tracker-group" },
      el("h3", { text: `${status} (${items.length})` }),
      el("ul", { class: "list" }, ...items.map((i) => trackerItemNode(i, res.items)))))
      : [el("li", { class: "muted small", text: "Nothing open." })]));
  } catch (err) {
    list.replaceChildren(el("li", { class: "form-error", role: "alert", text: err.message }));
  }
}

function trackerItemNode(i, all) {
  const facts = [
    i.kind === "pull_request" ? "pull request" : i.kind === "draft" ? "draft" : "",
    i.repo ?? "",
    i.agent ? `agent ${i.agent}` : "",
    i.assignees.length ? `assigned to ${i.assignees.join(", ")}` : "",
    i.sub_issues ? `${i.sub_issues[0]} of ${i.sub_issues[1]} sub-issues done` : "",
    i.parent ? `part of #${i.parent}` : "",
    i.pull_requests.length ? `closed by ${i.pull_requests.map((n) => `#${n}`).join(", ")} when merged` : "",
  ].filter(Boolean).join(" · ");
  const name = `${i.number ? `#${i.number} ` : ""}${i.title}`;
  return el("li", { class: "tracker-item" },
    el("div", { class: "task-top" },
      i.url ? externalLink(i.url, name, name, { class: "task-title" }) : el("span", { class: "task-title", text: name }),
      i.kind === "issue" && !i.repo ? el("button", {
        type: "button", class: "btn small", "aria-label": `Plan #${i.number}`, onclick: () => planFromItem(i, all),
      }, "Plan it") : null),
    facts ? el("p", { class: "muted small", text: facts }) : null);
}

/** A plan that works on issue `item`: a step per open sub-issue (or one for it). */
function planFromItem(item, all) {
  const workers = state.peers.filter((p) => p.capabilities.length);
  const share = workers.flatMap((p) => (p.shares ?? []).map((s) => ({ peer: p.name, ...s }))).find((s) => s.name !== "data");
  const target = share ? `@${share.peer}:${share.name}` : workers[0] ? `@${workers[0].name}` : "@peer:folder";
  const subs = all.filter((i) => i.kind === "issue" && !i.repo && i.open && i.parent === item.number);
  const lines = [`track: #${item.number}`, ...(subs.length
    ? subs.map((s) => `i${s.number} ${target} issue=#${s.number} ${s.title}`)
    : [`do ${target} ${item.title}`])];
  $("#tracker-items-dialog").close();
  document.querySelector('[data-mode="plan"]')?.click();
  $("#composer-input").value = lines.join("\n");
  state.playbook = null;
  renderPlanExplain();
  schedulePlanCheck();
  $("#composer-input").focus();
  toast(`A plan for #${item.number}: check who does each step and how it is checked, then run it`);
}

// ---------- audit log ----------
const OUTCOME_TEXT = { done: "Done", failed: "Failed", cancelled: "Cancelled", refused: "Refused" };
const OUTCOME_CLASS = { done: "done", failed: "failed", cancelled: "cancelled", refused: "failed" };

async function loadAudit() {
  const res = await api("/api/audit?limit=50");
  const panel = $("#audit-panel");
  panel.hidden = !res.enabled;
  if (!res.enabled) return;
  const list = $("#audit-list");
  list.replaceChildren();
  $("#audit-count").textContent = res.entries.length;
  if (!res.entries.length) {
    list.append(el("li", { class: "empty", text: "No peer has run a task on this agent yet." }));
  }
  for (const e of res.entries) {
    const access = e.access ? ACCESS_SHORT[e.access] ?? e.access : "no access";
    list.append(el("li", { class: "audit-item" },
      el("div", { class: "task-top" },
        el("span", { class: "task-title", text: `${e.peer} · #${e.task_id} · ${e.team ? "Team step" : providerLabel(e.provider)}${e.model ? ` · ${e.model}` : ""}` }),
        el("span", { class: `status ${OUTCOME_CLASS[e.outcome] ?? ""}`, text: OUTCOME_TEXT[e.outcome] ?? e.outcome })),
      el("div", { class: "task-meta", text: `${stamp(e.ts_ms)} · ${access} · ${duration(e.duration_ms)} · ${e.prompt_chars} chars, sha256 ${e.prompt_sha256.slice(0, 10)}` }),
      e.usage ? el("div", { class: "task-meta", title: "As its provider reported it", text: `Used ${usageText(e.usage)}` }) : null,
      e.origin ? el("div", { class: "task-meta", text: `Part of ${e.origin} on ${e.peer}` }) : null,
      e.error ? el("div", { class: e.outcome === "cancelled" ? "audit-note" : "audit-error", text: e.error }) : null));
  }
}

// ---------- composer ----------
function availableProviders() {
  const set = new Set(state.me?.providers || []);
  for (const p of state.peers) for (const c of p.capabilities) set.add(c);
  return [...set];
}

function renderTaskOptions() {
  const providerSel = $("#provider");
  const targetSel = $("#target");
  const prevProvider = providerSel.value;
  const prevTarget = targetSel.value;
  const providers = availableProviders();
  providerSel.replaceChildren(...providers.map((p) => el("option", { value: p, text: providerLabel(p) })));
  if (providers.includes(prevProvider)) providerSel.value = prevProvider;

  const provider = providerSel.value;
  const targets = [];
  if (provider && state.me?.providers.includes(provider)) targets.push(["local", "This agent"]);
  const serving = state.peers.filter((p) => p.capabilities.includes(provider));
  if (serving.length > 1) targets.push(["auto", "Any peer"]);
  for (const p of serving) targets.push([p.id, p.name]);
  // Squads with an online member that serves the provider: the task goes to the least busy.
  const squads = (state.squads ?? []).filter((s) => squadServing(s, provider).length)
    .map((s) => [`#${s.name}`, `#${s.name} · ${s.members.filter((m) => m.online).length}/${s.members.length} online`]);
  const option = ([v, label]) => el("option", { value: v, text: label });
  targetSel.replaceChildren(...targets.map(option),
    ...(squads.length ? [el("optgroup", { label: "Squads" }, ...squads.map(option))] : []));
  if ([...targets, ...squads].some(([v]) => v === prevTarget)) targetSel.value = prevTarget;
  renderShareOptions();
  renderModelOptions();
  updateComposer();
}

/** The connected members of `squad` that serve `provider`. */
function squadServing(squad, provider) {
  const names = new Set(squad.members.filter((m) => m.online).map((m) => m.name));
  return state.peers.filter((p) => names.has(p.name) && p.capabilities.includes(provider));
}

/** The squad the task form's Run on names, if it names one. */
function targetSquad() {
  const target = $("#target").value;
  return target.startsWith("#") ? (state.squads ?? []).find((s) => `#${s.name}` === target) : null;
}

// The Model list's choice that takes a model by name (EFFORTS and the model helpers are
// in ui/models.js).
const OTHER_MODEL = "*other*";
/** What the task form's Model and Reasoning lists offer, as last drawn (see modelMenu). */
let taskModels = null;

/** The agents a task to `target` may go to, as they say what they offer: this agent, a
 * peer, the connected peers that serve the provider (Any peer), or a squad's online
 * members that do. */
function targetAgents(target, provider) {
  if (target === "local") return state.me ? [state.me] : [];
  if (target === "auto") return state.peers.filter((p) => p.capabilities.includes(provider));
  const squad = targetSquad();
  if (squad) return squadServing(squad, provider);
  return state.peers.filter((p) => p.id === target);
}

/** The model and reasoning effort a task runs at: its agent's defaults (selected, named
 * when it says them), or a model it lets you pick (those it allows, else all its account
 * offers; one by name when it allows any and lists none). With Any peer or a squad,
 * what their peers allow together (the task goes to one that allows your pick). */
function renderModelOptions() {
  const provider = $("#provider").value;
  const target = $("#target").value;
  const agents = target ? targetAgents(target, provider) : [];
  const menu = modelMenu(agents, provider);
  taskModels = { menu, theirs: agents.length > 1 ? "Each peer's default" : "Its default" };
  const deflt = menu.defaultModel?.toLowerCase();
  const sel = $("#model");
  const prev = sel.value;
  sel.replaceChildren(
    el("option", { value: "", text: menu.defaultModel ? `${menu.defaultModel} (default)` : taskModels.theirs }),
    ...modelOptionNodes(menu.models.filter((m) => m.id.toLowerCase() !== deflt)),
    ...(menu.other ? [el("option", { value: OTHER_MODEL, text: "Another model…" })] : []));
  if ([...sel.options].some((o) => o.value === prev)) sel.value = prev;
  $("#model-field").hidden = !target;
  $("#model-name-field").hidden = !target || sel.value !== OTHER_MODEL;
  $("#effort-field").hidden = !target;
  renderEffortOptions();
}

/** The Reasoning list for the model picked: the levels its agent allows that the model
 * supports, with the default selected as the model takes it. */
function renderEffortOptions() {
  if (!taskModels) return;
  const chosen = $("#model").value;
  const pick = effortChoices(taskModels.menu, chosen === OTHER_MODEL ? $("#model-name").value.trim() : chosen);
  const effort = $("#effort");
  const prev = effort.value;
  effort.replaceChildren(
    el("option", { value: "", text: pick.fixed ? "None for this model" : pick.fitted ? `${pick.fitted} (default)` : taskModels.theirs }),
    ...pick.levels.filter((e) => e !== pick.fitted).map((e) => el("option", { value: e, text: e })));
  if ([...effort.options].some((o) => o.value === prev)) effort.value = prev;
  effort.disabled = pick.fixed;
}

function selectedModel() {
  if ($("#model-field").hidden) return null;
  const chosen = $("#model").value;
  return chosen === OTHER_MODEL ? $("#model-name").value.trim() || null : chosen || null;
}

function selectedEffort() {
  return $("#effort-field").hidden || $("#effort").disabled ? null : $("#effort").value || null;
}

/** Folder picker for a peer target: the folders that peer shares with this agent; for
 * a squad, those its members that serve the provider share (the task goes to one that
 * shares the folder). */
function renderShareOptions() {
  const sel = $("#share");
  const prev = sel.value;
  const peer = state.peers.find((p) => p.id === $("#target").value);
  const squad = targetSquad();
  let shares = peer?.shares ?? [];
  if (squad) {
    const members = squadServing(squad, $("#provider").value);
    const names = [...new Set(members.flatMap((m) => m.shares.map((s) => s.name)))];
    shares = names.map((name) => {
      const n = members.filter((m) => m.shares.some((s) => s.name === name)).length;
      return { name, label: `${name} (${n} of ${members.length} member${members.length === 1 ? "" : "s"})` };
    });
  }
  $("#share-field").hidden = !(peer || squad) || !shares.length;
  sel.replaceChildren(...shares.map((s) =>
    el("option", { value: s.name, text: s.label ?? `${s.name} (${(ACCESS_SHORT[s.access] ?? s.access).toLowerCase()})` })));
  if (shares.some((s) => s.name === prev)) sel.value = prev;
  renderMcpOptions();
  renderCheckOptions();
}

/** Checks of the chosen folder its peer's runtime runs after the model, as evidence.
 * A folder's own checks run its code, so they are offered only with full access. */
function renderCheckOptions() {
  const peer = state.peers.find((p) => p.id === $("#target").value);
  const share = (peer?.shares ?? []).find((s) => s.name === $("#share").value);
  const checks = $("#share-field").hidden || share?.access !== "full" ? [] : share?.checks ?? [];
  const checked = new Set(selectedChecks());
  $("#checks-field").hidden = !checks.length;
  $("#check-options").replaceChildren(...checks.map((name) =>
    el("label", { class: "mcp-chip", title: `${peer.name}'s runtime runs ${name} after the model: the task is proven only if it passes` },
      el("input", { type: "checkbox", value: name, checked: checked.has(name), onchange: syncAttempts }),
      el("span", { class: "mono", text: name }))));
  syncAttempts();
}

function selectedChecks() {
  if ($("#checks-field").hidden) return [];
  return [...$("#check-options").querySelectorAll("input:checked")].map((i) => i.value);
}

function syncAttempts() {
  $("#attempts-field").hidden = !selectedChecks().length;
}

/** MCP servers the chosen folder offers (Copilot and Claude tasks only), with what
 * their owner says each is for. */
function renderMcpOptions() {
  const peer = state.peers.find((p) => p.id === $("#target").value);
  const share = (peer?.shares ?? []).find((s) => s.name === $("#share").value);
  const servers = $("#share-field").hidden || $("#provider").value === "codex" ? [] : share?.mcp ?? [];
  const checked = new Set(selectedMcp());
  $("#mcp-field").hidden = !servers.length;
  $("#mcp-options").replaceChildren(...servers.map((name) => {
    const about = share?.mcp_about?.[name];
    return el("label", { class: "mcp-chip", title: about ? `${name}: ${about}` : `MCP server ${name}` },
      el("input", { type: "checkbox", value: name, checked: checked.has(name) }),
      el("span", { class: "mono", text: name }),
      about ? el("span", { class: "about", text: about }) : null);
  }));
}

function selectedMcp() {
  if ($("#mcp-field").hidden) return [];
  return [...$("#mcp-options").querySelectorAll("input:checked")].map((i) => i.value);
}

const PLAN_EXAMPLE = "fix @alice:app skill=implement-change verify=test claims=C-1 Make the parser accept empty input";

function updateComposer() {
  const task = state.mode === "task";
  const plan = state.mode === "plan";
  const input = $("#composer-input");
  const noProviders = task && !availableProviders().length;
  $("#task-opts").hidden = !task;
  $("#playbook-opts").hidden = !plan;
  // The layout follows the mode: in a plan the text box takes the full width.
  $("#composer").dataset.mode = state.mode;
  if (!plan) $("#plan-problems").hidden = true;
  renderPlanExplain();
  input.rows = plan ? 6 : 2;
  input.placeholder = plan
    ? `Start from a playbook, or write a plan: claim lines, then one step per line, e.g.\nclaim: C-1 The parser handles empty input\n${PLAN_EXAMPLE}`
    : task
      ? noProviders
        ? "No provider here yet: connect a peer that runs one, or create a digital worker (it runs on your own sign-ins)"
        : "Describe the task, e.g. “Add unit tests for the parser module”"
      : "Message all connected peers…";
  $("#composer-label").textContent = plan ? "Plan" : task ? "Task prompt" : "Message";
  $("#send-btn").querySelector("span").textContent = plan ? "Run plan" : task ? "Run" : "Send";
  $("#send-btn").disabled = noProviders || (task && !$("#target").value);
  $("#composer-hint").textContent = plan
    ? "Ctrl+Enter to run · claims are proven only by checks peers run (verify=) or a sign-off (approve) · the run is saved under Evidence runs"
    : "Enter to send · Shift+Enter for a new line";
  planTrack.sync();
}

async function submitComposer(e) {
  e?.preventDefault();
  const input = $("#composer-input");
  const text = input.value.trim();
  if (!text || $("#send-btn").disabled) return;
  // An existing issue chosen without its number: the choice asks for it.
  if (state.mode === "plan" && !planTrack.ready()) return;
  $("#send-btn").disabled = true;
  try {
    if (state.mode === "chat") {
      const res = await api("/api/chat", { method: "POST", body: JSON.stringify({ text }) });
      if (res.sent === 0) toast("No peers connected; nobody received the message");
    } else if (state.mode === "plan") {
      if (!(await checkPlan(text))) {
        toast("Fix the plan's errors first (listed under the plan)", "error");
        return;
      }
      const res = await api("/api/plans", { method: "POST", body: JSON.stringify({ text }) });
      const n = Object.keys(res.steps).length;
      toast(`Plan started: ${n} step${n === 1 ? "" : "s"}`);
      $("#plan-problems").hidden = true;
      state.playbook = null;
    } else {
      const checks = selectedChecks();
      const task = await api("/api/tasks", {
        method: "POST",
        body: JSON.stringify({
          provider: $("#provider").value,
          target: $("#target").value,
          share: $("#share-field").hidden ? null : $("#share").value,
          mcp: selectedMcp(),
          checks,
          attempts: checks.length ? Number($("#attempts").value) : null,
          model: selectedModel(),
          effort: selectedEffort(),
          prompt: text,
        }),
      });
      upsertTask(task);
      toast(checks.length
        ? `Task #${task.id} started on ${task.target}; it is proven only if ${checks.join(", ")} pass${checks.length === 1 ? "es" : ""}`
        : `Task #${task.id} started on ${task.target}`);
    }
    input.value = "";
  } catch (err) {
    toast(err.message, "error");
  } finally {
    updateComposer();
    input.focus();
  }
}

// ---------- allowlist ----------
const ACCESS_SHORT = { "read-only": "Read only", "workspace-write": "Edit files", full: "Full" };

function syncDelegateFields() {
  const on = $("#add-delegate").checked;
  $("#rate-field").hidden = !on;
  $("#access-field").hidden = !on;
  $("#shares-field").hidden = !on;
  const chosen = $("#add-access").value;
  const hint = [];
  if (chosen === "full") {
    hint.push("Shell commands run in an OS sandbox: writes only in the folder, no network.");
  }
  hint.push("Their tasks run only in the folders you tick below; files outside them and credential files (.env, keys, .ssh…) are always refused.");
  $("#access-hint").textContent = hint.join(" ");
}

/** A Microsoft 365 grant's labels and Work IQ tools show while its checkbox is ticked. */
function syncM365Fields() {
  $("#m365-grant-fields").hidden = !$("#add-m365").checked;
}

/** Ticks the folders in `selected` (default: the data folder). */
function renderShareChecks(selected = ["data"]) {
  const box = $("#add-shares");
  box.replaceChildren(...(state.me?.shares ?? []).map((s) =>
    el("label", { class: "check" },
      el("input", { type: "checkbox", value: s.name, checked: selected.includes(s.name) || undefined }),
      el("span", {}, el("strong", { text: s.name }), ` · at most ${(ACCESS_SHORT[s.max_access] ?? s.max_access).toLowerCase()} · `,
        el("span", { class: "mono", text: s.path })))));
}

// The allowlisted peer the dialog changes (null: it adds one), and what opened it.
let grantPeer = null;
let grantOpener = null;

/** The dialog adds a peer (`peer` null) or changes what `peer` may do. */
function setAddMode(peer) {
  grantPeer = peer;
  $("#add-h").textContent = peer ? `Change what ${peer.name} may do` : "Add a peer to the allowlist";
  $("#add-intro").hidden = !!peer;
  $("#grant-intro").hidden = !peer;
  $("#grant-intro").textContent = peer
    ? `${peer.name} stays ${peer.connected ? "connected" : "on the allowlist"}, and the change applies at once.`
    : "";
  $("#ticket-field").hidden = !!peer;
  $("#name-field").hidden = !!peer;
  $("#add-submit").textContent = peer ? "Save" : "Add peer";
}

/** `endpoint`: a peer's endpoint id to start from (one that tried to connect). */
function openAddDialog(endpoint) {
  $("#add-form").reset();
  setAddMode(null);
  grantOpener = null;
  renderShareChecks();
  syncDelegateFields();
  syncM365Fields();
  $("#add-m365-labels").removeAttribute("aria-invalid");
  $("#add-error").textContent = "";
  $("#add-dialog").showModal();
  if (typeof endpoint === "string") {
    $("#add-ticket").value = endpoint;
    $("#add-name").focus();
  } else {
    $("#add-ticket").focus();
  }
}

function openGrantDialog(peer, opener) {
  $("#add-form").reset();
  setAddMode(peer);
  grantOpener = opener;
  renderShareChecks(peer.shares?.length ? peer.shares : ["data"]);
  $("#add-delegate").checked = !!peer.can_delegate;
  $("#add-rate").value = peer.tasks_per_hour ?? 20;
  $("#add-access").value = peer.can_delegate ? peer.access ?? "read-only" : "read-only";
  $("#add-github").value = peer.github ?? "";
  $("#add-m365").checked = !!peer.m365;
  $("#add-m365-labels").value = peer.m365?.labels?.join(", ") ?? "";
  $("#add-m365-labels").removeAttribute("aria-invalid");
  $("#add-m365-tools").checked = !!peer.m365?.tools;
  syncDelegateFields();
  syncM365Fields();
  $("#add-error").textContent = "";
  $("#add-dialog").showModal();
  $("#add-delegate").focus();
}

async function submitAdd(e) {
  e.preventDefault();
  const button = $("#add-submit");
  if (button.disabled) return;
  const dialog = $("#add-dialog");
  const errorBox = $("#add-error");
  const labelsInput = $("#add-m365-labels");
  errorBox.textContent = "";
  labelsInput.removeAttribute("aria-invalid");
  const delegate = $("#add-delegate").checked;
  const grant = {
    delegate,
    tasks_per_hour: delegate ? Number($("#add-rate").value) || 20 : null,
    access: delegate ? $("#add-access").value : "read-only",
    shares: delegate ? [...document.querySelectorAll("#add-shares input:checked")].map((i) => i.value) : null,
  };
  // Empty forgets a peer's account when changing it; adding a peer again keeps the one it had.
  const github = $("#add-github").value.trim().replace(/^@/, "");
  const peer = grantPeer;
  const ticket = $("#add-ticket").value.trim();
  if (!peer && !ticket) {
    errorBox.textContent = "Paste the peer's ticket or endpoint id.";
    $("#add-ticket").focus();
    return;
  }
  // Unticked, changing a peer removes its grant (null, like --m365 off), and adding a peer again
  // keeps the one it had (left out, like no --m365).
  let m365 = peer ? null : undefined;
  if ($("#add-m365").checked) {
    const labels = labelsInput.value.split(",").map((l) => l.trim()).filter(Boolean);
    if (!labels.length) {
      errorBox.textContent = "Enter the sensitivity labels it may receive, e.g. General, Public, unlabeled.";
      labelsInput.setAttribute("aria-invalid", "true");
      labelsInput.focus();
      return;
    }
    m365 = { labels, tools: $("#add-m365-tools").checked };
  }
  button.disabled = true;
  button.textContent = peer ? "Saving…" : "Adding…";
  try {
    if (peer) {
      await api(`/api/trust/${encodeURIComponent(peer.id)}`, { method: "PUT", body: JSON.stringify({ ...grant, m365, github }) });
      if (dialog.open) dialog.close();
      toast(`Saved what ${peer.name} may do`);
    } else {
      const res = await api("/api/trust", {
        method: "POST",
        body: JSON.stringify({ ticket, name: $("#add-name").value.trim() || null, github: github || null, m365, ...grant }),
      });
      if (dialog.open) dialog.close();
      toast(`${res.name} added to the allowlist`);
    }
  } catch (err) {
    // Closed while it ran: the error is the toast's.
    if (dialog.open) errorBox.textContent = err.message;
    else toast(err.message, "error");
  } finally {
    button.disabled = false;
    button.textContent = grantPeer ? "Save" : "Add peer";
    if (dialog.open && focusLost()) button.focus();
  }
}

async function removePeer(t) {
  const ok = await confirmDialog({
    title: `Remove ${t.name} from the allowlist?`,
    body: "It is disconnected and can no longer connect or run tasks here. You can add it again with its ticket.",
    confirm: "Remove peer",
    danger: true,
  });
  if (!ok) return;
  try {
    await api(`/api/trust/${encodeURIComponent(t.id)}`, { method: "DELETE" });
    toast(`${t.name} removed`);
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- live updates ----------
async function loadState() {
  const s = await api("/api/state");
  Object.assign(state, {
    me: s.me, peers: s.peers, trusted: s.trusted, squads: s.squads, feed: s.feed, tasks: s.tasks,
    approvals: s.approvals, builtinChecks: s.builtin_checks ?? [],
  });
  renderMe();
  renderPeers();
  renderFeed();
  renderTasks();
  renderApprovals();
  await Promise.all([loadAudit(), loadRuns(), loadOps(), loadSchedules(), loadTools(), loadWorkers(), loadConnect()]);
}

let events = null;
function connectEvents() {
  const source = new EventSource(`/api/events?token=${encodeURIComponent(token)}`);
  events = source;
  source.onopen = () => loadState().catch((err) => toast(err.message, "error"));
  source.onmessage = (e) => {
    const msg = JSON.parse(e.data);
    if (msg.type === "feed") appendFeed(msg.item);
    else if (msg.type === "task") upsertTask(msg.task);
    else if (msg.type === "tasks") {
      state.tasks = msg.tasks;
      renderTasks();
    }
    else if (msg.type === "audit") loadAudit().catch((err) => toast(err.message, "error"));
    else if (msg.type === "runs") loadRuns().catch((err) => toast(err.message, "error"));
    else if (msg.type === "schedules") loadSchedules().catch((err) => toast(err.message, "error"));
    else if (msg.type === "tools") {
      loadTools().catch((err) => toast(err.message, "error"));
      // Skills and agents changed: a plan naming them is checked again.
      loadSkills().then(schedulePlanCheck).catch(() => {});
    }
    else if (msg.type === "ops") loadOps().catch((err) => toast(err.message, "error"));
    else if (msg.type === "workers") loadWorkers().catch((err) => toast(err.message, "error"));
    else if (msg.type === "approvals") {
      const before = state.approvals?.length ?? 0;
      state.approvals = msg.approvals;
      renderApprovals();
      if (msg.approvals.length > before) toast("A teammate asks for your approval");
    }
    else if (msg.type === "me") {
      state.me = msg.me;
      renderMe();
      renderTaskOptions();
    }
    else if (msg.type === "peers") {
      state.peers = msg.peers;
      state.trusted = msg.trusted;
      state.squads = msg.squads;
      renderPeers();
      renderSquads();
      renderWorkers();
    }
  };
  source.onerror = () => {
    if (authLost) return;
    const pill = $("#net-status");
    pill.dataset.state = "offline";
    pill.querySelector(".label").textContent = "Reconnecting…";
    // EventSource can't see status codes; probe once to tell "agent down" from "token rejected".
    fetch("/api/state", { headers: { Authorization: `Bearer ${token}` } })
      .then((res) => { if (res.status === 401) showAuthError(); })
      .catch(() => {});
  };
}

// ---------- wiring ----------
function init() {
  applyTheme();
  radioGroup($(".theme-switch"), (btn) => {
    localStorage.setItem(THEME_KEY, btn.dataset.themeChoice);
    applyTheme();
  });
  radioGroup($(".segmented"), (btn, viaKeyboard) => {
    state.mode = btn.dataset.mode;
    for (const b of document.querySelectorAll("[data-mode]")) {
      b.setAttribute("aria-checked", String(b === btn));
      b.tabIndex = b === btn ? 0 : -1;
    }
    renderTaskOptions();
    // Keyboard users stay in the switch (arrow keys); mouse users go straight to typing.
    if (!viaKeyboard) $("#composer-input").focus();
  });
  for (const b of document.querySelectorAll("[data-mode]")) b.tabIndex = b.dataset.mode === state.mode ? 0 : -1;

  $("#composer").addEventListener("submit", submitComposer);
  $("#composer-input").addEventListener("keydown", (e) => {
    if (e.key !== "Enter" || e.isComposing) return;
    // In a plan Enter starts a new step; Ctrl/Cmd+Enter runs it.
    if (state.mode === "plan" ? e.ctrlKey || e.metaKey : !e.shiftKey) submitComposer(e);
  });
  $("#add-squad-btn").addEventListener("click", () => openSquadDialog());
  $("#playbook-pick").addEventListener("click", openPlaybookPicker);
  $("#playbook-cancel").addEventListener("click", () => $("#playbook-dialog").close());
  $("#playbook-back").addEventListener("click", () => {
    setPlaybookStep("pick");
    $("#playbook-choices input:checked")?.focus();
  });
  $("#playbook-form").addEventListener("submit", submitPlaybook);
  $("#playbook-vars").addEventListener("input", (e) => e.target.removeAttribute("aria-invalid"));
  // Filled in: on to the plan, to review and run it; otherwise back to the opener.
  $("#playbook-dialog").addEventListener("close", (e) =>
    $(e.target.returnValue === "filled" ? "#composer-input" : "#playbook-pick").focus());
  $("#composer-input").addEventListener("input", () => {
    schedulePlanCheck();
    // An emptied plan no longer comes from a playbook.
    if (!$("#composer-input").value.trim()) state.playbook = null;
    if (!state.playbook) renderPlanExplain();
  });
  planTrack = trackChoice($("#plan-track"), $("#composer-input"),
    () => $("#composer-input").dispatchEvent(new Event("input", { bubbles: true })),
    () => state.mode === "plan");
  editorTrack = trackChoice($("#plan-editor-track"), $("#plan-editor-text"), () => {
    const editor = $("#plan-editor-text");
    if (planBuilder.active()) planBuilder.load(editor.value);
    editor.dispatchEvent(new Event("input", { bubbles: true }));
  }, () => $("#plan-editor").open);
  loadPlaybooks().catch((err) => toast(err.message, "error"));
  loadSkills().catch((err) => toast(err.message, "error"));
  loadGithub().catch((err) => toast(err.message, "error"));
  loadWorkiq().catch((err) => toast(err.message, "error"));
  loadTracker().catch((err) => toast(err.message, "error"));
  $("#tracker-btn").addEventListener("click", openTrackerDialog);
  $("#tracker-form").addEventListener("submit", submitTracker);
  $("#tracker-cancel").addEventListener("click", () => $("#tracker-dialog").close());
  $("#tracker-dialog").addEventListener("close", () => $("#tracker-btn").focus());
  $("#tracker-items-close").addEventListener("click", () => $("#tracker-items-dialog").close());
  $("#link-repo-btn").addEventListener("click", openRepoDialog);
  $("#repo-form").addEventListener("submit", submitRepo);
  $("#repo-cancel").addEventListener("click", () => $("#repo-dialog").close());
  $("#github-form").addEventListener("submit", submitGithubLogin);
  $("#github-cancel").addEventListener("click", () => $("#github-dialog").close());
  $("#github-new-btn").addEventListener("click", signInAnother);
  $("#gh-flow-copy").addEventListener("click", () => ghFlow && copy(ghFlow.code, "Code"));
  $("#gh-flow-cancel").addEventListener("click", () => $("#gh-flow-dialog").close());
  $("#gh-flow-dialog").addEventListener("close", ghFlowClosed);
  $("#workiq-form").addEventListener("submit", submitWorkiqSignIn);
  $("#workiq-cancel").addEventListener("click", () => $("#workiq-dialog").close());
  $("#workiq-dialog").addEventListener("close", () => focusWorkiq("sign-in"));
  $("#workiq-hosted-copy").addEventListener("click", () => copy($("#workiq-hosted-command").textContent, "Command"));
  $("#workiq-budget-form").addEventListener("submit", submitWorkiqBudget);
  $("#workiq-budget-cancel").addEventListener("click", () => $("#workiq-budget-dialog").close());
  $("#workiq-budget-dialog").addEventListener("close", () => focusWorkiq("budget"));
  for (const id of ["#workiq-account", "#workiq-per-day", "#workiq-per-run"]) {
    $(id).addEventListener("input", (e) => e.target.removeAttribute("aria-invalid"));
  }
  renderKbImportSources();
  $("#kb-import-btn").addEventListener("click", openKbImport);
  $("#kb-import-form").addEventListener("submit", submitKbImport);
  $("#kb-import-cancel").addEventListener("click", () => $("#kb-import-dialog").close());
  $("#kb-import-dialog").addEventListener("close", () => $("#kb-import-btn").focus());
  $("#kb-import-sources").addEventListener("change", onKbImportSource);
  for (const id of ["#kb-import-topic", "#kb-import-query"]) {
    $(id).addEventListener("input", (e) => e.target.removeAttribute("aria-invalid"));
  }
  $("#pr-form").addEventListener("submit", submitPr);
  $("#pr-cancel").addEventListener("click", () => $("#pr-dialog").close());
  // Monitors run in the background: refresh the health panel now and then.
  setInterval(() => { if (!document.hidden) loadOps().catch(() => {}); }, 20000);
  $("#run-close").addEventListener("click", () => $("#run-dialog").close());
  $("#pull-cancel").addEventListener("click", () => $("#pull-dialog").close());
  $("#pull-form").addEventListener("submit", submitPull);
  $("#squad-cancel").addEventListener("click", () => $("#squad-dialog").close());
  // Back to the button that opened it (a squad's own, if it is still there).
  $("#squad-dialog").addEventListener("close", () => {
    const back = squadOpener?.isConnected ? squadOpener
      : [...document.querySelectorAll("#squad-list button")].find((b) => b.getAttribute("aria-label") === squadOpener?.getAttribute("aria-label"));
    (back ?? $("#add-squad-btn")).focus();
  });
  $("#squad-form").addEventListener("submit", submitSquad);
  $("#team-cancel").addEventListener("click", () => $("#team-dialog").close());
  $("#team-form").addEventListener("submit", submitTeamPlan);
  $("#team-remove").addEventListener("click", removeTeamPlan);
  $("#team-who").addEventListener("change", showTeamBy);
  $("#clear-tasks-btn").addEventListener("click", clearTasks);
  $("#add-monitor-btn").addEventListener("click", openMonitorDialog);
  $("#monitor-cancel").addEventListener("click", () => $("#monitor-dialog").close());
  $("#monitor-dialog").addEventListener("close", () => $("#add-monitor-btn").focus());
  $("#monitor-form").addEventListener("submit", submitMonitor);
  $("#monitor-share").addEventListener("change", renderMonitorChecks);
  $("#monitor-check").addEventListener("change", renderMonitorCommand);
  $("#monitor-name").addEventListener("input", (e) => {
    e.target.dataset.typed = "1";
    e.target.removeAttribute("aria-invalid");
  });
  $("#schedule-open").addEventListener("click", () => planTrack.ready() && openScheduleDialog());
  $("#schedule-cancel").addEventListener("click", () => $("#schedule-dialog").close());
  $("#schedule-form").addEventListener("submit", submitSchedule);
  $("#when-repeat").addEventListener("change", syncWhenPicker);
  for (const id of ["#when-time", "#when-every", "#when-unit", "#when-custom"]) $(id).addEventListener("input", previewWhen);
  $("#when-days").addEventListener("change", previewWhen);
  $("#confirm-cancel").addEventListener("click", () => $("#confirm-dialog").close(""));
  $("#tools-open").addEventListener("click", () => openTools());
  $("#plan-expand").addEventListener("click", openPlanEditor);
  $("#plan-editor-close").addEventListener("click", () => $("#plan-editor").close());
  $("#plan-editor-text").addEventListener("input", onPlanEditorInput);
  $("#plan-editor-text").addEventListener("keydown", (e) => {
    if (e.key === "Enter" && (e.ctrlKey || e.metaKey)) {
      e.preventDefault();
      $("#plan-editor-run").click();
    }
  });
  const editorToComposer = () => { $("#composer-input").value = $("#plan-editor-text").value; };
  // However the editor closes, the plan goes back to the composer.
  $("#plan-editor").addEventListener("close", () => {
    editorToComposer();
    if (!$("#composer-input").value.trim()) state.playbook = null;
    renderPlanExplain();
    schedulePlanCheck();
  });
  $("#plan-editor-run").addEventListener("click", () => {
    if (!planBuilder.ready() || !editorTrack.ready()) return;
    editorToComposer();
    $("#plan-editor").close();
    submitComposer();
  });
  $("#plan-editor-schedule").addEventListener("click", () => {
    if (!planBuilder.ready() || !editorTrack.ready()) return;
    editorToComposer();
    $("#plan-editor").close();
    openScheduleDialog();
  });
  for (const id of ["#add-dialog", "#squad-dialog", "#schedule-dialog", "#run-dialog", "#playbook-dialog",
    "#github-dialog", "#repo-dialog", "#pr-dialog", "#plan-editor", "#tools-dialog", "#confirm-dialog", "#worker-dialog",
    "#monitor-dialog", "#team-dialog", "#tracker-dialog", "#tracker-items-dialog", "#gh-flow-dialog", "#models-dialog",
    "#branch-dialog", "#pull-dialog", "#workiq-dialog", "#workiq-budget-dialog", "#kb-import-dialog"]) guardDialog($(id));
  initModelsDialog();
  initBranchReview();
  $("#provider").addEventListener("change", renderTaskOptions);
  $("#target").addEventListener("change", () => { renderShareOptions(); renderModelOptions(); updateComposer(); });
  $("#model").addEventListener("change", () => {
    const other = $("#model").value === OTHER_MODEL;
    $("#model-name-field").hidden = !other;
    renderEffortOptions();
    if (other) $("#model-name").focus();
  });
  $("#share").addEventListener("change", () => { renderMcpOptions(); renderCheckOptions(); });
  $("#me-id").addEventListener("click", () => state.me && copy(state.me.id, "Endpoint id"));
  $("#copy-ticket").addEventListener("click", copyTicket);
  $("#add-peer-btn").addEventListener("click", () => openAddDialog());
  $("#add-cancel").addEventListener("click", () => $("#add-dialog").close());
  // Back to the button that opened it: a peer's Change… (rebuilt by then: found by its label), or Add peer.
  $("#add-dialog").addEventListener("close", () => {
    const label = grantOpener?.getAttribute("aria-label");
    const back = grantOpener?.isConnected ? grantOpener
      : label ? [...document.querySelectorAll("#trust-list button")].find((b) => b.getAttribute("aria-label") === label) : null;
    (back ?? $("#add-peer-btn")).focus();
  });
  $("#add-form").addEventListener("submit", submitAdd);
  $("#add-delegate").addEventListener("change", syncDelegateFields);
  $("#add-access").addEventListener("change", syncDelegateFields);
  $("#add-m365").addEventListener("change", syncM365Fields);
  setInterval(() => {
    for (const span of document.querySelectorAll(".elapsed[data-start]")) {
      span.textContent = `running ${duration(Date.now() - Number(span.dataset.start))}`;
    }
  }, 1000);

  if (!token) {
    $("#feed").replaceChildren(el("div", { class: "feed-empty" },
      el("h2", { text: "Access token missing" }),
      el("p", { text: "Open the full link that Proofkin printed when it started (it ends with #t=…)." })));
    return;
  }
  loadState().then(connectEvents).catch((err) => toast(err.message, "error"));
}

init();
