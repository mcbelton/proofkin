// Proofkin web UI: task branches. Whether a task branch's work is done with, from the
// server's branch states (src/github.rs BranchState): where its work is, what a pull
// request from it would change, and whether deleting it loses anything. And the Review
// changes dialog, which shows its commits, its proof, its files and its diff before a
// pull request is opened or the branch deleted. The pure helpers at the top are tested
// in node (tests/branches.test.cjs); the dialog uses app.js helpers ($, el, api, toast,
// stamp, externalLink, openPr, deleteBranch) when it opens.
"use strict";

const branchCount = (n, noun) => `${n} ${noun}${n === 1 ? "" : "s"}`;

/** What a pull request from a branch in state `st` would change ("1 commit, 9 files
 * (+213 −100)"), or null when it has nothing the repository's branch lacks. */
function branchChangeText(st) {
  if (!st.ahead && !st.files) return null;
  const files = st.files ? `${branchCount(st.files, "file")} (+${st.added} −${st.removed})` : "no file changes";
  return st.ahead ? `${branchCount(st.ahead, "commit")}, ${files}` : files;
}

/** Where the work of a task branch in state `st` is, for its line on the GitHub card.
 * `lead`: text, its pull request as { number, url }; `note`: what a pull request would
 * change while one is open, else whether deleting the branch loses anything (`safe`:
 * nothing, as its work is on the repository's branch `base` or a pull request merged it
 * as it is); `tone`: how the note reads (safe, warn, or plain). */
function branchStatus(st, base) {
  const pr = st.pull_request ?? null;
  const ref = pr ? { number: pr.number, url: pr.url } : null;
  const changes = branchChangeText(st);
  if (pr?.state === "OPEN") {
    return { lead: ["pull request ", ref, " is open"], note: changes, tone: null, safe: false, open: true };
  }
  if (st.safe_to_delete) {
    const lead = pr?.state === "MERGED" && pr.current ? ["merged in ", ref]
      : st.in_base ? [`its commit is on ${base}`]
      : [`its changes are on ${base}`];
    return { lead, note: "safe to delete", tone: "safe", safe: true, open: false };
  }
  const lead = pr?.state === "MERGED" ? ["merged in ", ref, ", but it changed since"]
    : pr?.state === "CLOSED" ? ["pull request ", ref, " was closed"]
    : st.issues_closed ? ["its issues are closed"]
    : ["no pull request yet"];
  return {
    lead, note: changes ? `not on ${base}: ${changes}` : `not on ${base}`, tone: "warn", safe: false, open: false,
  };
}

/** A status's words, its pull request as #N. */
function branchStatusText(status) {
  const lead = status.lead.map((p) => (typeof p === "string" ? p : `#${p.number}`)).join("");
  return status.note ? `${lead} · ${status.note}` : lead;
}

/** What deleting a task branch in state `st` (null: not known) of repository `slug`
 * loses, for its confirmation: nothing when its work is on the repository's branch
 * `base` or a pull request merged it as it is. */
function branchDeleteNote(st, base, slug) {
  const where = `It goes from this agent's clone of ${slug} and from GitHub, if it was pushed.`;
  if (!st) return `Its commits are lost unless they are on ${base} or a pull request merged them. ${where}`;
  const pr = st.pull_request;
  if (pr?.state === "OPEN") return `Pull request #${pr.number} from it is open: GitHub closes it when the branch goes. ${where}`;
  if (st.safe_to_delete) {
    const why = pr?.state === "MERGED" && pr.current ? `pull request #${pr.number} merged it`
      : st.in_base ? `its commit is on ${base}` : `its changes are on ${base}`;
    return `Nothing is lost: ${why}. ${where}`;
  }
  const changes = branchChangeText(st);
  const work = pr?.state === "MERGED"
    ? `Pull request #${pr.number} merged an earlier version of it; what it has now isn't on ${base}`
    : `Its work isn't on ${base}`;
  return `${work}${changes ? `: ${changes}` : ""}. Deleting the branch loses it${st.issues_closed ? ", even though its issues are closed" : ""}. ${where}`;
}

