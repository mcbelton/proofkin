// The plan editor's palette as pure functions of the plan text and the cursor, so it
// is tested without a page (tests/plan_text.test.cjs). A click never pastes raw text:
// each part goes where the plan syntax (src/plan.rs) expects it.
const planText = (() => {
  // Step and claim names: letters, digits, '-', '_' or '.', not starting with '.'.
  const NAME = /^(?!\.)[A-Za-z0-9._-]{1,64}$/;
  // Words that start a part of a step line, so never its name.
  const KEYWORDS = new Set(["after", "approve", "team", "pr", "using"]);
  const MODEL_HEADERS = ["model", "effort"];
  // What kind of work a plan is (kind:, src/plan.rs Kind): the label and the issue type
  // its tracked run's issues get (src/tracker.rs).
  const KINDS = [
    { value: "bug", name: "Bug", label: "bug", type: "Bug" },
    { value: "feature", name: "Feature", label: "feature", type: "Feature" },
    { value: "enhancement", name: "Enhancement", label: "enhancement", type: "Feature" },
    { value: "docs", name: "Docs", label: "documentation", type: "Task" },
    { value: "task", name: "Task", label: "task", type: "Task" },
  ];
  // Step options (before what to do), by the option they set. Repeating a list
  // option adds to it; the others take one value.
  const LISTS = new Map([["verify", "verify"], ["mcp", "mcp"], ["skill", "skill"], ["skills", "skill"],
    ["context", "context"], ["claims", "claims"], ["claim", "claims"], ["workiq", "workiq"],
    ["reviewers", "reviewers"], ["reviewer", "reviewers"]]);
  const SINGLE = new Set(["attempts", "save", "apply", "agent", "levels", "timeout", "issue", "model", "effort", "changes", "base"]);
  // A sign-off is a person's decision: no checks, retries, patches, saved result, agent, model, effort or search.
  const NOT_IN_SIGNOFF = new Set(["verify", "attempts", "apply", "save", "agent", "levels", "workiq", "model", "effort", "base"]);
  // A team step is done and proven by the lead's team plan.
  const NOT_IN_TEAM = new Set(["verify", "attempts", "apply", "mcp", "agent", "model", "effort", "base"]);
  // A pull request step opens the pull request with the work before it: only a time limit, an
  // issue and who reviews it (an option only it has).
  const IN_PR = new Set(["timeout", "issue", "reviewers"]);
  // A @workiq step asks Microsoft 365 for an answer: context, never proof. Its agent= is a Work IQ
  // agent id; it has no skills, MCP servers, checks, claims, folder, model or search of its own.
  // With apply=STEP it makes the Microsoft 365 changes STEP proposed, once you signed them off.
  const WORKIQ = "@workiq";
  const NOT_IN_WORKIQ = new Set(["verify", "attempts", "mcp", "skill", "claims", "levels", "context", "workiq", "model", "effort", "base"]);
  /// Whether `target` is Microsoft 365 Copilot (@workiq; with a folder, which it can't have).
  const isWorkiq = (target) => target === WORKIQ || Boolean(target?.startsWith(`${WORKIQ}:`));
  // What a step with workiq= searches in Microsoft 365, by source (src/plan.rs WORKIQ_SOURCES).
  const WORKIQ_SOURCES = [
    ["all", "all of your Microsoft 365: mail, meetings, files, Teams messages, people and connectors"],
    ["email", "your mail"],
    ["meetings", "your meetings"],
    ["files", "your files in OneDrive and SharePoint"],
    ["teams", "your Teams chats and channel messages"],
    ["people", "people in your organization (for your own agent and digital workers only)"],
    ["connectors", "what Microsoft Graph connectors bring in from other systems"],
  ];
  /// What a new step does until you say: selected, so typing replaces it (the plan
  /// check refuses to run it as is, see src/plan.rs).
  const TODO = "What to do";
  const ERRORS = {
    squadSignoff: "A sign-off needs a person (@name or @me), not a squad.",
    signoffOption: "A sign-off is a person's decision: it has no checks, retries, patches, saved result, agent, model, reasoning effort or Microsoft 365 search.",
    teamOption: "A team step is done and proven by the lead's team plan: it has no checks, retries, patches, MCP servers, agent, model or reasoning effort of its own.",
    workiqOption: "A @workiq step is an answer from Microsoft 365, which is context and never proof: it has no checks, claims, retries, skills, MCP servers, model, reasoning effort or search of its own.",
    workiqKind: "A @workiq step is an answer from Microsoft 365, which is context and never proof: it can't be a sign-off or a team step.",
    workiqApply: "A @workiq step doesn't start from another step's work: with apply=, it makes the Microsoft 365 changes that step proposed. Take out apply= first.",
    prOption: "A pull request step opens the pull request with the work before it: it takes only a time limit, an issue and who reviews it.",
    prTarget: "A pull request step is opened by the agent that holds the folder's clone: a peer and its folder (@peer:folder).",
    reviewersOption: "Only a pull request step asks for reviews (reviewers=).",
    changesOption: "Only a sign-off says how often its approver may ask for changes (changes=).",
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

  /// The line around `pos` and where its step's text ends: a line of a step's task
  /// text is read as the step's own line.
  function lineAt(text, pos) {
    const lines = linesOf(text);
    const found = lines.find((l) => l.start <= pos && pos <= l.end) ?? lines.at(-1);
    return lines[found.head];
  }

  /// Each line of `text` with where it starts and ends, how the parser reads it (see
  /// kindOf; "text" for a line that continues the task text of a step or the plan's
  /// description, about:), the index of the step or about: line it belongs to (its own
  /// for any other line) and, on a step or about: line, where its last continuation
  /// line ends.
  function linesOf(text) {
    let start = 0;
    const lines = text.split("\n").map((line, index) => {
      const at = { start, end: start + line.length, line, kind: kindOf(line), head: index, blockEnd: start + line.length };
      start = at.end + 1;
      return at;
    });
    const indent = (l) => l.match(/^\s*/)[0].length;
    for (let i = 0; i < lines.length; i++) {
      if (lines[i].kind !== "step" && lines[i].kind !== "about") continue;
      const base = indent(lines[i].line);
      let last = i;
      for (let j = i + 1; j < lines.length; j++) {
        if (!lines[j].line.trim()) continue;
        if (indent(lines[j].line) <= base) break;
        last = j;
      }
      for (let k = i + 1; k <= last; k++) Object.assign(lines[k], { kind: "text", head: i });
      lines[i].blockEnd = lines[last].end;
      i = last;
    }
    return lines;
  }

  /// How the parser reads a line on its own, before matching step continuations.
  function kindOf(line) {
    const t = line.trim();
    return !t ? "empty" : t.startsWith("//") ? "comment" : t.startsWith("claim:") ? "claim"
      : t.startsWith("track:") ? "track" : t.startsWith("kind:") ? "kind" : t.startsWith("about:") ? "about"
        : MODEL_HEADERS.find((key) => t.startsWith(`${key}:`)) ?? "step";
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
    const step = { id: null, after: [], approve: false, team: false, pr: false, target: null, using: null, options: [], todo: "" };
    const found = [];
    let i = 0;
    if (NAME.test(words[0] ?? "") && !KEYWORDS.has(words[0])) step.id = words[i++];
    for (; i < words.length; i++) {
      const [word, next] = [words[i], words[i + 1]];
      if (word === "after" && !step.target && !step.after.length && next) {
        step.after = next.split(",").filter(Boolean);
        i++;
      } else if (word === "approve" && !step.target && !step.approve && !step.team && !step.pr) {
        step.approve = true;
      } else if (word === "team" && !step.target && !step.approve && !step.team && !step.pr) {
        step.team = true;
      } else if (word === "pr" && !step.target && !step.approve && !step.team && !step.pr) {
        step.pr = true;
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
  const formatStep = (s) => [s.id, s.after.length ? `after ${s.after.join(",")}` : "", s.approve ? "approve" : "", s.team ? "team" : "", s.pr ? "pr" : "",
    s.target, s.using ? `using ${s.using}` : "", ...s.options, s.todo].filter(Boolean).join(" ");

  /// The names of the plan's steps.
  const stepIds = (text) => linesOf(text).filter((l) => l.kind === "step")
    .map((l) => parseStep(l.line).id).filter(Boolean);
  /// The ids of the plan's claims.
  const claimIds = (text) => linesOf(text).filter((l) => l.kind === "claim")
    .map((l) => l.line.trim().match(/^claim:\s*(\S+)/)?.[1]).filter(Boolean);
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
    const pos = kind === "claim" ? text.length : at.blockEnd;
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
      const workiq = isWorkiq(part);
      if (workiq && (step.approve || step.team)) return { error: ERRORS.workiqKind };
      if (step.pr && !/^@[^:]+:.+/.test(part)) return { error: ERRORS.prTarget };
      if (workiq && step.options.some((o) => NOT_IN_WORKIQ.has(optionOf(o)))) return { error: ERRORS.workiqOption };
      if (workiq && !isWorkiq(step.target) && valuesOf(step, "apply").length) return { error: ERRORS.workiqApply };
      // A sign-off is by a person: the peer, not one of its folders. Work IQ has no folders.
      step.target = step.approve || workiq ? part.split(":")[0] : part;
    } else if (part.startsWith("after ")) {
      const dep = part.slice("after ".length);
      if (dep === step.id) return { error: ERRORS.ownStep(dep) };
      if (!step.after.includes(dep)) step.after.push(dep);
    } else {
      if (step.approve && NOT_IN_SIGNOFF.has(optionOf(part))) return { error: ERRORS.signoffOption };
      if (step.team && NOT_IN_TEAM.has(optionOf(part))) return { error: ERRORS.teamOption };
      if (step.pr && !IN_PR.has(optionOf(part))) return { error: ERRORS.prOption };
      if (!step.pr && optionOf(part) === "reviewers") return { error: ERRORS.reviewersOption };
      if (!step.approve && optionOf(part) === "changes") return { error: ERRORS.changesOption };
      if (step.target === WORKIQ && NOT_IN_WORKIQ.has(optionOf(part))) return { error: ERRORS.workiqOption };
      if (optionOf(part) === "workiq") {
        setValues(step, "workiq", valueOf(part).split(",").filter(Boolean)
          .reduce((have, source) => toggleSource(have, source), valuesOf(step, "workiq")));
      } else {
        setOption(step, part);
      }
    }
    step.id ??= nextStepId(text);
    step.target ??= target;
    // A pull request step's title is optional: no placeholder to type over.
    step.todo ||= (kind === "step" && at.blockEnd > at.end) || step.pr ? "" : TODO;
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
      const have = lines.find((l) => l.kind === "track");
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
      const lastClaim = lines.filter((l) => l.kind === "claim").at(-1);
      if (lastClaim) return change(lastClaim.end, lastClaim.end, `\n${line}`, lastClaim.end + 1, select);
      const firstStep = lines.find((l) => l.kind === "step");
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
        const at = lines.find((l) => l.kind === "claim" && claimIds(l.line)[0] === edit.claim);
        if (at) {
          text = at.end < text.length ? text.slice(0, at.start) + text.slice(at.end + 1)
            : text.slice(0, Math.max(at.start - 1, 0));
        }
        continue;
      }
      const at = lines.find((l) => l.kind === "step" && parseStep(l.line).id === (edit.step ?? edit.after));
      if (!at) continue;
      if (edit.do === "add_line") {
        text = `${text.slice(0, at.blockEnd)}\n${edit.line}${text.slice(at.blockEnd)}`;
        continue;
      }
      const step = parseStep(at.line);
      (edit.do === "add" ? setOption : removeOption)(step, edit.option);
      const line = at.line.match(/^\s*/)[0] + formatStep(step);
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

  /// The workiq= sources `have` with `source` turned on or off: all searches everything,
  /// so it clears the others, and another source clears all (src/plan.rs refuses both).
  function toggleSource(have, source, on = true) {
    if (!on) return have.filter((v) => v !== source);
    if (source === "all") return ["all"];
    const rest = have.filter((v) => v !== "all");
    return rest.includes(source) ? rest : [...rest, source];
  }

  /// Sets who does a step. Microsoft 365 Copilot (@workiq) answers it: it has no folder,
  /// provider, or options it can't have (see NOT_IN_WORKIQ). Its agent= is a Work IQ agent's
  /// id, a peer's is one of your agents, so agent= goes when the step moves between them.
  function setTarget(step, target) {
    const workiq = isWorkiq(target);
    if (workiq) {
      for (const option of NOT_IN_WORKIQ) setValues(step, option, null);
      // The work a peer's step started from (apply=) isn't Microsoft 365 changes to make.
      if (!isWorkiq(step.target)) setValues(step, "apply", null);
      step.using = null;
    }
    if (workiq !== isWorkiq(step.target)) setValues(step, "agent", null);
    step.target = workiq ? WORKIQ : target;
  }

  /// The lines after the step or about: line `all[index]` that continue it, without
  /// their common indentation, as the parser reads them (src/plan.rs).
  function continuedOf(all, index) {
    const body = [];
    for (let i = index + 1; all[i]?.head === index && all[i].kind === "text"; i++) body.push(all[i].line.trimEnd());
    const indent = (l) => l.match(/^\s*/)[0].length;
    const common = Math.min(...body.filter(Boolean).map(indent));
    return body.map((l) => l.slice(common)).join("\n");
  }

  /// The lines after the step line `all[index]` that continue its task text.
  function moreOf(all, index) {
    const more = continuedOf(all, index);
    return parseStep(all[index].line).todo ? more : more.replace(/^\n+/, "");
  }

  /// Lines that continue a line, as the builder writes them: indented under it.
  const indented = (more) => (more ? more.split("\n").map((l) => (l ? `  ${l}` : "")) : []);

  /// A step's whole task text: what is on its line and the lines that continue it.
  const taskOf = (s) => (s.more ? (s.todo ? `${s.todo}\n${s.more}` : s.more) : s.todo);

  /// Sets a step's task text; its first line goes on the step's line, the rest continues it.
  function setTask(s, text) {
    const [first, ...rest] = text.split("\n");
    s.todo = first.split(/\s+/).filter(Boolean).join(" ");
    s.more = rest.join("\n").trimEnd();
  }

  /// The plan as the builder edits it: notes (comments before the first claim or step,
  /// and after the last), settings (with the kind: and about: lines as written), claims
  /// and steps, with their comments. Lenient, like the palette: a half-written line is
  /// a step with parts missing.
  function parsePlan(text) {
    const plan = { notes: [], track: null, modelLines: [], kindLines: [], aboutLines: [], claims: [], steps: [] };
    let comments = [];
    const all = linesOf(text);
    for (const [index, { line, kind: lineKind }] of all.entries()) {
      if (lineKind === "text") continue;
      const t = line.trim();
      const kind = kindOf(t);
      if (kind === "empty") continue;
      if (kind === "comment") {
        comments.push(t.replace(/^\/\/ ?/, ""));
        continue;
      }
      if (kind === "track" || kind === "kind" || kind === "about" || MODEL_HEADERS.includes(kind)) {
        if (kind === "track") plan.track = t.slice("track:".length).trim();
        // Keep invalid lines, too, so unrelated Builder changes don't silently fix them.
        else if (kind === "kind") plan.kindLines.push(t);
        else if (kind === "about") plan.aboutLines.push(t, ...indented(continuedOf(all, index)));
        else plan.modelLines.push(t);
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
        plan.steps.push({ ...parseStep(t), more: moreOf(all, index), notes: comments });
      }
      comments = [];
    }
    plan.notes.push(...comments);
    for (const c of plan.claims) c.id ??= freeId(plan.claims.map((x) => x.id), (n) => `C-${n}`, 1);
    for (const s of plan.steps) s.id ??= freeId(plan.steps.map((x) => x.id), (n) => `step${n}`, 1);
    return Object.assign(plan, modelsFrom(plan.modelLines));
  }

  /// The text of `plan` (notes, settings, claims, then steps), and what each line is:
  /// {claim} or {step} by id, so the plan check's "line N" points at a card.
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
    for (const key of MODEL_HEADERS) {
      for (const line of plan.modelLines ?? []) if (line.startsWith(`${key}:`)) add(line);
    }
    for (const line of [...(plan.kindLines ?? []), ...(plan.aboutLines ?? [])]) add(line);
    if (plan.claims.length) gap();
    for (const c of plan.claims) {
      c.notes?.forEach((n) => note(n, { claim: c.id }));
      add(`claim: ${c.id} ${c.text}`.trimEnd(), { claim: c.id });
    }
    if (plan.steps.length) gap();
    for (const s of plan.steps) {
      s.notes?.forEach((n) => note(n, { step: s.id }));
      add(formatStep(s), { step: s.id });
      for (const l of indented(s.more)) add(l, { step: s.id });
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
      if (isWorkiq(s.target)) {
        if (s.approve || s.team) need("step", i, "target", ERRORS.workiqKind);
        else if (s.options.some((o) => NOT_IN_WORKIQ.has(optionOf(o)))) need("step", i, "target", ERRORS.workiqOption);
      }
      if (s.pr && s.target && !/^@[^:]+:.+/.test(s.target)) need("step", i, "target", ERRORS.prTarget);
      if (s.pr && s.options.some((o) => !IN_PR.has(optionOf(o)))) need("step", i, "target", ERRORS.prOption);
      if (!s.pr && valuesOf(s, "reviewers").length) need("step", i, "target", ERRORS.reviewersOption);
      if (!s.approve && valuesOf(s, "changes").length) need("step", i, "target", ERRORS.changesOption);
      // A pull request step without a title takes that of the work it opens; one that makes the
      // Microsoft 365 changes another proposed (@workiq apply=) needs no words either.
      const makesChanges = s.target === WORKIQ && valuesOf(s, "apply").length > 0;
      if (!s.pr && !makesChanges && blank(taskOf(s), TODO)) {
        need("step", i, "todo", s.approve ? "Say what to check before signing off" : "Say what this step should do");
      }
      if (!s.approve && !s.team && !s.pr && s.target !== WORKIQ && valuesOf(s, "claims").length && !valuesOf(s, "verify").length) {
        need("step", i, "claims", "Without a check, this step can't prove a claim: pick a check, or let a sign-off prove it");
      }
    });
    return out;
  }

  /// How the plan's run is tracked, from its `track:` line (as src/plan.rs reads it):
  /// { kind: "new", title } (no line: title "", or the new issue's title), { kind:
  /// "issue", number }, or { kind: "off" }.
  function trackOf(text) {
    const line = linesOf(text).find((l) => l.kind === "track");
    const value = line ? line.line.trim().slice("track:".length).trim() : "";
    if (/^off$/i.test(value)) return { kind: "off" };
    const n = value.match(/^#?(\d+)$/);
    if (n && Number(n[1]) > 0) return { kind: "issue", number: Number(n[1]) };
    return { kind: "new", title: value };
  }

  /// `text` with its `track:` line set to `value` (`track: value`), put first if it has
  /// none; `value` null takes the line out.
  function setTrack(text, value) {
    const have = linesOf(text).find((l) => l.kind === "track");
    if (!have) return value == null ? text : `track: ${value}\n${text}`;
    if (value != null) return text.slice(0, have.start) + `track: ${value}` + text.slice(have.end);
    return removeLine(text, have);
  }

  function removeLine(text, have) {
    // The line and the line break after it (before it, for the last line).
    return have.end < text.length
      ? text.slice(0, have.start) + text.slice(have.end + 1)
      : text.slice(0, Math.max(0, have.start - 1));
  }

  function modelsFrom(lines) {
    return Object.fromEntries(MODEL_HEADERS.map((key) => [key,
      lines.find((line) => line.startsWith(`${key}:`))?.slice(key.length + 1).trim() ?? null]));
  }

  /// The plan's model and effort (null when absent); a step's own text isn't a setting.
  function modelsOf(text) {
    return modelsFrom(linesOf(text).filter((l) => MODEL_HEADERS.includes(l.kind)).map((l) => l.line.trim()));
  }

  /// Set or remove either header without rewriting the rest of the plan. Omit a key to
  /// leave it alone; null removes it. New headers go after track: and before the body.
  function setModels(text, values) {
    for (const key of MODEL_HEADERS) {
      if (!(key in values)) continue;
      const value = values[key]?.trim();
      const lines = linesOf(text);
      const [first, ...duplicates] = lines.filter((l) => l.kind === key);
      for (const line of duplicates.reverse()) text = removeLine(text, line);
      if (first) {
        text = value ? text.slice(0, first.start) + `${key}: ${value}` + text.slice(first.end) : removeLine(text, first);
      } else if (value) {
        const body = lines.find((l) => l.kind === "claim" || l.kind === "step");
        const before = lines.filter((l) => !body || l.start < body.start);
        const prior = before.filter((l) => l.kind === "track" || (key === "effort" && l.kind === "model")).at(-1);
        if (prior) {
          text = text.slice(0, prior.end) + `\n${key}: ${value}` + text.slice(prior.end);
        } else {
          text = `${key}: ${value}${text ? `\n${text}` : ""}`;
        }
      }
    }
    return text;
  }

  const modelSteps = (text) => linesOf(text).filter((l) => l.kind === "step")
    .map((l) => parseStep(l.line))
    .filter((s) => s.target && !s.approve && !s.pr && !s.team && !isWorkiq(s.target));

  /// The plan's kind: its kind: line's value in lower case ("" if it has none), or null
  /// without the line.
  function planKind(text) {
    const line = linesOf(text).find((l) => l.kind === "kind");
    return line ? line.line.trim().slice("kind:".length).trim().toLowerCase() : null;
  }

  /// `text` with its kind: line set to `value` (`kind: value`), put after track: and
  /// the model settings if it has none; null takes it out, as it does a second one.
  function setKind(text, value) {
    const lines = linesOf(text);
    const [first, ...duplicates] = lines.filter((l) => l.kind === "kind");
    for (const line of duplicates.reverse()) text = removeLine(text, line);
    if (first) return value ? text.slice(0, first.start) + `kind: ${value}` + text.slice(first.end) : removeLine(text, first);
    if (!value) return text;
    const body = lines.find((l) => l.kind === "claim" || l.kind === "step" || l.kind === "about");
    const prior = lines.filter((l) => (!body || l.start < body.start) && (l.kind === "track" || MODEL_HEADERS.includes(l.kind))).at(-1);
    return prior ? `${text.slice(0, prior.end)}\nkind: ${value}${text.slice(prior.end)}` : `kind: ${value}${text ? `\n${text}` : ""}`;
  }

  /// What a tracked run does with the plan's kind, for the line under the Kind choice:
  /// text and { code } parts, none without a kind or tracking. An issue the plan names
  /// keeps its own labels and type (src/tracker.rs).
  function kindNote(text) {
    const kind = KINDS.find((k) => k.value === planKind(text));
    const track = trackOf(text);
    if (!kind || track.kind === "off") return [];
    if (track.kind === "issue" && track.number) {
      return ["The issues the run makes for its steps are labelled ", { code: kind.label },
        ` (and typed Task where the repository has issue types); #${track.number} keeps its labels and type.`];
    }
    return ["The run's issue is labelled ", { code: kind.label }, ` (and typed ${kind.type} where the repository has issue types).`];
  }

  function trackerForPlan(text, state) {
    if (planKind(text) === "task" && state?.tasks) return state.tasks;
    return state?.tracker ?? null;
  }

  /// What running the plan does on GitHub, for the line under the tracking choice:
  /// `track` as trackOf reads it (an issue without a number: its number isn't typed
  /// yet), `tracker` the linked tracker (repo, repo_url, login, visibility, board) and,
  /// for an issue it works on, what looking the issue up found: { issue }, { missing },
  /// { error }, or nothing yet. Parts are text or links ({ text, href, label }); `warn`
  /// marks a note to read before running.
  function trackNote(track, tracker, found = null) {
    // A tracker GitHub can't be reached for has only its settings.
    const base = tracker.repo_url || `https://${tracker.host || "github.com"}/${tracker.repo}`;
    const repo = { text: tracker.repo, href: `${base}/issues`, label: `${tracker.repo} issues on GitHub` };
    const board = tracker.board
      ? { text: `${tracker.board.title} board`, href: tracker.board.url, label: `The ${tracker.board.title} board on GitHub` }
      : null;
    const who = tracker.login ? `, assigned to ${tracker.login}` : "";
    const after = [
      ...(board ? [" They move on the ", board, " as the steps run, and each result is commented on its issue (one that may hold Microsoft 365 content stays off GitHub)."]
        : [" Each step's result is commented on its issue (one that may hold Microsoft 365 content stays off GitHub)."]),
      ...(tracker.visibility === "public" ? [" The repository is public: anyone can read them."]
        : tracker.visibility === "internal" ? [" The repository is internal: everyone in its enterprise can read them."]
          : tracker.visibility === "private" ? [" The repository is private."] : []),
    ];
    const note = (parts, warn = false) => ({ parts, warn });
    if (track.kind === "off") return note(["Nothing goes to GitHub: the run is kept under Evidence runs only."]);
    if (track.kind === "new") {
      const which = track.title ? `the issue “${track.title}”` : "an issue";
      return note([`Running it opens ${which} in `, repo, `, with a sub-issue per step${who}.`, ...after]);
    }
    if (!track.number) return note(["Type the number of an issue in ", repo, ": the run works on it, and its steps become sub-issues of it."]);
    const n = `#${track.number}`;
    if (!found) return note([`Looking up ${n} in `, repo, "…"]);
    if (found.error) return note([`Couldn't look up ${n}: ${found.error}`], true);
    if (!found.issue) return note([repo, ` has no issue ${n}: the plan would run without tracking. Check the number, or choose New issue.`], true);
    const issue = found.issue;
    const title = issue.title.length > 60 ? `${issue.title.slice(0, 59)}…` : issue.title;
    const link = { text: `${n} ${title}`, href: issue.url, label: `Issue ${n} ${issue.title} on GitHub` };
    const steps = `its steps become sub-issues of it (or take up the open ones an earlier run left)${who}.`;
    return issue.open
      ? note(["Running it works on ", link, " in ", repo, `: ${steps}`, ...after])
      : note([link, " is closed in ", repo, `: running it reopens it, and ${steps}`, ...after], true);
  }

  return {
    addPart, addLine, applyFix, stepIds, claimIds, parseStep, formatStep, ERRORS,
    parsePlan, formatPlan, valuesOf, setValues, renameStep, removeStep, renameClaim, removeClaim,
    dependents, needs, isTodo: (text) => blank(text, TODO), isTodoClaim: (text) => blank(text, TODO_CLAIM),
    taskOf, setTask, lineKinds: (text) => linesOf(text).map((l) => ({ kind: l.kind, head: l.head })),
    trackOf, setTrack, trackerForPlan, trackNote, WORKIQ, WORKIQ_SOURCES, isWorkiq, toggleSource, setTarget,
    modelsOf, setModels, modelSteps, KINDS, planKind, setKind, kindNote,
  };
})();
