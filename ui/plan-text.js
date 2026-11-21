// The plan editor's palette as pure functions of the plan text and the cursor, so it
// is tested without a page (tests/plan_text.test.cjs). A click never pastes raw text:
// each part goes where the plan syntax (src/plan.rs) expects it.
const planText = (() => {
  // Step and claim names: letters, digits, '-', '_' or '.', not starting with '.'.
  const NAME = /^(?!\.)[A-Za-z0-9._-]{1,64}$/;
  // Words that start a part of a step line, so never its name.
  const KEYWORDS = new Set(["after", "approve", "team", "using"]);
  // Step options (before what to do), by the option they set. Repeating a list
  // option adds to it; the others take one value.
  const LISTS = new Map([["verify", "verify"], ["mcp", "mcp"], ["skill", "skill"], ["skills", "skill"],
    ["context", "context"], ["claims", "claims"], ["claim", "claims"]]);
  const SINGLE = new Set(["attempts", "save", "apply", "agent", "levels", "timeout", "issue"]);
  // A sign-off is a person's decision: no checks, retries, patches, saved result or agent.
  const NOT_IN_SIGNOFF = new Set(["verify", "attempts", "apply", "save", "agent", "levels"]);
  // A team step is done and proven by the lead's team plan.
  const NOT_IN_TEAM = new Set(["verify", "attempts", "apply", "mcp", "agent"]);
  /// What a new step does until you say: selected, so typing replaces it (the plan
  /// check refuses to run it as is, see src/plan.rs).
  const TODO = "What to do";
  const ERRORS = {
    squadSignoff: "A sign-off needs a person (@name or @me), not a squad.",
    signoffOption: "A sign-off is a person's decision: it has no checks, retries, patches, saved result or agent.",
    teamOption: "A team step is done and proven by the lead's team plan: it has no checks, retries, patches, MCP servers or agent of its own.",
    ownStep: (id) => `Step ${id} can't wait for itself.`,
  };

  const keyOf = (word) => word.slice(0, Math.max(word.indexOf("="), 0)) || null;
  const valueOf = (word) => word.slice(word.indexOf("=") + 1);
  /// The option `word` sets ("skill" for skills=a), or null if it isn't one.
  const optionOf = (word) => {
    const key = keyOf(word);
    return LISTS.get(key) ?? (SINGLE.has(key) ? key : null);
  };
  const isTarget = (word) => /^[@#]./.test(word);

  /// The line around `pos`: where it starts and ends, and its text.
  function lineAt(text, pos) {
    const start = pos > 0 ? text.lastIndexOf("\n", pos - 1) + 1 : 0;
    const newline = text.indexOf("\n", pos);
    const end = newline < 0 ? text.length : newline;
    return { start, end, line: text.slice(start, end) };
  }

  /// Each line of `text` with where it starts and ends.
  function linesOf(text) {
    let start = 0;
    return text.split("\n").map((line) => {
      const at = { start, end: start + line.length, line };
      start = at.end + 1;
      return at;
    });
  }

  /// How the parser reads a line: "empty", "comment", "claim", "track" (how the run is
  /// tracked on GitHub) or "step".
  function kindOf(line) {
    const t = line.trim();
    return !t ? "empty" : t.startsWith("//") ? "comment" : t.startsWith("claim:") ? "claim"
      : t.startsWith("track:") ? "track" : "step";
  }

  /// Sets an option on `step`: a list option adds its values (skill=a, then skill=b,
  /// is skill=a,b), another option replaces its value.
  function setOption(step, word) {
    const option = optionOf(word);
    const at = step.options.findIndex((o) => optionOf(o) === option);
    if (at < 0) {
      step.options.push(word);
    } else if (SINGLE.has(option)) {
      step.options[at] = word;
    } else {
      const have = step.options[at];
      const values = valueOf(have).split(",").filter(Boolean);
      for (const v of valueOf(word).split(",")) if (v && !values.includes(v)) values.push(v);
      step.options[at] = `${keyOf(have)}=${values.join(",")}`;
    }
  }

  /// Removes `word`'s values from the step's option (claims=C-1 from claims=C-1,C-2);
  /// an option left without values goes.
  function removeOption(step, word) {
    const at = step.options.findIndex((o) => optionOf(o) === optionOf(word));
    if (at < 0) return;
    const drop = valueOf(word).split(",");
    const values = valueOf(step.options[at]).split(",").filter((v) => v && !drop.includes(v));
    if (values.length) step.options[at] = `${keyOf(step.options[at])}=${values.join(",")}`;
    else step.options.splice(at, 1);
  }

  /// A step line's parts. Lenient, so a click can repair a line: parts out of order
  /// are found, repeated options are merged, and a line of plain words is what to do.
  function parseStep(line) {
    const words = line.trim().split(/\s+/).filter(Boolean);
    const step = { id: null, after: [], approve: false, team: false, target: null, using: null, options: [], todo: "" };
    const found = [];
    let i = 0;
    if (NAME.test(words[0] ?? "") && !KEYWORDS.has(words[0])) step.id = words[i++];
    for (; i < words.length; i++) {
      const [word, next] = [words[i], words[i + 1]];
      if (word === "after" && !step.target && !step.after.length && next) {
        step.after = next.split(",").filter(Boolean);
        i++;
      } else if (word === "approve" && !step.target && !step.approve && !step.team) {
        step.approve = true;
      } else if (word === "team" && !step.target && !step.approve && !step.team) {
        step.team = true;
      } else if (isTarget(word) && !step.target) {
        step.target = word;
      } else if (word === "using" && step.target && !step.using && next) {
        step.using = next;
        i++;
      } else if (optionOf(word)) {
        found.push(word);
      } else {
        break;
      }
    }
    step.todo = words.slice(i).join(" ");
    if (step.id && i === 1 && words.length > 1) {
      step.todo = words.join(" ");
      step.id = null;
    }
    for (const word of found) setOption(step, word);
    return step;
  }

  /// A step as plan text, its parts in the order the parser reads them.
  const formatStep = (s) => [s.id, s.after.length ? `after ${s.after.join(",")}` : "", s.approve ? "approve" : "", s.team ? "team" : "",
    s.target, s.using ? `using ${s.using}` : "", ...s.options, s.todo].filter(Boolean).join(" ");

  /// The names of the plan's steps.
  const stepIds = (text) => text.split("\n").filter((l) => kindOf(l) === "step")
    .map((l) => parseStep(l).id).filter(Boolean);
  /// The ids of the plan's claims.
  const claimIds = (text) => text.split("\n").map((l) => l.trim().match(/^claim:\s*(\S+)/)?.[1]).filter(Boolean);
  /// The first of make(from), make(from + 1), ... that isn't taken.
  function freeId(taken, make, from) {
    let n = from;
    while (taken.includes(make(n))) n++;
    return make(n);
  }
  const nextStepId = (text) => {
    const ids = stepIds(text);
    return freeId(ids, (n) => `step${n}`, ids.length + 1);
  };

  /// The change to make: replace [from, to) with `insert`, then select [start, end) of
  /// the line that starts at `lineStart`.
  const change = (from, to, insert, lineStart, [start, end]) =>
    ({ from, to, insert, select: [lineStart + start, lineStart + end] });

  /// Puts a new `line` on the cursor's line if it is empty, else on a line after it;
  /// after the plan's last line if the cursor is on a claim (claims come first).
  function placeLine(text, at, kind, line, select) {
    if (kind === "empty") return change(at.start, at.end, line, at.start, select);
    const pos = kind === "claim" ? text.length : at.end;
    const lead = pos === text.length && text.endsWith("\n") ? "" : "\n";
    return change(pos, pos, lead + line, pos + lead.length, select);
  }

  /// Adds `part` (who runs it, an option or `after STEP`) to the step on the cursor's
  /// line, where the plan syntax expects it, or else starts a new step with it. Returns
  /// the change and what to select after it, or {error} if the part doesn't fit.
  function addPart(text, pos, part, { target }) {
    const at = lineAt(text, pos);
    const kind = kindOf(at.line);
    const step = parseStep(kind === "step" ? at.line : "");
    if (isTarget(part)) {
      if (step.approve && part.startsWith("#")) return { error: ERRORS.squadSignoff };
      // A sign-off is by a person: the peer, not one of its folders.
      step.target = step.approve ? part.split(":")[0] : part;
    } else if (part.startsWith("after ")) {
      const dep = part.slice("after ".length);
      if (dep === step.id) return { error: ERRORS.ownStep(dep) };
      if (!step.after.includes(dep)) step.after.push(dep);
    } else {
      if (step.approve && NOT_IN_SIGNOFF.has(optionOf(part))) return { error: ERRORS.signoffOption };
      if (step.team && NOT_IN_TEAM.has(optionOf(part))) return { error: ERRORS.teamOption };
      setOption(step, part);
    }
    step.id ??= nextStepId(text);
    step.target ??= target;
    step.todo ||= TODO;
    const line = (kind === "step" ? at.line.match(/^\s*/)[0] : "") + formatStep(step);
    // Select a placeholder to type over: a new topic's name, or what to do.
    const topic = part === "save=topic" ? line.indexOf(" save=topic ") : -1;
    const select = topic >= 0 ? [topic + 6, topic + 11]
      : step.todo === TODO ? [line.length - TODO.length, line.length] : [line.length, line.length];
    return kind === "step" ? change(at.start, at.end, line, at.start, select) : placeLine(text, at, kind, line, select);
  }

  /// Adds a claim, a step or a sign-off as a line of its own and selects what to write
  /// in it. Claims go after the other claims (before the steps if there are none);
  /// steps and sign-offs after the cursor's line. A sign-off is on the step at the
  /// cursor, or the last step.
  function addLine(text, pos, what, { target, check }) {
    const at = lineAt(text, pos);
    const kind = kindOf(at.line);
    const claims = claimIds(text);
    const ids = stepIds(text);
    const proves = claims.length ? `claims=${claims[0]}` : "";
    const make = (head, todo) => {
      const line = [...head, todo].filter(Boolean).join(" ");
      return [line, [line.length - todo.length, line.length]];
    };
    if (what === "track") {
      // One per plan, at the top: select its issue number to type over.
      const lines = linesOf(text);
      const have = lines.find((l) => kindOf(l.line) === "track");
      if (have) {
        const start = have.line.indexOf("track:") + "track:".length;
        return change(have.start, have.start, "", have.start, [start + (have.line[start] === " " ? 1 : 0), have.line.length]);
      }
      return change(0, 0, `track: #1\n`, 0, [8, 9]);
    }
    if (what === "claim") {
      const [line, select] = make(["claim:", freeId(claims, (n) => `C-${n}`, claims.length + 1)],
        "What must be true at the end");
      const lines = linesOf(text);
      const lastClaim = lines.filter((l) => kindOf(l.line) === "claim").at(-1);
      if (lastClaim) return change(lastClaim.end, lastClaim.end, `\n${line}`, lastClaim.end + 1, select);
      const firstStep = lines.find((l) => kindOf(l.line) === "step");
      if (firstStep) return change(firstStep.start, firstStep.start, `${line}\n`, firstStep.start, select);
      const lead = !text || text.endsWith("\n") ? "" : "\n";
      return change(text.length, text.length, lead + line, text.length + lead.length, select);
    }
    if (what === "step") {
      const id = freeId(ids, (n) => `step${n}`, ids.length + 1);
      // Only a step with a check can prove a claim: without one it would spoil it.
      const [line, select] = make([id, target, check ? `verify=${check}` : "", check ? proves : ""], TODO);
      return placeLine(text, at, kind, line, select);
    }
    const of = (kind === "step" && parseStep(at.line).id) || ids.at(-1);
    const id = ids.includes("review") ? freeId(ids, (n) => `review-${n}`, 2) : "review";
    const [line, select] = make([id, of ? `after ${of}` : "", "approve @me", proves],
      of ? `Check the result of ${of} and approve it` : "Approve it");
    return placeLine(text, at, kind, line, select);
  }

  /// Makes a plan check fix's edits (Fix in src/plan.rs) on `text`: add or remove a
  /// step's option, add a line after a step's, remove a claim. Returns the new text.
  function applyFix(text, edits) {
    for (const edit of edits) {
      const lines = linesOf(text);
      if (edit.do === "remove_claim") {
        const at = lines.find((l) => kindOf(l.line) === "claim" && claimIds(l.line)[0] === edit.claim);
        if (at) {
          text = at.end < text.length ? text.slice(0, at.start) + text.slice(at.end + 1)
            : text.slice(0, Math.max(at.start - 1, 0));
        }
        continue;
      }
      const at = lines.find((l) => kindOf(l.line) === "step" && parseStep(l.line).id === (edit.step ?? edit.after));
      if (!at) continue;
      let line = `${at.line}\n${edit.line}`;
      if (edit.do !== "add_line") {
        const step = parseStep(at.line);
        (edit.do === "add" ? setOption : removeOption)(step, edit.option);
        line = at.line.match(/^\s*/)[0] + formatStep(step);
      }
      text = text.slice(0, at.start) + line + text.slice(at.end);
    }
    return text;
  }

  // ---------- the plan as a model, for the plan builder (ui/plan-builder.js) ----------
  /// The palette's placeholders (the plan check refuses to run them as they are).
  const TODO_CLAIM = "What must be true at the end";
  const blank = (text, todo) => !text.trim() || text.trim() === todo;

  /// The values of a step's option, by the option it sets ("skill", "claims", ...).
  function valuesOf(step, option) {
    const word = step.options.find((o) => optionOf(o) === option);
    return word ? valueOf(word).split(",").filter(Boolean) : [];
  }

  /// Sets a step's option to `values` (a list or one value) where it stands; no value
  /// removes it.
  function setValues(step, option, values) {
    const list = [].concat(values ?? []).map((v) => String(v).trim()).filter(Boolean);
    const at = step.options.findIndex((o) => optionOf(o) === option);
    if (!list.length) {
      if (at >= 0) step.options.splice(at, 1);
      return;
    }
    const word = `${at >= 0 ? keyOf(step.options[at]) : option}=${list.join(",")}`;
    if (at >= 0) step.options[at] = word;
    else step.options.push(word);
  }

  /// The plan as the builder edits it: notes (comments before the first claim or step,
  /// and after the last), how its run is tracked (`track:`, kept as written), claims
  /// and steps, each with the comments just above it. Lenient, like the palette: a
  /// half-written line is a step with parts missing.
  function parsePlan(text) {
    const plan = { notes: [], track: null, claims: [], steps: [] };
    let comments = [];
    for (const line of text.split("\n")) {
      const t = line.trim();
      const kind = kindOf(t);
      if (kind === "empty") continue;
      if (kind === "comment") {
        comments.push(t.replace(/^\/\/ ?/, ""));
        continue;
      }
      if (kind === "track") {
        plan.track = t.slice("track:".length).trim();
        plan.notes.push(...comments);
        comments = [];
        continue;
      }
      if (!plan.claims.length && !plan.steps.length) {
        plan.notes.push(...comments);
        comments = [];
      }
      if (kind === "claim") {
        const [, id, rest] = t.match(/^claim:\s*(\S*)\s*(.*)$/);
        const named = NAME.test(id);
        plan.claims.push({ id: named ? id : null, text: named ? rest : `${id} ${rest}`.trim(), notes: comments });
      } else {
        plan.steps.push({ ...parseStep(t), notes: comments });
      }
      comments = [];
    }
    plan.notes.push(...comments);
    for (const c of plan.claims) c.id ??= freeId(plan.claims.map((x) => x.id), (n) => `C-${n}`, 1);
    for (const s of plan.steps) s.id ??= freeId(plan.steps.map((x) => x.id), (n) => `step${n}`, 1);
    return plan;
  }

  /// The text of `plan` (notes, claims, then steps), and what each line is: {claim}
  /// or {step} by id, so the plan check's "line N" points at a card.
  function formatPlan(plan) {
    const lines = [];
    const items = [];
    const add = (text, item = null) => {
      lines.push(text);
      items.push(item);
    };
    const note = (n, item) => add(`// ${n}`.trimEnd(), item);
    const gap = () => lines.length && lines.at(-1) !== "" && add("");
    plan.notes.forEach((n) => note(n));
    if (plan.track) add(`track: ${plan.track}`);
    if (plan.claims.length) gap();
    for (const c of plan.claims) {
      c.notes?.forEach((n) => note(n, { claim: c.id }));
      add(`claim: ${c.id} ${c.text}`.trimEnd(), { claim: c.id });
    }
    if (plan.steps.length) gap();
    for (const s of plan.steps) {
      s.notes?.forEach((n) => note(n, { step: s.id }));
      add(formatStep(s), { step: s.id });
    }
    return { text: lines.join("\n"), items };
  }

  /// Renames step `from`, and where other steps wait for it or apply its work.
  function renameStep(plan, from, to) {
    for (const s of plan.steps) {
      if (s.id === from) s.id = to;
      s.after = s.after.map((d) => (d === from ? to : d));
      if (valuesOf(s, "apply")[0] === from) setValues(s, "apply", to);
    }
  }

  /// Removes step `id`, and where other steps wait for it or apply its work.
  function removeStep(plan, id) {
    plan.steps = plan.steps.filter((s) => s.id !== id);
    for (const s of plan.steps) {
      s.after = s.after.filter((d) => d !== id);
      if (valuesOf(s, "apply")[0] === id) setValues(s, "apply", null);
    }
  }

  /// Renames claim `from`, and where steps prove it.
  function renameClaim(plan, from, to) {
    for (const c of plan.claims) if (c.id === from) c.id = to;
    for (const s of plan.steps) setValues(s, "claims", valuesOf(s, "claims").map((c) => (c === from ? to : c)));
  }

  /// Removes claim `id`, and where steps prove it.
  function removeClaim(plan, id) {
    plan.claims = plan.claims.filter((c) => c.id !== id);
    for (const s of plan.steps) setValues(s, "claims", valuesOf(s, "claims").filter((c) => c !== id));
  }

  /// The steps that wait for step `id`, directly or not: it can't wait for them.
  function dependents(plan, id) {
    const found = new Set();
    for (let grew = true; grew;) {
      grew = false;
      for (const s of plan.steps) {
        if (!found.has(s.id) && s.id !== id && s.after.some((d) => d === id || found.has(d))) {
          found.add(s.id);
          grew = true;
        }
      }
    }
    return found;
  }

  /// What must be filled in or fixed before the plan can run, by card and field:
  /// {kind: "claim" | "step", index, field, message}.
  function needs(plan) {
    const out = [];
    const need = (kind, index, field, message) => out.push({ kind, index, field, message });
    const badName = "Use letters, digits, '-', '_' or '.'";
    plan.claims.forEach((c, i) => {
      if (!NAME.test(c.id)) need("claim", i, "id", badName);
      else if (plan.claims.findIndex((x) => x.id === c.id) < i) need("claim", i, "id", `Another claim is ${c.id}`);
      if (blank(c.text, TODO_CLAIM)) need("claim", i, "text", "Say what must be true");
    });
    plan.steps.forEach((s, i) => {
      if (!NAME.test(s.id) || KEYWORDS.has(s.id)) need("step", i, "id", badName);
      else if (plan.steps.findIndex((x) => x.id === s.id) < i) need("step", i, "id", `Another step is called ${s.id}`);
      if (!s.target) need("step", i, "target", s.approve ? "Choose who signs off" : "Choose who does it");
      if (blank(s.todo, TODO)) {
        need("step", i, "todo", s.approve ? "Say what to check before signing off" : "Say what this step should do");
      }
      if (!s.approve && !s.team && valuesOf(s, "claims").length && !valuesOf(s, "verify").length) {
        need("step", i, "claims", "Without a check, this step can't prove a claim: pick a check, or let a sign-off prove it");
      }
    });
    return out;
  }

  return {
    addPart, addLine, applyFix, stepIds, claimIds, parseStep, formatStep, ERRORS,
    parsePlan, formatPlan, valuesOf, setValues, renameStep, removeStep, renameClaim, removeClaim,
    dependents, needs, isTodo: (text) => blank(text, TODO), isTodoClaim: (text) => blank(text, TODO_CLAIM),
  };
})();
