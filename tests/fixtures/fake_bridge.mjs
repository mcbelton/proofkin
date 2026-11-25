// Stand-in for bridge/index.mjs speaking the same JSON-lines protocol, so the
// Rust bridge client can be tested without Claude/Codex credentials.
import { createInterface } from "node:readline";

const cancelled = [];

createInterface({ input: process.stdin }).on("line", (line) => {
  const message = JSON.parse(line);
  if (message.cancel !== undefined) {
    cancelled.push(message.cancel);
    return;
  }
  const { id, provider, prompt, access, cwd, model, effort } = message;
  if (prompt === "exit") process.exit(0);
  if (prompt === "hang") return;
  if (prompt === "cancelled?") {
    process.stdout.write(JSON.stringify({ id, ok: true, text: cancelled.join(",") }) + "\n");
    return;
  }
  if (prompt.startsWith("env:")) {
    const names = prompt.slice(4).split(",");
    const text = names.map((n) => `${n}=${process.env[n] ?? "<unset>"}`).join(";");
    process.stdout.write(JSON.stringify({ id, ok: true, text }) + "\n");
    return;
  }
  if (prompt === "usage") {
    const usage = { input_tokens: 1200, cached_input_tokens: 1000, output_tokens: 80, cost_usd: 0.0123 };
    process.stdout.write(JSON.stringify({ id, ok: true, text: "used", usage }) + "\n");
    return;
  }
  if (prompt === "bad usage") {
    const usage = { input_tokens: -1, output_tokens: "lots", cost_usd: "free" };
    process.stdout.write(JSON.stringify({ id, ok: true, text: "used", usage }) + "\n");
    return;
  }
  const delay = prompt === "slow" ? 200 : 0;
  setTimeout(() => {
    const reply = prompt === "fail"
      ? { id, ok: false, error: "boom" }
      : { id, ok: true, text: `${provider}|${access}|${cwd.length > 0}|${prompt}`, model: model ?? `${provider}-default-model`, effort };
    process.stdout.write(JSON.stringify(reply) + "\n");
  }, delay);
});
