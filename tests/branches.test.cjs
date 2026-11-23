// Cases for task branches on the GitHub card (ui/branches.js): where a branch's work is,
// whether deleting it loses anything (and what the confirmation says), and a diff read
// into its files' lines for the Review changes dialog. tests/ui.rs runs this with node.
const assert = require("node:assert");
const fs = require("node:fs");
const path = require("node:path");
const vm = require("node:vm");

const code = fs.readFileSync(path.join(__dirname, "..", "ui", "branches.js"), "utf8");
const { branchChangeText, branchStatus, branchStatusText, branchDeleteNote, diffSections, diffHeaderPath } =
  vm.runInThisContext(`${code}\n;({ branchChangeText, branchStatus, branchStatusText, branchDeleteNote, diffSections, diffHeaderPath })`);

const url = (n) => `https://github.com/acme/app/pull/${n}`;
const pr = (number, state, current = true) => ({ number, state, url: url(number), current });
const work = { ahead: 1, files: 9, added: 213, removed: 100 };
const say = (st) => branchStatusText(branchStatus(st, "main"));

// What a pull request would change.
assert.strictEqual(branchChangeText(work), "1 commit, 9 files (+213 −100)");
assert.strictEqual(branchChangeText({ ahead: 2, files: 1, added: 1, removed: 0 }), "2 commits, 1 file (+1 −0)");
assert.strictEqual(branchChangeText({ ahead: 1, files: 0, added: 0, removed: 0 }), "1 commit, no file changes");
assert.strictEqual(branchChangeText({ ahead: 0, files: 0 }), null);

// Safe to delete: merged as it is, its commit on main, or its changes there.
let status = branchStatus({ ...work, pull_request: pr(7, "MERGED"), safe_to_delete: true }, "main");
assert.deepStrictEqual(status.lead, ["merged in ", { number: 7, url: url(7) }]);
assert.deepStrictEqual([status.safe, status.open, status.tone], [true, false, "safe"]);
assert.strictEqual(branchStatusText(status), "merged in #7 · safe to delete");
assert.strictEqual(say({ ahead: 0, files: 0, in_base: true, same_as_base: true, safe_to_delete: true }),
  "its commit is on main · safe to delete");
assert.strictEqual(say({ ...work, same_as_base: true, safe_to_delete: true, pull_request: pr(3, "CLOSED") }),
  "its changes are on main · safe to delete");

// Not safe: closed issues don't mean the work is on main (what the card said before).
status = branchStatus({ ...work, issues_closed: true }, "main");
assert.deepStrictEqual([status.safe, status.tone], [false, "warn"]);
assert.strictEqual(branchStatusText(status), "its issues are closed · not on main: 1 commit, 9 files (+213 −100)");
assert.strictEqual(say({ ...work, ahead: 2, pull_request: pr(7, "MERGED", false) }),
  "merged in #7, but it changed since · not on main: 2 commits, 9 files (+213 −100)");
assert.strictEqual(say({ ...work, pull_request: pr(3, "CLOSED") }), "pull request #3 was closed · not on main: 1 commit, 9 files (+213 −100)");
assert.strictEqual(say(work), "no pull request yet · not on main: 1 commit, 9 files (+213 −100)");
assert.strictEqual(say({ ahead: 0, files: 0 }), "no pull request yet · not on main");
// An open pull request: what it changes; deleting the branch would close it.
status = branchStatus({ ...work, pull_request: pr(12, "OPEN") }, "main");
assert.deepStrictEqual([status.open, status.safe, status.tone], [true, false, null]);
assert.strictEqual(branchStatusText(status), "pull request #12 is open · 1 commit, 9 files (+213 −100)");

// The delete confirmation says what is lost.
const where = "It goes from this agent's clone of acme/app and from GitHub, if it was pushed.";
const note = (st) => branchDeleteNote(st, "main", "acme/app");
assert.strictEqual(note({ ...work, pull_request: pr(7, "MERGED"), safe_to_delete: true }),
  `Nothing is lost: pull request #7 merged it. ${where}`);
assert.strictEqual(note({ in_base: true, safe_to_delete: true }), `Nothing is lost: its commit is on main. ${where}`);
assert.strictEqual(note({ ...work, same_as_base: true, safe_to_delete: true }), `Nothing is lost: its changes are on main. ${where}`);
assert.strictEqual(note({ ...work, issues_closed: true }),
  `Its work isn't on main: 1 commit, 9 files (+213 −100). Deleting the branch loses it, even though its issues are closed. ${where}`);
assert.strictEqual(note({ ...work, pull_request: pr(7, "MERGED", false) }),
  `Pull request #7 merged an earlier version of it; what it has now isn't on main: 1 commit, 9 files (+213 −100). Deleting the branch loses it. ${where}`);
assert.strictEqual(note({ ...work, pull_request: pr(12, "OPEN") }),
  `Pull request #12 from it is open: GitHub closes it when the branch goes. ${where}`);
assert.strictEqual(note(null), `Its commits are lost unless they are on main or a pull request merged them. ${where}`);

// A diff read into files: a hunk's lines by its counts (a removed "-- x" line is no
// header), new, renamed and deleted files, and "\ No newline" notes.
const diff = [
  "diff --git a/src/a.rs b/src/a.rs",
  "index 1111111..2222222 100644",
  "--- a/src/a.rs",
  "+++ b/src/a.rs",
  "@@ -1,3 +1,3 @@ fn a",
  " fn a() {}",
  "--- a comment that was removed",
  "+// a comment",
  " fn b() {}",
  "diff --git a/new.txt b/new.txt",
  "new file mode 100644",
  "index 0000000..3333333",
  "--- /dev/null",
  "+++ b/new.txt",
  "@@ -0,0 +1 @@",
  "+new",
  "\\ No newline at end of file",
  "diff --git a/old name.txt b/new name.txt",
  "similarity index 100%",
  "rename from old name.txt",
  "rename to new name.txt",
  "diff --git a/gone.bin b/gone.bin",
  "deleted file mode 100644",
  "index 4444444..0000000",
  "Binary files a/gone.bin and /dev/null differ",
  "diff --git a/b b/b",
  "@@ -1,2 +1,2 @@",
  "",
  "-x",
  "+y",
  "",
].join("\n");
const files = diffSections(diff);
assert.deepStrictEqual(files.map((f) => f.path), ["src/a.rs", "new.txt", "new name.txt", "gone.bin", "b"]);
const kinds = (f) => f.lines.map((l) => l.kind);
assert.deepStrictEqual(kinds(files[0]), ["hunk", "ctx", "del", "add", "ctx"]);
assert.strictEqual(files[0].lines[2].text, "--- a comment that was removed");
assert.deepStrictEqual(kinds(files[1]), ["meta", "hunk", "add", "note"]);
assert.deepStrictEqual(files[2].lines.map((l) => l.text), ["similarity index 100%", "rename from old name.txt", "rename to new name.txt"]);
assert.deepStrictEqual(files[3].lines.map((l) => l.text), ["deleted file mode 100644", "Binary files a/gone.bin and /dev/null differ"]);
// A context line whose space was stripped is still context.
assert.deepStrictEqual(kinds(files[4]), ["hunk", "ctx", "del", "add"]);
assert.deepStrictEqual(diffSections(""), []);
assert.strictEqual(diffHeaderPath("diff --git \"a/x y\" \"b/x y\""), "x y");

console.log("ok: task branches");
