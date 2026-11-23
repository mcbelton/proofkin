// Proofkin web UI: models. Which models and reasoning efforts a task may ask an agent
// for, from what it says (its settings, and the models its account offers it), for the
// Run task form (app.js) and the plan builder's step cards (plan-builder.js); and the
// Models dialog, where an owner picks them for this agent and its digital workers.
// The pure helpers at the top are tested in node (tests/models.test.cjs); the dialog
// uses app.js helpers ($, el, api, toast, providerLabel, state) when it opens.
"use strict";

// Reasoning efforts, least to most (src/models.rs EFFORTS).
const EFFORTS = ["minimal", "low", "medium", "high", "xhigh", "max"];

const sameModel = (a, b) => a.toLowerCase() === b.toLowerCase();

/** What `agents` (a peer, this agent, or the peers a task may go to) say about their
 * models for `provider`, merged: the models a task may pick (those each allows, else
 * all its account offers), the efforts it may ask for, and, for one agent, its defaults.
 * `other`: one of them allows any model and lists none (older versions, Claude, Codex),
 * so a model may also be named. */
function modelMenu(agents, provider) {
  const models = [];
  const efforts = new Set();
  let other = !agents.length;
  for (const agent of agents) {
    const set = agent?.models?.[provider] ?? {};
    const offers = agent?.offers?.[provider] ?? [];
    const allowed = set.allowed ?? [];
    if (!allowed.length && !offers.length) other = true;
    const list = allowed.length
      ? allowed.map((id) => offers.find((o) => sameModel(o.id, id)) ?? { id })
      : offers;
    for (const m of list) {
      // Efforts a listed model supports (none: it takes none); unknown for one not listed.
      const supported = offers.includes(m) ? m.efforts ?? [] : null;
      const had = models.find((x) => sameModel(x.id, m.id));
      if (!had) {
        models.push({ id: m.id, name: m.name ?? m.id, vendor: m.vendor ?? null, multiplier: m.multiplier ?? null, efforts: supported });
      } else if (supported) {
        had.efforts = [...new Set([...(had.efforts ?? []), ...supported])];
      }
    }
    for (const e of set.efforts?.length ? set.efforts : EFFORTS) efforts.add(e);
  }
  const one = agents.length === 1 ? agents[0]?.models?.[provider] ?? {} : null;
  return {
    defaultModel: one ? one.default ?? one.allowed?.[0] ?? null : null,
    defaultEffort: one ? one.effort ?? one.efforts?.[0] ?? null : null,
    models,
    other,
    efforts: EFFORTS.filter((e) => efforts.has(e)),
  };
}

/** `effort` as a model that supports only `supported` takes it (src/models.rs
 * fit_effort): itself, else the most it supports below it, else the least it supports. */
function fitEffort(effort, supported) {
  if (supported.includes(effort)) return effort;
  const want = EFFORTS.indexOf(effort);
  if (want < 0) return null;
  const known = EFFORTS.filter((e) => supported.includes(e));
  return known.filter((e) => EFFORTS.indexOf(e) <= want).pop() ?? known[0] ?? null;
}

/** The reasoning efforts a task on `model` (none: the default) may ask for: those the
 * menu's agents allow that the model supports. `fitted`: the default effort as the
 * model takes it; `fixed`: the model takes none. */
function effortChoices(menu, model) {
  const id = model || menu.defaultModel;
  const supported = id ? menu.models.find((m) => sameModel(m.id, id))?.efforts ?? null : null;
  if (supported && !supported.length) return { levels: [], fitted: null, fixed: true };
  return {
    levels: supported ? menu.efforts.filter((e) => supported.includes(e)) : menu.efforts,
    fitted: menu.defaultEffort && supported ? fitEffort(menu.defaultEffort, supported) : menu.defaultEffort,
    fixed: false,
  };
}

/** A model's option text: short (a select's list is as wide as its longest option). */
function modelOptionText(m) {
  return m.multiplier ? `${m.id} · ${m.multiplier}x` : m.id;
}

