// Cases for the agent factory's dialog (ui/factory.js): the Deployments form says why it
// can't deploy before it sends, Evaluate shows the last evaluation's result, and approving a
// release redraws Deployments. tests/ui.rs runs this with node, on a small fake page: the
// script runs with the page's own el() and $ from ui/app.js.
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

// ---------- a fake page: what factory.js uses of the DOM ----------
class Node {
  constructor() {
    this.parentNode = null;
  }
  remove() {
    const parent = this.parentNode;
    if (!parent) return;
    parent.childNodes.splice(parent.childNodes.indexOf(this), 1);
    this.parentNode = null;
  }
}

class Text extends Node {
  constructor(data) {
    super();
    this.data = String(data);
  }
  get textContent() {
    return this.data;
  }
  set textContent(v) {
    this.data = String(v);
  }
}

const kebab = (s) => s.replace(/[A-Z]/g, (c) => `-${c.toLowerCase()}`);

class Element extends Node {
  constructor(tag) {
    super();
    this.localName = tag.toLowerCase();
    this.attrs = new Map();
    this.childNodes = [];
    this.listeners = {};
    this.tabIndex = 0;
    this.dataset = new Proxy({}, {
      get: (_, k) => (typeof k === "string" ? this.getAttribute(`data-${kebab(k)}`) ?? undefined : undefined),
      set: (_, k, v) => {
        this.setAttribute(`data-${kebab(k)}`, v);
        return true;
      },
    });
  }
  getAttribute(k) {
    return this.attrs.has(k) ? this.attrs.get(k) : null;
  }
  setAttribute(k, v) {
    this.attrs.set(k, String(v));
  }
  removeAttribute(k) {
    this.attrs.delete(k);
  }
  hasAttribute(k) {
    return this.attrs.has(k);
  }
  get id() {
    return this.getAttribute("id") ?? "";
  }
  set id(v) {
    this.setAttribute("id", v);
  }
  get className() {
    return this.getAttribute("class") ?? "";
  }
  set className(v) {
    this.setAttribute("class", v);
  }
  get children() {
    return this.childNodes.filter((c) => c instanceof Element);
  }
  get nextElementSibling() {
    const siblings = this.parentNode?.children ?? [];
    return siblings[siblings.indexOf(this) + 1] ?? null;
  }
  get previousElementSibling() {
    const siblings = this.parentNode?.children ?? [];
    return siblings[siblings.indexOf(this) - 1] ?? null;
  }
  get textContent() {
    return this.childNodes.map((c) => c.textContent).join("");
  }
  set textContent(v) {
    this.replaceChildren(...(v === "" || v == null ? [] : [String(v)]));
  }
  get isConnected() {
    let top = this;
    while (top.parentNode) top = top.parentNode;
    return top === document;
  }
  get checked() {
    return this._checked ?? this.hasAttribute("checked");
  }
  set checked(v) {
    this._checked = Boolean(v);
  }
  get selected() {
    return this._selected ?? this.hasAttribute("selected");
  }
  set selected(v) {
    this._selected = Boolean(v);
  }
  get value() {
    if (this.localName === "select") {
      const options = this.querySelectorAll("option");
      return (options.find((o) => o.selected) ?? options[0])?.value ?? "";
    }
    if (this.localName === "option") return this.getAttribute("value") ?? this.textContent;
    if (this._value !== undefined) return this._value;
    return this.getAttribute("value") ?? (this.getAttribute("type") === "checkbox" ? "on" : "");
  }
  set value(v) {
    if (this.localName === "select") {
      for (const o of this.querySelectorAll("option")) o.selected = o.value === String(v);
    } else {
      this._value = String(v);
    }
  }
  append(...items) {
    for (const item of items) {
      const node = typeof item === "string" ? new Text(item) : item;
      node.remove();
      node.parentNode = this;
      this.childNodes.push(node);
    }
  }
  replaceChildren(...items) {
    for (const c of this.childNodes) c.parentNode = null;
    this.childNodes = [];
    this.append(...items);
  }
  replaceWith(...items) {
    const parent = this.parentNode;
    if (!parent) return;
    const nodes = items.map((i) => (typeof i === "string" ? new Text(i) : i));
    for (const n of nodes) if (n !== this) n.remove();
    parent.childNodes.splice(parent.childNodes.indexOf(this), 1, ...nodes);
    this.parentNode = null;
    for (const n of nodes) n.parentNode = parent;
  }
  contains(node) {
    for (let n = node; n; n = n.parentNode) if (n === this) return true;
    return false;
  }
  matches(selector) {
    return parseSelectors(selector).some((parts) => matchesComplex(this, parts));
  }
  closest(selector) {
    for (let n = this; n instanceof Element; n = n.parentNode) if (n.matches(selector)) return n;
    return null;
  }
  querySelectorAll(selector) {
    const list = parseSelectors(selector);
    const found = [];
    const walk = (n) => {
      for (const c of n.children) {
        if (list.some((parts) => matchesComplex(c, parts))) found.push(c);
        walk(c);
      }
    };
    walk(this);
    return found;
  }
  querySelector(selector) {
    return this.querySelectorAll(selector)[0] ?? null;
  }
  addEventListener(type, fn) {
    (this.listeners[type] ??= []).push(fn);
  }
  removeEventListener(type, fn) {
    this.listeners[type] = (this.listeners[type] ?? []).filter((f) => f !== fn);
  }
  // Calls the listeners as the page would (bubbling up); what they return is kept on the
  // event, so a test can wait for an async handler to end.
  dispatchEvent(event) {
    event.target ??= this;
    event.pending ??= [];
    for (let n = this; n && !event.stopped; n = n.parentNode) {
      event.currentTarget = n;
      for (const fn of n.listeners?.[event.type] ?? []) {
        const result = fn.call(n, event);
        if (result && typeof result.then === "function") event.pending.push(result);
      }
      if (!["click", "submit", "change", "input", "keydown"].includes(event.type)) break;
    }
    return !event.defaultPrevented;
  }
  focus() {
    document.activeElement = this;
  }
  showModal() {
    this.setAttribute("open", "");
  }
  close() {
    this.removeAttribute("open");
    this.dispatchEvent(newEvent("close"));
  }
}
for (const k of ["hidden", "disabled", "open"]) {
  Object.defineProperty(Element.prototype, k, {
    get() {
      return this.hasAttribute(k);
    },
    set(v) {
      if (v) this.setAttribute(k, "");
      else this.removeAttribute(k);
    },
  });
}

