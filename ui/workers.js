// Proofkin web UI: digital workers. Agents of this profile, each with its own
// key, allowlist and folders, running on this profile's sign-ins: created here,
// started and stopped (with this agent, if their owner wants), and deleted. Uses
// app.js helpers: $, el, api, toast, confirmDialog, avatar, icon, ICONS,
// providerLabel, ACCESS_SHORT and state (connected peers).
"use strict";

const workerState = {
  loaded: false,
  canOwn: false,
  profile: "",
  owner: null,
  list: [],
  signedIn: [],
  ghAccounts: [],
  folders: [],
};
const WORKER_PROVIDERS = ["copilot", "claude", "codex"];
const WORKER_NAME = /^[A-Za-z0-9_-][A-Za-z0-9._-]{0,63}$/;

/// `accounts`: also the GitHub CLI's accounts, to sign in to Copilot with (slower).
async function loadWorkers(accounts = false) {
  const w = await api(accounts ? "/api/workers?accounts=true" : "/api/workers");
  Object.assign(workerState, {
    loaded: true,
    canOwn: w.can_own,
    profile: w.profile,
    owner: w.owner,
    list: w.workers,
    signedIn: w.signed_in,
    ghAccounts: accounts ? w.gh_accounts ?? [] : workerState.ghAccounts,
    folders: w.folders,
  });
  renderWorkers();
}

function workerOnline(w) {
  return !!w.id && state.peers.some((p) => p.id === w.id);
}

/// What a worker is doing, in words, and the status chip it shows with.
function workerStatus(w) {
  const online = workerOnline(w);
  switch (w.process?.state) {
    case "running": return online ? ["Online", "done"] : ["Starting…", "running"];
    case "elsewhere": return online ? ["Online, started outside this agent", "done"] : ["Running outside this agent", "waiting"];
    case "exited": return ["Stopped on its own", "failed"];
    default: return online ? ["Online", "done"] : ["Stopped", "waiting"];
  }
}

function renderWorkers() {
  if (!workerState.loaded) return;
  const list = $("#worker-list");
  $("#add-worker-btn").hidden = !workerState.canOwn;
  if (!workerState.canOwn) {
    list.replaceChildren(el("li", {
      class: "empty",
      text: workerState.owner
        ? `This agent is a digital worker of ${workerState.owner}; only owners create workers.`
        : "This agent can't start digital workers.",
    }));
    return;
  }
  if (!workerState.list.length) {
    list.replaceChildren(el("li", { class: "empty" }, "Agents of yours that run on your sign-ins and take your tasks. ",
      el("strong", { text: "New worker" }), " creates one."));
    return;
  }
  list.replaceChildren(...workerState.list.map(workerNode));
}

function workerNode(w) {
  const [text, kind] = workerStatus(w);
  const phase = w.process?.state ?? "off";
  const here = phase === "running";
  const runs = w.providers.length ? `Runs ${w.providers.map(providerLabel).join(", ")}` : "Runs no models";
  const where = `works in ${["its data folder", ...w.folders].join(", ")}`;
  const p = w.process ?? {};
  const release = w.release;
  return el("li", { class: "worker", "data-worker": w.name },
    avatar(w.name, workerOnline(w)),
    el("div", { class: "who" },
      el("span", { class: "worker-head" }, el("span", { class: "name", text: w.name }), el("span", { class: `status ${kind}`, text })),
      w.role ? el("span", { class: "role", text: w.role }) : null,
      release ? el("span", { class: "sub", text: `Runs release ${release.version} of factory agent ${release.agent}${release.state === "running" ? "" : ` (${release.state})`}` }) : null,
      el("span", { class: "sub", text: `${runs} · ${where}` }),
      phase === "exited" ? el("details", { class: "worker-log" },
        el("summary", { text: p.code === null || p.code === undefined ? "Why it stopped" : `Why it stopped (exit code ${p.code})` }),
        el("pre", { tabindex: "0", text: p.log || "Its log is empty." })) : null,
      el("div", { class: "worker-actions" },
        phase === "elsewhere" ? null : el("button", {
          type: "button", class: "btn small", "aria-label": `${here ? "Stop" : "Start"} ${w.name}`,
          onclick: (e) => (here ? stopWorker(w, e.currentTarget) : startWorker(w, e.currentTarget)),
        }, here ? "Stop" : "Start"),
        w.providers.length ? el("button", {
          type: "button", class: "btn small", "aria-label": `Models for ${w.name}`,
          title: Object.keys(w.models ?? {}).length
            ? `Its own: ${Object.entries(w.models).map(([p, m]) => `${providerLabel(p)} ${modelsSummary(m)}`).join("; ")}`
            : "It uses your models settings",
          onclick: (e) => openModelsDialog(w.name, e.currentTarget),
        }, "Models…") : null,
        release ? el("button", {
          type: "button", class: "btn small", "aria-label": `Open factory agent ${release.agent}`,
          title: "Its releases, deployments, budgets and metrics",
          onclick: (e) => openFactoryAgent(release.agent, e.currentTarget, "deployments"),
        }, "Open agent") : null,
        el("label", { class: "check small", title: "Start it whenever this agent starts, and stop it when this agent stops" },
          el("input", { type: "checkbox", checked: w.start_with_owner || undefined, onchange: (e) => setStartWithOwner(w, e.currentTarget) }),
          el("span", { text: "Starts with this agent" })))),
    el("button", {
      type: "button", class: "icon-btn", title: `Delete ${w.name}`, "aria-label": `Delete digital worker ${w.name}`,
      onclick: () => deleteWorker(w),
    }, icon(ICONS.trash)));
}