/** The efforts a model supports, as a range ("low–max"), or none. */
function effortRange(efforts) {
  const known = EFFORTS.filter((e) => (efforts ?? []).includes(e));
  if (!known.length) return null;
  return known.length === 1 ? known[0] : `${known[0]}–${known[known.length - 1]}`;
}

/** Models by vendor, in the order they come: [[vendor, models], …]. */
function modelsByVendor(models) {
  const groups = new Map();
  for (const m of models) {
    const vendor = m.vendor ?? "Other";
    if (!groups.has(vendor)) groups.set(vendor, []);
    groups.get(vendor).push(m);
  }
  return [...groups];
}

/** Options for `models`, grouped by vendor when there is more than one. */
function modelOptionNodes(models) {
  const option = (m) => el("option", {
    value: m.id, text: modelOptionText(m),
    title: [m.name !== m.id ? m.name : null, m.vendor, m.multiplier ? `${m.multiplier}x the base rate` : null,
      effortRange(m.efforts) ? `reasoning ${effortRange(m.efforts)}` : null].filter(Boolean).join(" · ") || null,
  });
  const groups = modelsByVendor(models);
  if (groups.length < 2) return models.map(option);
  return groups.map(([vendor, list]) => el("optgroup", { label: vendor }, ...list.map(option)));
}

// ---------- the Models dialog ----------
const modelsDialog = {
  /** null: this agent's settings; else a digital worker's name. */
  worker: null,
  opener: null,
  data: null,
  provider: null,
  /** Per provider: what the form shows, and what it showed when opened. */
  drafts: {},
  saved: {},
};

/** The form's state for a provider's settings `set` (`inherit`: a worker's, using its owner's). */
function modelsDraft(set, inherit) {
  set = set ?? {};
  return {
    inherit,
    anyModel: !set.allowed?.length,
    allowed: [...(set.allowed ?? [])],
    names: (set.allowed ?? []).join(", "),
    model: set.default ?? "",
    anyEffort: !set.efforts?.length,
    efforts: [...(set.efforts ?? [])],
    effort: set.effort ?? "",
  };
}

/** Opens the Models dialog for this agent (`worker` null) or one of its digital workers. */
async function openModelsDialog(worker, opener) {
  const dialog = $("#models-dialog");
  Object.assign(modelsDialog, { worker, opener, data: null, provider: null, drafts: {}, saved: {} });
  $("#models-h").textContent = worker ? `Models for ${worker}` : "Models";
  $("#models-intro").textContent = "";
  $("#models-error").textContent = "";
  $("#models-provider-field").hidden = true;
  $("#models-body").replaceChildren(el("p", { class: "muted small", role: "status" },
    el("span", { class: "spinner", "aria-hidden": "true" }), " Listing the models your account offers…"));
  $("#models-save").disabled = true;
  dialog.addEventListener("close", () => {
    // Its button may have been drawn again meanwhile: found by its label.
    const label = opener?.getAttribute("aria-label");
    const back = opener?.isConnected ? opener
      : label ? [...document.querySelectorAll("button[aria-label]")].find((b) => b.getAttribute("aria-label") === label) : null;
    back?.focus();
  }, { once: true });
  dialog.showModal();
  $("#models-cancel").focus();
  try {
    const data = await api(worker ? `/api/workers/${encodeURIComponent(worker)}/models` : "/api/models");
    if (modelsDialog.worker !== worker || !dialog.open) return;
    modelsDialog.data = data;
    for (const p of data.providers) {
      const own = data.models?.[p];
      modelsDialog.drafts[p] = modelsDraft(own ?? (worker ? data.owner_models?.[p] : null), Boolean(worker) && !own);
      modelsDialog.saved[p] = JSON.stringify(modelsDialog.drafts[p]);
    }
    modelsDialog.provider = data.providers[0] ?? null;
    $("#models-intro").textContent = modelsIntro(data);
    const pick = $("#models-provider");
    pick.replaceChildren(...data.providers.map((p) => el("option", { value: p, text: providerLabel(p) })));
    $("#models-provider-field").hidden = data.providers.length < 2;
    $("#models-save").disabled = !data.providers.length;
    renderModelsDialog();
    (data.providers.length > 1 ? pick : $("#models-body").querySelector("input, select") ?? $("#models-cancel")).focus();
  } catch (err) {
    $("#models-body").replaceChildren();
    $("#models-error").textContent = err.message;
  }
}