/** A path in a diff's ---/+++ line ("b/src/a.rs", "/dev/null", quoted when odd). */
function diffPath(text) {
  let path = text.replace(/\t$/, "");
  if (path.startsWith("\"") && path.endsWith("\"")) path = path.slice(1, -1);
  if (path === "/dev/null") return null;
  return path.replace(/^[ab]\//, "");
}

/** The path in a diff's "diff --git a/… b/…" line. */
function diffHeaderPath(line) {
  const rest = line.slice("diff --git ".length);
  const quoted = rest.lastIndexOf(" \"b/");
  if (rest.endsWith("\"") && quoted >= 0) return rest.slice(quoted + 4, -1);
  const at = rest.lastIndexOf(" b/");
  return at >= 0 ? rest.slice(at + 3) : rest;
}

/** A unified diff (git diff) as its files: each { path, lines: [{ kind, text }] }, kind
 * one of meta (a file's header: new, deleted, renamed, binary), hunk, add, del, ctx or
 * note ("\ No newline at end of file"). A hunk's lines are told by its counts, so a
 * removed line reading "-- x" is a removal, not a header. */
function diffSections(diff) {
  const files = [];
  let file = null;
  // Lines of the hunk still to come: old (context and removed) and new (context and added).
  let older = 0;
  let newer = 0;
  const lines = diff.split("\n");
  if (lines[lines.length - 1] === "") lines.pop();
  for (const line of lines) {
    if (file && (older > 0 || newer > 0)) {
      const mark = line[0];
      if (mark === "\\") {
        file.lines.push({ kind: "note", text: line });
      } else if (mark === "+") {
        newer -= 1;
        file.lines.push({ kind: "add", text: line });
      } else if (mark === "-") {
        older -= 1;
        file.lines.push({ kind: "del", text: line });
      } else {
        older -= 1;
        newer -= 1;
        file.lines.push({ kind: "ctx", text: line });
      }
      continue;
    }
    if (line.startsWith("diff --git ")) {
      file = { path: diffHeaderPath(line), lines: [] };
      files.push(file);
      continue;
    }
    if (!file) continue;
    const hunk = /^@@ -\d+(?:,(\d+))? \+\d+(?:,(\d+))? @@/.exec(line);
    if (hunk) {
      older = hunk[1] === undefined ? 1 : Number(hunk[1]);
      newer = hunk[2] === undefined ? 1 : Number(hunk[2]);
      file.lines.push({ kind: "hunk", text: line });
    } else if (line.startsWith("\\")) {
      file.lines.push({ kind: "note", text: line });
    } else if (line.startsWith("+++ ")) {
      file.path = diffPath(line.slice(4)) ?? file.path;
    } else if (line.startsWith("rename to ")) {
      file.path = line.slice("rename to ".length);
      file.lines.push({ kind: "meta", text: line });
    } else if (!line.startsWith("--- ") && !line.startsWith("index ")) {
      file.lines.push({ kind: "meta", text: line });
    }
  }
  return files;
}

// ---------- the GitHub card's branch states, and the Review changes dialog ----------
/** The last states the GitHub card got, by repository and branch name. */
const taskBranchStates = new Map();
const branchKey = (r, name) => `${r.name}\n${name}`;

/** A diff file shows open when it is this short; longer ones open on a click. */
const OPEN_DIFF_LINES = 400;

const branchReview = { repo: null, branch: null, opener: null };

/** The status line for branch `b` of `r`: what branchStatus says, its pull request linked. */
function branchStatusNodes(r, b) {
  const st = taskBranchStates.get(branchKey(r, b.name));
  if (!st) return null;
  const status = branchStatus(st, r.branch);
  const lead = status.lead.map((p) => (typeof p === "string" ? p
    : externalLink(p.url, `#${p.number}`, `pull request #${p.number}`)));
  return {
    status,
    nodes: [...lead, ...(status.note ? [" · ", el("span", {
      class: status.tone === "safe" ? "safe-text" : status.tone === "warn" ? "warn-text" : null, text: status.note,
    })] : [])],
  };
}

/** Opens the review of task branch `b` of linked repository `r`: where its work is, its
 * commits, the proof a pull request from it would carry, its files and its diff. */
async function openBranchReview(r, b, opener) {
  Object.assign(branchReview, { repo: r, branch: b, opener });
  const dialog = $("#branch-dialog");
  $("#branch-h").textContent = `Review ${b.name}`;
  $("#branch-desc").textContent = `${b.name} → ${r.branch} in ${r.slug}`;
  const shown = branchStatusNodes(r, b);
  $("#branch-status").replaceChildren(...(shown?.nodes ?? []));
  $("#branch-status").hidden = !shown;
  // An open pull request is reviewed on GitHub; one with nothing to add needs none.
  $("#branch-delete").hidden = Boolean(shown?.status.open);
  $("#branch-pr").hidden = Boolean(shown && (shown.status.open || shown.status.safe));
  $("#branch-error").textContent = "";
  $("#branch-proof").textContent = "";
  $("#branch-changes").replaceChildren(el("p", { class: "muted small", role: "status" },
    el("span", { class: "spinner", "aria-hidden": "true" }), " Reading its changes…"));
  dialog.showModal();
  $("#branch-close").focus();
  const path = (what) => `/api/repos/${encodeURIComponent(r.name)}/${what}?branch=${encodeURIComponent(b.name)}`;
  const [changes, proof] = await Promise.allSettled([api(path("changes")), api(path("proof"))]);
  if (branchReview.branch !== b || !dialog.open) return;
  $("#branch-proof").textContent = proof.status === "fulfilled"
    ? proof.value.proof?.summary ?? `No proof to add: ${proof.value.reason}`
    : `No proof to add: ${proof.reason.message}`;
  if (changes.status === "rejected") {
    $("#branch-changes").replaceChildren();
    $("#branch-error").textContent = changes.reason.message;
    return;
  }
  renderBranchChanges(changes.value);
}

/** Draws a branch's changes (see src/github.rs BranchChanges) in the review. */
function renderBranchChanges(c) {
  const parts = [];
  parts.push(el("section", { class: "review-part", "aria-labelledby": "branch-commits-h" },
    el("h3", { id: "branch-commits-h", text: `Commits not on ${c.base}` }),
    c.commits.length
      ? el("ul", { class: "review-list" }, ...c.commits.map((m) => el("li", {},
        el("span", { class: "mono", text: m.commit }), " ",
        el("span", { text: m.subject }),
        el("span", { class: "muted", text: ` · ${stamp(m.unix * 1000)}` }))))
      : el("p", { class: "muted small", text: `None: its commit is on ${c.base}.` })));
  const added = c.files.reduce((n, f) => n + (f.added ?? 0), 0);
  const removed = c.files.reduce((n, f) => n + (f.removed ?? 0), 0);
  parts.push(el("section", { class: "review-part", "aria-labelledby": "branch-files-h" },
    el("h3", { id: "branch-files-h", text: c.files.length ? `${branchCount(c.files.length, "file")} (+${added} −${removed})` : "Files" }),
    c.files.length
      ? el("ul", { class: "review-list" }, ...c.files.map((f) => el("li", {},
        el("span", { class: "mono", translate: "no", text: f.path }), " ",
        f.added === null || f.added === undefined
          ? el("span", { class: "muted", text: "binary" })
          : el("span", { class: "diff-counts" },
            el("span", { class: "diff-added", text: `+${f.added}` }), " ",
            el("span", { class: "diff-removed", text: `−${f.removed}` })))))
      : el("p", { class: "muted small", text: "No file changes." })));
  const sections = diffSections(c.diff);
  if (sections.length) {
    // A long file's lines are drawn when it is opened.
    const lines = (f) => f.lines.map((l) => el("span", { class: `diff-${l.kind}`, text: l.text || " " }));
    parts.push(el("section", { class: "review-part", "aria-labelledby": "branch-diff-h" },
      el("h3", { id: "branch-diff-h", text: "Changes" }),
      ...sections.map((f) => {
        const open = f.lines.length <= OPEN_DIFF_LINES;
        const pre = el("pre", { class: "diff", tabindex: "0", translate: "no", "aria-label": `Changes to ${f.path}` }, ...(open ? lines(f) : []));
        return el("details", {
          class: "diff-file", open: open || undefined,
          ontoggle: (e) => { if (e.currentTarget.open && !pre.firstChild) pre.append(...lines(f)); },
        },
        el("summary", {}, el("span", { class: "mono", translate: "no", text: f.path }),
          open ? null : el("span", { class: "muted", text: ` · ${f.lines.length} lines` })),
        pre);
      }),
      c.truncated ? el("p", { class: "warn-text small", text: "The diff stops here: it is too long to show whole. The files above list all it changes." }) : null));
  }
  $("#branch-changes").replaceChildren(...parts);
}

function initBranchReview() {
  const dialog = $("#branch-dialog");
  $("#branch-close").addEventListener("click", () => dialog.close());
  $("#branch-pr").addEventListener("click", () => {
    const { repo, branch } = branchReview;
    dialog.close();
    openPr(repo, branch);
  });
  $("#branch-delete").addEventListener("click", async () => {
    const { repo, branch } = branchReview;
    if (await deleteBranch(repo, branch) && dialog.open) dialog.close();
  });
  // Back to the button that opened it: rebuilt by then (found by its label), or gone.
  dialog.addEventListener("close", () => {
    const opener = branchReview.opener;
    const label = opener?.getAttribute("aria-label");
    const back = opener?.isConnected ? opener
      : label ? [...document.querySelectorAll("#github button[aria-label]")].find((x) => x.getAttribute("aria-label") === label) : null;
    // Deleted: its line is gone, so focus goes to the card.
    if (!$("#pr-dialog").open) (back ?? $("#github button"))?.focus();
  });
}