// Selectors: lists of compounds (tag, #id, .class, [attr], [attr=v], [attr="v"], :checked,
// :not(...)) joined by descendant combinators.
function splitTop(s, comma) {
  const parts = [];
  let depth = 0;
  let quote = null;
  let part = "";
  for (const ch of s) {
    if (quote) {
      part += ch;
      if (ch === quote) quote = null;
      continue;
    }
    if (ch === '"' || ch === "'") quote = ch;
    else if (ch === "[" || ch === "(") depth += 1;
    else if (ch === "]" || ch === ")") depth -= 1;
    if (depth === 0 && (comma ? ch === "," : /\s/.test(ch))) {
      if (part.trim()) parts.push(part.trim());
      part = "";
      continue;
    }
    part += ch;
  }
  if (part.trim()) parts.push(part.trim());
  return parts;
}

function parseCompound(s) {
  const c = { tag: null, id: null, classes: [], attrs: [], checked: false, not: [] };
  let i = 0;
  const name = () => {
    const m = /^[\w-]+/.exec(s.slice(i));
    if (!m) throw new Error(`the fake page can't read the selector ${s}`);
    i += m[0].length;
    return m[0];
  };
  while (i < s.length) {
    if (s[i] === "#") {
      i += 1;
      c.id = name();
    } else if (s[i] === ".") {
      i += 1;
      c.classes.push(name());
    } else if (s[i] === "[") {
      const end = s.indexOf("]", i);
      const body = s.slice(i + 1, end);
      const eq = body.indexOf("=");
      c.attrs.push(eq < 0 ? [body.trim(), null] : [body.slice(0, eq).trim(), body.slice(eq + 1).trim().replace(/^(["'])(.*)\1$/, "$2")]);
      i = end + 1;
    } else if (s.startsWith(":checked", i)) {
      c.checked = true;
      i += ":checked".length;
    } else if (s.startsWith(":not(", i)) {
      const end = s.indexOf(")", i);
      c.not.push(parseCompound(s.slice(i + ":not(".length, end)));
      i = end + 1;
    } else {
      c.tag = name().toLowerCase();
    }
  }
  return c;
}

function parseSelectors(selector) {
  return splitTop(selector, true).map((complex) => splitTop(complex, false).map((part) => {
    if (part === ">" || part === "+" || part === "~") throw new Error(`the fake page can't read the selector ${selector}`);
    return parseCompound(part);
  }));
}

function matchesCompound(n, c) {
  if (!(n instanceof Element)) return false;
  if (c.tag && n.localName !== c.tag) return false;
  if (c.id && n.getAttribute("id") !== c.id) return false;
  const classes = n.className.split(/\s+/);
  if (c.classes.some((k) => !classes.includes(k))) return false;
  if (c.attrs.some(([k, v]) => !n.hasAttribute(k) || (v !== null && n.getAttribute(k) !== v))) return false;
  if (c.checked && !n.checked) return false;
  return !c.not.some((x) => matchesCompound(n, x));
}

function matchesComplex(n, parts) {
  if (!matchesCompound(n, parts[parts.length - 1])) return false;
  let i = parts.length - 2;
  for (let up = n.parentNode; i >= 0 && up instanceof Element; up = up.parentNode) {
    if (matchesCompound(up, parts[i])) i -= 1;
  }
  return i < 0;
}

function newEvent(type) {
  return {
    type,
    defaultPrevented: false,
    preventDefault() {
      this.defaultPrevented = true;
    },
    stopPropagation() {
      this.stopped = true;
    },
  };
}

const html = new Element("html");
const body = new Element("body");
html.append(body);
const document = {
  documentElement: html,
  body,
  activeElement: body,
  createElement: (tag) => new Element(tag),
  createElementNS: (_, tag) => new Element(tag),
  createTextNode: (data) => new Text(data),
  querySelector: (s) => html.querySelector(s),
  querySelectorAll: (s) => html.querySelectorAll(s),
  getElementById(id) {
    const walk = (n) => {
      for (const c of n.children) {
        if (c.getAttribute("id") === id) return c;
        const found = walk(c);
        if (found) return found;
      }
      return null;
    };
    return walk(html);
  },
};
html.parentNode = document;
const $ = (s) => document.querySelector(s);

/// Fires `type` on `node` and waits for its handlers to end.
async function fire(node, type) {
  const event = newEvent(type);
  node.dispatchEvent(event);
  await Promise.all(event.pending);
}

// ---------- the page's API, answered by `routes` ----------
const calls = [];
let routes = {};
async function api(p, options = {}) {
  const method = options.method ?? "GET";
  calls.push({ method, path: p, body: options.body === undefined ? undefined : JSON.parse(options.body) });
  const answer = routes[`${method} ${p}`];
  if (answer === undefined) throw new Error(`404 Not Found: ${method} ${p}`);
  if (answer instanceof Error) throw answer;
  return JSON.parse(JSON.stringify(answer));
}
const posts = () => calls.filter((c) => c.method === "POST");

// ---------- the scripts, loaded as the page does ----------
const ui = path.join(__dirname, "..", "ui");
const app = fs.readFileSync(path.join(ui, "app.js"), "utf8");
const helpers = [/^const \$ = .*$/m, /^function el\(tag[\s\S]*?\r?\n\}\r?\n/m].map((re) => {
  const found = re.exec(app);
  assert.ok(found, `app.js has ${re}`);
  return found[0];
});
const sandbox = {
  document,
  Node,
  CSS: { escape: (s) => String(s) },
  state: { me: { profile: "boss" }, approvals: [] },
  api,
  toast: () => {},
  confirmDialog: async () => true,
  stamp: (ms) => new Date(ms).toISOString(),
  providerLabel: (p) => p,
  ACCESS_SHORT: { "read-only": "Read only", "workspace-write": "Edit files", full: "Full" },
  renderMarkdown: (text) => {
    const box = new Element("div");
    box.textContent = text;
    return box;
  },
  setTimeout,
  clearTimeout,
  console,
};
vm.createContext(sandbox);
vm.runInContext(`"use strict";\n${helpers.join("\n")}`, sandbox, { filename: "app.js" });
const el = (...args) => sandbox.el(...args);

// What initFactory() wires up when the script loads (ui/index.html).
const TABS = ["overview", "blueprint", "evaluate", "reports", "releases", "deployments", "metrics"];
body.append(
  el("div", { id: "factory-list" }),
  el("button", { type: "button", id: "factory-new-btn" }, "New agent"),
  el("dialog", { id: "factory-new-dialog" },
    el("form", { id: "factory-new-form" },
      el("p", { id: "factory-new-progress" }),
      el("p", { id: "factory-new-error" }),
      el("button", { type: "button", id: "factory-new-cancel" }, "Cancel"))),
  el("dialog", { id: "factory-agent-dialog" },
    el("h2", { id: "factory-agent-h" }),
    el("button", { type: "button", id: "factory-agent-close" }, "Close"),
    el("div", { role: "tablist" }, ...TABS.map((t) => el("button", {
      type: "button", role: "tab", id: `ftab-${t}`, "data-tab": t, "aria-controls": `fpanel-${t}`,
    }, t))),
    ...TABS.map((t) => el("section", { role: "tabpanel", id: `fpanel-${t}` },
      ...(t === "blueprint"
        ? [el("div", { id: "factory-edit-mode" }), el("div", { id: "factory-form-box" }), el("div", { id: "factory-json-box" })]
        : [])))),
);
vm.runInContext(fs.readFileSync(path.join(ui, "factory.js"), "utf8"), sandbox, { filename: "factory.js" });
const F = vm.runInContext(
  "({ factoryState, renderFactoryDeployments, renderFactoryEvaluate, renderFactoryReleases, selectFactoryTab, factoryLastText })",
  sandbox,
);

// ---------- an agent ----------
const NAME = "ux-triage";
const AGENT = `/api/factory/agents/${NAME}`;
const route = { provider: "copilot", model: "claude-opus-5.5", effort: "max" };
const release = (version, folders, more = {}) => ({
  version, state: "released", created: "2025-05-02T10:00:00Z", signer: "boss", evaluation: "ev-1",
  folders, approvals: [], pending: [], ...more,
});
const agentWith = (more) => ({
  name: NAME, title: "Support triage", state: "active", active_version: "1.0.0",
  releases: [release("1.0.0", [])], deployments: [], evaluations: [], history: [],
  candidates: [route], left_out: [], ...more,
});
function open(agent, draft = { blueprint: { deploy: { folders: [] } } }) {
  F.factoryState.open = NAME;
  F.factoryState.agent = agent;
  F.factoryState.draft = draft;
  routes = { [`GET ${AGENT}`]: agent, [`GET /api/factory/drafts/${NAME}`]: draft };
  calls.length = 0;
}
const form = () => $("#fpanel-deployments form");
const formError = () => $("#fpanel-deployments form .form-error");
const hint = (tail) => `It works in the folders you tick (and its own data folder). None ticked: ${tail}`;
const SHARED = [{ name: "reports", access: "read-only" }, { name: "ledger", access: "workspace-write" }];

(async () => {
  // No folder to offer, and its release names none: it says so before anything is clicked,
  // and Deploy is disabled; Enter in the form sends nothing either.
  open(agentWith({ choices: { folders: [] } }));
  F.renderFactoryDeployments();
  const none = $("#factory-dep-none");
  assert.ok(!none.hidden);
  assert.strictEqual(none.textContent, "You share no folders yet, and a deployment works in at least one: share one with proofkin share add NAME PATH --profile boss, then deploy it here.");
  assert.strictEqual(none.querySelector(".mono").textContent, "proofkin share add NAME PATH --profile boss");
  assert.ok($("#factory-deploy-btn").disabled);
  assert.strictEqual($("#factory-deploy-btn").getAttribute("aria-describedby"), "factory-dep-none");
  assert.strictEqual($("#fcfg-dep-folders-hint").textContent, hint("its release names none"));
  await fire(form(), "submit");
  assert.deepStrictEqual(posts(), []);

  // Folders offered, none ticked, and its release names none: refused before it sends, with
  // focus on the first folder; one ticked is sent.
  open(agentWith({ choices: { folders: SHARED } }));
  F.renderFactoryDeployments();
  assert.ok($("#factory-dep-none").hidden);
  assert.ok(!$("#factory-deploy-btn").disabled);
  assert.strictEqual($("#factory-deploy-btn").getAttribute("aria-describedby"), null);
  await fire(form(), "submit");
  assert.strictEqual(formError().textContent, "Tick at least one folder it works in: its release names none");
  const boxes = form().querySelectorAll('input[name="fcfg-dep-folders"]');
  assert.deepStrictEqual(boxes.map((b) => b.value), ["reports", "ledger"]);
  assert.strictEqual(document.activeElement, boxes[0]);
  assert.deepStrictEqual(posts(), []);
  boxes[1].checked = true;
  routes[`POST ${AGENT}/deployments`] = { version: "1.0.0", worker: NAME };
  await fire(form(), "submit");
  assert.strictEqual(formError().textContent, "");
  assert.deepStrictEqual(posts().map((c) => c.body), [{ version: "1.0.0", canary: true, start: true, folders: ["ledger"] }]);

  // Its folders aren't known (its draft doesn't parse): the route answers, in its words,
  // and Deploy can be clicked again.
  open(agentWith({}));
  F.renderFactoryDeployments();
  assert.ok($("#factory-dep-none").hidden);
  const refused = "ux-triage's release names no folders to work in: pick at least one folder you share";
  routes[`POST ${AGENT}/deployments`] = new Error(refused);
  await fire(form(), "submit");
  assert.deepStrictEqual(posts().map((c) => c.body), [{ version: "1.0.0", canary: true, start: true }]);
  assert.strictEqual(formError().textContent, refused);
  assert.ok(!$("#factory-deploy-btn").disabled);
  assert.strictEqual($("#factory-deploy-btn").textContent, "Deploy");
  assert.strictEqual(document.activeElement, $("#factory-deploy-btn"));

  // The hint names the folders the picked release names, and follows another pick.
  open(agentWith({ releases: [release("1.1.0", []), release("1.0.0", ["reports", "ledger"])], choices: { folders: [] } }));
  F.renderFactoryDeployments();
  const version = $("#factory-dep-version");
  assert.strictEqual(version.value, "1.0.0");
  assert.strictEqual($("#fcfg-dep-folders-hint").textContent, hint("the folders its release names (reports, ledger)"));
  assert.ok($("#factory-dep-none").hidden);
  assert.ok(!$("#factory-deploy-btn").disabled);
  version.value = "1.1.0";
  await fire(version, "change");
  assert.strictEqual($("#fcfg-dep-folders-hint").textContent, hint("its release names none"));
  assert.ok(!$("#factory-dep-none").hidden);
  assert.ok($("#factory-deploy-btn").disabled);

  // Evaluate shows how the last evaluation ended, until another starts, with the way to its
  // report (a deployment's canary isn't one of them).
  assert.strictEqual(F.factoryLastText({ state: "passed", recommended: route }), "Passed: it recommends copilot claude-opus-5.5 (max reasoning)");
  const failed = {
    id: "ev-2", state: "failed", running: false, started: "2025-05-02T09:00:00Z", finished: "2025-05-02T09:20:00Z",
    done: 9, total: 9, recommended: null, error: null, canary: null,
    gate: { passed: false, reasons: ["copilot claude-opus-5.5 (max reasoning): 67% of its runs passed; the gate needs 90%"] },
    gates: [{ candidate: route, passed: false, reasons: ["67% of its runs passed; the gate needs 90%"] }],
  };
  const canary = { ...failed, id: "ev-3", state: "passed", canary: { version: "1.0.0" } };
  const older = { ...failed, id: "ev-1", state: "passed", recommended: route };
  open(agentWith({ evaluations: [canary, failed, older] }), { blueprint: { evals: { repetitions: 3 }, deploy: { folders: [] } } });
  F.renderFactoryEvaluate();
  assert.strictEqual($("#fpanel-evaluate #factory-last-text").textContent, "Failed: 67% of its runs passed; the gate needs 90%");
  assert.match($("#factory-last").textContent, /Its last evaluation · 2025-05-02T09:20:00.000Z · ev-2/);
  assert.ok($("#factory-run"), "and its form, to start another");
  // Its report, even when another was picked under Evaluations.
  F.factoryState.reportPicked = true;
  F.factoryState.report = { id: "ev-1" };
  routes["GET /api/factory/evaluations/ev-2"] = new Error("its report isn't needed here");
  await fire($("#factory-last-open"), "click");
  assert.strictEqual(F.factoryState.reportPicked, false);
  assert.strictEqual($("#factory-report-pick").value, "ev-2");
  assert.ok(!$("#fpanel-reports").hidden);
  assert.strictEqual(document.activeElement, $("#ftab-reports"));
  F.factoryState.agent.evaluations.unshift({ ...failed, id: "ev-4", state: "running", running: true, done: 1, gates: [] });
  F.renderFactoryEvaluate();
  assert.strictEqual($("#factory-last"), null);
  assert.ok($("#factory-progress"));

  // Approving a release here redraws Deployments at once: no "Nothing to deploy yet" left
  // until the factory's event comes.
  const waiting = release("1.0.0", [], { state: "pending-approval", pending: [{ approver: "me" }] });
  open(agentWith({ state: "draft", active_version: null, releases: [waiting], choices: { folders: SHARED } }), null);
  sandbox.state.approvals = [{ id: "ap-1", title: "Release agent ux-triage 1.0.0?", text: "Its evaluation passed." }];
  F.selectFactoryTab("releases");
  F.renderFactoryReleases();
  F.renderFactoryDeployments();
  assert.strictEqual($("#factory-deploy-btn"), null);
  assert.match($("#fpanel-deployments").textContent, /Nothing to deploy yet/);
  routes[`GET ${AGENT}`] = agentWith({
    releases: [release("1.0.0", [], { approvals: [{ approver: "me", approved: true, at: "2025-05-02T10:01:00Z" }] })],
    choices: { folders: SHARED },
  });
  routes["POST /api/approvals/ap-1"] = {};
  const approve = $("#fpanel-releases [data-own-answer]").querySelectorAll("button").find((b) => b.textContent === "Approve");
  await fire(approve, "click");
  assert.deepStrictEqual(posts().map((c) => [c.path, c.body]), [["/api/approvals/ap-1", { approved: true, changes: false, note: "" }]]);
  assert.ok($("#fpanel-deployments #factory-deploy-btn"));
  assert.strictEqual($("#fpanel-releases [data-own-answer]"), null);
  assert.strictEqual(document.activeElement, $("#ftab-releases"));

  console.log("ok: the deploy form says why it can't deploy before it sends, Evaluate shows the last result, and a sign-off redraws Deployments");
})().catch((err) => {
  console.error(err);
  process.exit(1);
});