function modelsIntro(data) {
  if (modelsDialog.worker) {
    return `Peers that delegate to ${data.name} pick from what you allow here. A running worker applies it within seconds.`;
  }
  if (!data.providers.length) {
    return "This agent isn't signed in to a provider: sign in to Copilot, Claude or Codex first.";
  }
  const workers = data.owns_workers ? " Your digital workers use these too, unless they have their own." : "";
  if (!state.me?.providers?.length) {
    return "Your digital workers run on your sign-ins and use these, unless they have their own. A running worker applies them within seconds.";
  }
  return state.me.serving
    ? `Peers that delegate to this agent pick from what you allow here, and it applies at once.${workers}`
    : `Tasks you run on this agent use these.${workers}`;
}

/** Draws the shown provider's settings. */
function renderModelsDialog() {
  const p = modelsDialog.provider;
  const data = modelsDialog.data;
  const body = $("#models-body");
  if (!p || !data) return;
  const d = modelsDialog.drafts[p];
  const listed = data.listed?.[p] ?? [];
  const usable = listed.filter((m) => !m.policy);
  const off = listed.filter((m) => m.policy);
  const name = providerLabel(p);
  const parts = [];
  if (modelsDialog.worker) {
    const owner = data.owner_models?.[p];
    parts.push(el("label", { class: "check" },
      el("input", { type: "checkbox", id: "models-inherit", checked: d.inherit || undefined,
        onchange: (e) => { d.inherit = e.currentTarget.checked; renderModelsDialog(); $("#models-inherit").focus(); } }),
      el("span", {}, `Use ${data.owner}'s settings for ${name}`,
        el("span", { class: "muted small", text: owner ? ` (${modelsSummary(owner)})` : " (any model, its defaults)" }))));
  }
  if (!d.inherit) {
    // Models: any the account offers, or only the ones ticked (named, without a list).
    const scope = (any) => el("label", { class: "check" },
      el("input", { type: "radio", name: "models-scope", value: any ? "any" : "only", checked: d.anyModel === any || undefined,
        onchange: () => { d.anyModel = any; renderModelsDialog(); $(`input[name="models-scope"][value="${any ? "any" : "only"}"]`).focus(); } }),
      el("span", { text: any ? (usable.length ? "Any model your account offers" : "Any model") : (usable.length ? "Only the ones ticked" : "Only the ones named") }));
    const choices = [scope(true), scope(false)];
    if (usable.length) {
      const extra = d.allowed.filter((id) => !usable.some((m) => sameModel(m.id, id)));
      const tick = (m) => el("label", { class: "check model-check" },
        el("input", { type: "checkbox", value: m.id, disabled: d.anyModel || undefined,
          checked: (d.anyModel || d.allowed.some((id) => sameModel(id, m.id))) || undefined,
          onchange: (e) => {
            d.allowed = e.currentTarget.checked ? [...d.allowed, m.id] : d.allowed.filter((id) => !sameModel(id, m.id));
            renderModelsDefaults();
          } }),
        el("span", {}, el("span", { class: "mono", text: m.id }),
          el("span", { class: "muted small", text: [m.name !== m.id ? m.name : null, m.multiplier ? `${m.multiplier}x` : null,
            effortRange(m.efforts) ? `reasoning ${effortRange(m.efforts)}` : "no reasoning levels"].filter(Boolean).join(" · ") })));
      choices.push(el("div", { class: "model-checks", role: "group", "aria-label": `${name} models` },
        ...modelsByVendor(usable).map(([vendor, list]) => el("div", { class: "model-vendor" },
          el("h3", { text: vendor }), ...list.map(tick))),
        ...(extra.length ? [el("div", { class: "model-vendor" }, el("h3", { text: "Not offered to your account" }),
          ...extra.map((id) => tick({ id, name: id, efforts: null })))] : [])));
      if (off.length) {
        choices.push(el("p", { class: "muted small", text: `Not enabled for your account: ${off.map((m) => m.id).join(", ")}.` }));
      }
    } else {
      const error = data.errors?.[p];
      choices.push(el("label", { class: "field" },
        el("span", { class: "visually-hidden", text: `${name} models, by name` }),
        el("input", { type: "text", id: "models-names", class: "mono", spellcheck: "false", autocomplete: "off",
          value: d.names, disabled: d.anyModel || undefined, placeholder: "e.g. claude-sonnet-5.5, claude-opus-5.5…",
          oninput: (e) => {
            d.names = e.currentTarget.value;
            d.allowed = d.names.split(",").map((s) => s.trim()).filter(Boolean);
            renderModelsDefaults();
          } }),
        el("span", { class: "muted small", text: error
          ? `Couldn't list the models your account offers (${error}): name them, separated by commas.`
          : `${name} doesn't list its models: name them, separated by commas.` })));
    }
    parts.push(el("fieldset", { class: "field" }, el("legend", { text: "Models peers may ask for" }), ...choices));
    // The default: picked from the list, or named when there is none.
    const pickDefault = usable.length
      ? el("select", { id: "models-default", onchange: (e) => { d.model = e.currentTarget.value; } })
      : el("input", { type: "text", id: "models-default", class: "mono", spellcheck: "false", autocomplete: "off",
        value: d.model, placeholder: `${name}'s own default…`, oninput: (e) => { d.model = e.currentTarget.value.trim(); } });
    parts.push(el("label", { class: "field" }, el("span", { text: "Default model" }), pickDefault,
      el("span", { class: "muted small", text: usable.length || d.anyModel
        ? "Tasks that name no model run on it."
        : "Tasks that name no model run on it: one of those named (empty: the first)." })));
    // Reasoning: any level, or only the ones ticked.
    const levelScope = (any) => el("label", { class: "check" },
      el("input", { type: "radio", name: "models-effort-scope", value: any ? "any" : "only", checked: d.anyEffort === any || undefined,
        onchange: () => { d.anyEffort = any; renderModelsDialog(); $(`input[name="models-effort-scope"][value="${any ? "any" : "only"}"]`).focus(); } }),
      el("span", { text: any ? "Any level" : "Only the ones ticked" }));
    parts.push(el("fieldset", { class: "field" }, el("legend", { text: "Reasoning levels peers may ask for" }),
      levelScope(true), levelScope(false),
      el("div", { class: "mcp-options effort-checks", role: "group", "aria-label": "Reasoning levels" },
        ...EFFORTS.map((e) => el("label", { class: "mcp-chip" },
          el("input", { type: "checkbox", value: e, disabled: d.anyEffort || undefined,
            checked: (d.anyEffort || d.efforts.includes(e)) || undefined,
            onchange: (ev) => {
              d.efforts = EFFORTS.filter((x) => (x === e ? ev.currentTarget.checked : d.efforts.includes(x)));
              renderModelsDefaults();
            } }),
          el("span", { text: e }))))));
    parts.push(el("label", { class: "field" }, el("span", { text: "Default reasoning" }),
      el("select", { id: "models-effort", onchange: (e) => { d.effort = e.currentTarget.value; } }),
      el("span", { class: "muted small", text: "Tasks that ask for no level work at it; a model that supports fewer takes the nearest it supports below." })));
  }
  body.replaceChildren(...parts);
  renderModelsDefaults();
}

