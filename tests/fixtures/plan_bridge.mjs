// Stand-in for bridge/index.mjs that drafts plans for `proofkin plan draft`, speaking the
// same JSON-lines protocol: it answers with prose around a plan, which names a peer that
// isn't connected when the prompt mentions ghost, and has a sign-off after its step when
// the prompt asks for one.
import { createInterface } from "node:readline";

createInterface({ input: process.stdin }).on("line", (line) => {
  const message = JSON.parse(line);
  if (message.cancel !== undefined) return;
  const { id, prompt, access, cwd } = message;
  let plan = prompt.includes("ghost") ? "fix @ghost:app Fix the bug" : "ok approve @me Ship it?";
  if (prompt.includes("add a second sign-off")) plan += "\nagain after ok approve @me Ship it, really?";
  const text = [
    `Here is a plan (${access}, ${cwd.length > 0 ? "in a folder" : "nowhere"}).`,
    "",
    "```plan",
    plan,
    "```",
    "",
    "Review it before you run it.",
  ].join("\n");
  process.stdout.write(JSON.stringify({ id, ok: true, text, model: "fake-planner" }) + "\n");
});