/// Back on the worker's Start/Stop button once its item is drawn again (a disabled
/// button loses focus).
function refocusWorker(name) {
  if (document.activeElement && document.activeElement !== document.body) return;
  document.querySelector(`#worker-list [data-worker="${CSS.escape(name)}"] .worker-actions button`)?.focus();
}

async function startWorker(w, button) {
  button.disabled = true;
  button.textContent = "Starting…";
  try {
    await api(`/api/workers/${encodeURIComponent(w.name)}/start`, { method: "POST" });
  } catch (err) {
    toast(err.message, "error");
  }
  await loadWorkers().catch(() => {});
  refocusWorker(w.name);
}

async function stopWorker(w, button) {
  button.disabled = true;
  button.textContent = "Stopping…";
  try {
    await api(`/api/workers/${encodeURIComponent(w.name)}/stop`, { method: "POST" });
    toast(`Stopped ${w.name}`);
  } catch (err) {
    toast(err.message, "error");
  }
  await loadWorkers().catch(() => {});
  refocusWorker(w.name);
}

async function setStartWithOwner(w, input) {
  input.disabled = true;
  try {
    await api(`/api/workers/${encodeURIComponent(w.name)}`, {
      method: "PATCH", body: JSON.stringify({ start_with_owner: input.checked }),
    });
    toast(input.checked ? `${w.name} starts with this agent` : `${w.name} no longer starts with this agent`);
  } catch (err) {
    input.checked = !input.checked;
    toast(err.message, "error");
  } finally {
    input.disabled = false;
  }
}

async function deleteWorker(w) {
  const release = w.release
    ? ` It runs release ${w.release.version} of factory agent ${w.release.agent}: that deployment ends (the agent and its releases stay).`
    : "";
  const ok = await confirmDialog({
    title: `Delete digital worker ${w.name}?`,
    body: `It stops, leaves your allowlist and squads, and its profile goes: its key, its allowlist, the folders it shares and its audit log. Its data folder stays (${w.data_folder}).${release}`,
    confirm: "Delete worker",
    danger: true,
  });
  if (!ok) return;
  try {
    const res = await api(`/api/workers/${encodeURIComponent(w.name)}`, { method: "DELETE" });
    const left = res.squads?.length ? `; it left ${res.squads.map((s) => `#${s}`).join(", ")}` : "";
    const ended = res.deployments_ended?.length ? `; its deployment of ${res.deployments_ended.join(", ")} ended` : "";
    toast(`Deleted ${w.name}${left}${ended}`);
  } catch (err) {
    toast(err.message, "error");
  }
}

// ---------- new worker ----------
function renderWorkerProviders() {
  const signedIn = new Map(workerState.signedIn.map((s) => [s.provider, s.account]));
  const checked = new Set([...document.querySelectorAll("#worker-providers input:checked")].map((i) => i.value));
  $("#worker-providers").replaceChildren(...WORKER_PROVIDERS.map((p) => {
    const account = signedIn.get(p);
    // The only provider it can run is picked for you.
    const on = account && (checked.has(p) || signedIn.size === 1);
    return el("label", { class: "check" },
      el("input", { type: "checkbox", value: p, disabled: !account || undefined, checked: on || undefined }),
      el("span", {}, providerLabel(p), el("span", { class: "muted small", text: account ? ` · ${account}` : " · not signed in" })));
  }));
  const box = $("#worker-signin");
  box.replaceChildren();
  const more = WORKER_PROVIDERS.filter((p) => p !== "copilot" && !signedIn.has(p));
  const terminal = more.length
    ? el("span", { class: "muted small" }, `To use ${more.map(providerLabel).join(" or ")}, sign ${workerState.profile} in from a terminal: `,
      el("span", { class: "mono", text: `proofkin auth login ${more[0]} --profile ${workerState.profile}` }))
    : null;
  if (signedIn.has("copilot")) {
    if (terminal) box.append(terminal);
    return;
  }
  if (!workerState.ghAccounts.length) {
    box.append(el("p", { class: "muted small" }, "To use Copilot, sign the GitHub CLI in to an account with Copilot (",
      el("span", { class: "mono", text: "gh auth login" }), "), then open this dialog again."), terminal);
    return;
  }
  // Each host has an active account; Copilot is usually on github.com's.
  const accounts = workerState.ghAccounts;
  const preferred = accounts.find((a) => a.active && a.host === "github.com") ?? accounts.find((a) => a.active) ?? accounts[0];
  const pick = el("select", { id: "worker-gh", "aria-label": "GitHub account for Copilot" },
    ...accounts.map((a) => el("option", { value: `${a.login}@${a.host}`, selected: a === preferred || undefined, text: `${a.login}@${a.host}` })));
  const button = el("button", { type: "button", class: "btn small", onclick: (e) => signInCopilot(pick, e.currentTarget) }, "Sign in to Copilot");
  box.append(el("div", { class: "worker-signin" },
    el("span", { class: "small", text: `Sign ${workerState.profile} in to Copilot with an account of the GitHub CLI:` }),
    el("div", { class: "worker-signin-row" }, pick, button)), terminal);
}