/** The default model and reasoning lists: with only some allowed, one of those. */
function renderModelsDefaults() {
  const p = modelsDialog.provider;
  const d = modelsDialog.drafts[p];
  const model = $("#models-default");
  const effort = $("#models-effort");
  if (!d || d.inherit || !model || !effort) return;
  if (model.tagName === "SELECT") {
    const usable = (modelsDialog.data.listed?.[p] ?? []).filter((m) => !m.policy);
    const ids = d.anyModel ? usable.map((m) => m.id) : d.allowed;
    if (!d.anyModel && !ids.some((id) => sameModel(id, d.model))) d.model = ids[0] ?? "";
    const known = !d.model || ids.some((id) => sameModel(id, d.model));
    model.replaceChildren(
      ...(d.anyModel ? [el("option", { value: "", text: `${providerLabel(p)}'s own default` })] : []),
      ...ids.map((id) => el("option", { value: id, text: id })),
      // One set by hand that the account doesn't list: kept unless changed.
      ...(known ? [] : [el("option", { value: d.model, text: d.model })]));
    model.value = d.model;
    model.disabled = !d.anyModel && !ids.length;
    if (model.disabled) model.replaceChildren(el("option", { value: "", text: "Tick a model first" }));
  }
  const levels = d.anyEffort ? EFFORTS : d.efforts;
  if (!d.anyEffort && !levels.includes(d.effort)) d.effort = levels[0] ?? "";
  effort.replaceChildren(
    ...(d.anyEffort ? [el("option", { value: "", text: "The model's own" })] : []),
    ...levels.map((e) => el("option", { value: e, text: e })));
  effort.value = d.effort;
  effort.disabled = !d.anyEffort && !levels.length;
  if (effort.disabled) effort.replaceChildren(el("option", { value: "", text: "Tick a level first" }));
}