async function signInCopilot(pick, button) {
  const [user, host] = pick.value.split("@");
  $("#worker-error").textContent = "";
  button.disabled = true;
  button.textContent = "Signing in…";
  try {
    const res = await api("/api/sign-in/copilot", { method: "POST", body: JSON.stringify({ user, host }) });
    toast(`Signed in to Copilot as ${res.account}`);
    await loadWorkers();
    renderWorkerProviders();
    $("#worker-providers input:not(:disabled)")?.focus();
  } catch (err) {
    $("#worker-error").textContent = err.message;
    button.disabled = false;
    button.textContent = "Sign in to Copilot";
  }
}

function renderWorkerFolders() {
  $("#worker-folders").replaceChildren(...(workerState.folders.length
    ? workerState.folders.map((f) => el("label", { class: "check" },
      el("input", { type: "checkbox", value: f.name }),
      el("span", {}, el("strong", { text: f.name }), ` · at most ${(ACCESS_SHORT[f.max_access] ?? f.max_access).toLowerCase()}`,
        el("span", { class: "mono", text: ` ${f.path}` }))))
    : [el("span", { class: "muted small" }, "You share no folders besides your data folder. Share one with ",
      el("span", { class: "mono", text: `proofkin share add NAME PATH --profile ${workerState.profile}` }),
      ", or link a repository under GitHub.")]));
}

async function openWorkerDialog() {
  $("#worker-form").reset();
  $("#worker-error").textContent = "";
  $("#worker-providers").replaceChildren();
  try {
    await loadWorkers(true);
  } catch (err) {
    toast(err.message, "error");
    return;
  }
  renderWorkerProviders();
  renderWorkerFolders();
  $("#worker-dialog").showModal();
  $("#worker-name").focus();
}

async function submitWorker(e) {
  e.preventDefault();
  const error = $("#worker-error");
  error.textContent = "";
  const name = $("#worker-name").value.trim();
  const providers = [...document.querySelectorAll("#worker-providers input:checked")].map((i) => i.value);
  const folders = [...document.querySelectorAll("#worker-folders input:checked")].map((i) => i.value);
  if (!WORKER_NAME.test(name)) {
    error.textContent = name ? "A name may only use letters, digits, '-', '_' and '.' (not first)." : "Enter a name.";
    $("#worker-name").focus();
    return;
  }
  if (!providers.length) {
    error.textContent = "Pick what it runs. Sign in to Copilot above if none is signed in.";
    ($("#worker-providers input:not(:disabled)") ?? $("#worker-gh"))?.focus();
    return;
  }
  const button = $("#worker-submit");
  button.disabled = true;
  button.textContent = "Creating…";
  try {
    const res = await api("/api/workers", {
      method: "POST",
      body: JSON.stringify({ name, role: $("#worker-role").value, providers, folders, start_with_owner: $("#worker-start").checked }),
    });
    $("#worker-dialog").close();
    if (res.error) toast(`Created ${name}, but it didn't start: ${res.error}`, "error");
    else toast(res.started ? `Created ${name}; it's starting` : `Created ${name}`);
    await loadWorkers().catch(() => {});
  } catch (err) {
    error.textContent = err.message;
    // To the field the server refused, if it's about one.
    const field = /name|allowlist|exists/.test(err.message) ? $("#worker-name")
      : /folder/.test(err.message) ? $("#worker-folders input")
        : /role/.test(err.message) ? $("#worker-role") : null;
    button.disabled = false;
    (field ?? button).focus();
  } finally {
    button.disabled = false;
    button.textContent = "Create worker";
  }
}

$("#add-worker-btn").addEventListener("click", openWorkerDialog);
$("#worker-cancel").addEventListener("click", () => $("#worker-dialog").close());
$("#worker-dialog").addEventListener("close", () => $("#add-worker-btn").focus());
$("#worker-form").addEventListener("submit", submitWorker);