/** A provider's settings in a few words: its default model and reasoning, and what peers
 * may ask for ("claude-opus-5.5 · max reasoning · only gpt-5.5, claude-opus-5.5 · levels high–max"). */
function modelsSummary(set) {
  set = set ?? {};
  const allowed = set.allowed ?? [];
  return [set.default ?? allowed[0] ?? "its default model",
    set.effort ?? set.efforts?.[0] ? `${set.effort ?? set.efforts[0]} reasoning` : null,
    allowed.length > 3 ? `only ${allowed.length} models` : allowed.length ? `only ${allowed.join(", ")}` : null,
    set.efforts?.length ? `levels ${set.efforts.join(", ")}` : null].filter(Boolean).join(" · ");
}

/** Saves each provider whose settings changed; errors stay in the dialog, with the form. */
async function saveModels(e) {
  e.preventDefault();
  const { worker, data } = modelsDialog;
  if (!data) return;
  const changed = data.providers.filter((p) => JSON.stringify(modelsDialog.drafts[p]) !== modelsDialog.saved[p]);
  for (const p of changed) {
    const d = modelsDialog.drafts[p];
    if (d.inherit) continue;
    const problem = !d.anyModel && !d.allowed.length ? "Tick at least one model, or choose Any model."
      : !d.anyEffort && !d.efforts.length ? "Tick at least one reasoning level, or choose Any level." : null;
    if (problem) {
      modelsDialog.provider = p;
      $("#models-provider").value = p;
      renderModelsDialog();
      $("#models-error").textContent = `${providerLabel(p)}: ${problem}`;
      return;
    }
  }
  const button = $("#models-save");
  button.disabled = true;
  button.textContent = "Saving…";
  $("#models-error").textContent = "";
  try {
    for (const p of changed) {
      const d = modelsDialog.drafts[p];
      await api(worker ? `/api/workers/${encodeURIComponent(worker)}/models` : "/api/models", {
        method: "PUT",
        body: JSON.stringify({
          provider: p, inherit: d.inherit,
          default: d.model || null, allowed: d.anyModel ? [] : d.allowed,
          effort: d.effort || null, efforts: d.anyEffort ? [] : d.efforts,
        }),
      });
      modelsDialog.saved[p] = JSON.stringify(d);
    }
    $("#models-dialog").close();
    toast(!changed.length ? "Nothing changed" : worker ? `Saved ${worker}'s models` : "Saved this agent's models");
  } catch (err) {
    $("#models-error").textContent = err.message;
  } finally {
    button.disabled = false;
    button.textContent = "Save models";
  }
}

function initModelsDialog() {
  $("#models-form").addEventListener("submit", saveModels);
  $("#models-cancel").addEventListener("click", () => $("#models-dialog").close());
  $("#models-provider").addEventListener("change", (e) => {
    modelsDialog.provider = e.currentTarget.value;
    $("#models-error").textContent = "";
    renderModelsDialog();
  });
}
