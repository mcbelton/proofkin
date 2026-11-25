// Bridge between proofkin (Rust) and the official Claude Agent SDK and OpenAI Codex SDK.
//
// Protocol: one JSON object per line.
//   stdin : {"id":1,"provider":"claude"|"codex","prompt":"...","cwd":"...","access":"read-only"|"workspace-write"|"full","model":null,
//            "effort":"minimal"|"low"|"medium"|"high"|"xhigh"|"max" (optional),
//            "mcp":{"name":{"type":"stdio","command":"...","args":[],"env":{},"tools":[]}}}  (mcp optional)
//   stdout: {"id":1,"ok":true,"text":"...","model":"...","effort":"...","usage":{...}} or {"id":1,"ok":false,"error":"..."}
//           (model: the model that answered, when the SDK says; effort: the effort it was given;
//            usage: input_tokens (cached ones included), cached_input_tokens, output_tokens and
//            cost_usd, each only when the SDK reports it)
// stdout is reserved for protocol lines; all logging goes to stderr.
import { existsSync, readFileSync, realpathSync } from "node:fs";
import { homedir } from "node:os";
import { basename, extname, isAbsolute, join, relative, resolve, sep } from "node:path";
import { createInterface } from "node:readline";

console.log = console.info = console.debug = (...args) => console.error(...args);

const READ = ["Read", "Glob", "Grep"];
const WRITE = ["Edit", "Write", "NotebookEdit"];
export const CLAUDE_TOOLS = {
  "read-only": READ,
  "workspace-write": [...READ, ...WRITE],
  full: [...READ, ...WRITE, "Bash"],
};
// Codex runs commands in its own sandbox; "full" never means danger-full-access.
const CODEX_SANDBOX = {
  "read-only": "read-only",
  "workspace-write": "workspace-write",
  full: "workspace-write",
};

// Same rules as src/llm/policy.rs.
const SENSITIVE_DIRS = [".ssh", ".gnupg", ".aws", ".azure", ".kube", ".docker", ".proofkin", ".p2pagents", ".copilot"];
const SENSITIVE_FILES = [
  ".env", ".envrc", ".npmrc", ".yarnrc.yml", ".pypirc", ".netrc", "_netrc", ".pgpass",
  ".git-credentials", ".htpasswd", "credentials", "credentials.json", "secrets.json",
  "secrets.yaml", "secrets.yml", "secrets.toml",
];
const SENSITIVE_EXTENSIONS = ["pem", "key", "p12", "pfx", "jks", "keystore", "kdbx", "ppk", "gpg", "asc"];
const PROTECTED_DIRS = [".git", ".vscode", ".idea", ".claude", ".codex", ".copilot", ".cursor"];
const PROTECTED_FILES = [".mcp.json", ".gitmodules"];
const TEMPLATE_SUFFIXES = [".example", ".sample", ".template", ".dist"];

/** Resolves symlinks on the existing part of `path`. */
function real(path) {
  const missing = [];
  let current = path;
  for (;;) {
    try {
      return resolve(realpathSync.native(current), ...missing.reverse());
    } catch {
      const parent = resolve(current, "..");
      if (parent === current) return path;
      missing.push(basename(current));
      current = parent;
    }
  }
}

function sensitive(parts) {
  const names = parts.map((p) => p.toLowerCase());
  if (names.some((n) => SENSITIVE_DIRS.includes(n) || n.startsWith("id_rsa"))) return true;
  const file = names.at(-1) ?? "";
  if (SENSITIVE_FILES.includes(file)) return true;
  if (file.startsWith(".env.") && !TEMPLATE_SUFFIXES.some((s) => file.endsWith(s))) return true;
  if (["id_dsa", "id_ecdsa", "id_ed25519"].some((k) => file.startsWith(k))) return true;
  return SENSITIVE_EXTENSIONS.includes(extname(file).slice(1));
}

function protectedPath(parts) {
  const names = parts.map((p) => p.toLowerCase());
  return names.some((n) => PROTECTED_DIRS.includes(n))
    || PROTECTED_FILES.includes(names.at(-1) ?? "")
    || (names[0] === ".github" && names[1] === "hooks");
}

/** Why `path` may not be read (or written) in workspace `cwd`, or null. */
export function pathRefusal(cwd, path, write = false) {
  if (typeof path !== "string" || !path) return "missing path";
  const root = real(resolve(cwd));
  const target = real(resolve(root, path));
  const rel = relative(root, target);
  if (rel === ".." || rel.startsWith(`..${sep}`) || isAbsolute(rel)) {
    return `${path} is outside the workspace ${root}`;
  }
  const parts = rel === "" ? [] : rel.split(sep);
  if (sensitive(parts)) return `${path} may contain credentials`;
  if (write && protectedPath(parts)) return `${path} is tool configuration that could run code; edit it yourself`;
  return null;
}

const HOME_REFS = [
  "$home", "${home}", "$env:userprofile", "$env:home", "$env:appdata", "$env:localappdata",
  "%userprofile%", "%appdata%", "%localappdata%", "~/", "~\\",
];
const NULL_DEVICES = ["/dev/null", "/dev/stdout", "/dev/stderr", "nul"];

/** Best-effort scan of a shell command (same rules as Workspace::check_command); not a sandbox. */
export function commandRefusal(cwd, text) {
  if (typeof text !== "string" || !text) return "missing command";
  const lower = text.toLowerCase();
  if (lower.includes("://")) return "shell commands may not access URLs";
  if (HOME_REFS.some((r) => lower.includes(r))) return "shell commands may not reference the home folder";
  for (const token of text.split(/[\s'"`;|&()<>,={}]+/)) {
    if (!token || token.startsWith("-") || NULL_DEVICES.includes(token)) continue;
    if (process.platform === "win32" && token.startsWith("/")) continue;
    if (token === "~") return "shell commands may not reference the home folder";
    const reason = pathRefusal(cwd, token);
    if (reason) return reason;
  }
  return null;
}

/** Why Claude may not run `tool` with `input` at `access`, or null. `mcp`: the task's MCP servers. */
export function toolRefusal(cwd, access, tool, input = {}, mcp = {}) {
  if (typeof tool === "string" && tool.startsWith("mcp__")) return mcpRefusal(mcp, tool);
  if (!CLAUDE_TOOLS[access]?.includes(tool)) return `${tool} is not allowed with ${access} access`;
  switch (tool) {
    case "Read":
      return pathRefusal(cwd, input.file_path);
    case "Glob":
    case "Grep": {
      const pattern = tool === "Glob" ? input.pattern : input.glob;
      if (typeof pattern === "string" && (isAbsolute(pattern) || pattern.split(/[\\/]/).includes(".."))) {
        return "search patterns must stay inside the workspace";
      }
      return input.path ? pathRefusal(cwd, input.path) : null;
    }
    case "Edit":
    case "Write":
      return pathRefusal(cwd, input.file_path, true);
    case "NotebookEdit":
      return pathRefusal(cwd, input.notebook_path, true);
    case "Bash":
      return commandRefusal(cwd, input.command);
    default:
      return null;
  }
}

// Same list as HOME_SECRETS in src/llm/policy.rs.
const HOME_SECRETS = [
  ".ssh", ".aws", ".azure", ".gnupg", ".kube", ".docker", ".proofkin", ".p2pagents", ".copilot",
  ".config/gh", ".git-credentials", ".netrc", ".npmrc", ".pypirc",
];

/** How Claude names MCP servers and tools in `mcp__<server>__<tool>`. */
const mcpName = (name) => String(name).replace(/[^a-zA-Z0-9_-]/g, "_");

/**
 * The server and tool of Claude tool name `mcp__<server>__<tool>`. Proofkin's server
 * names have no `__` and don't start or end with `_`, so the first `__` after
 * `mcp__` ends the server's name.
 */
function mcpParts(tool) {
  if (typeof tool !== "string" || !tool.startsWith("mcp__")) return null;
  const rest = tool.slice(5);
  const end = rest.indexOf("__");
  if (end <= 0) return null;
  return { server: rest.slice(0, end), tool: rest.slice(end + 2) };
}

/** Why Claude may not call MCP tool `tool` (`mcp__<server>__<tool>`), or null. */
export function mcpRefusal(mcp, tool) {
  const parts = mcpParts(tool);
  const entry = parts && Object.entries(mcp ?? {}).find(([name]) => mcpName(name) === parts.server);
  if (!entry) return `${tool} is not from an MCP server this task may use`;
  const [name, server] = entry;
  const allowed = server.tools ?? [];
  if (allowed.length === 0 || allowed.some((t) => mcpName(t) === parts.tool)) return null;
  return `${tool} is not one of the tools the owner allows on MCP server ${name}`;
}

// Same envelope as src/boundary.rs: MCP results are external content.
const TAG = "external_content";

/** `body` in an `<external_content source="...">` envelope it can't close or reopen. */
export function externalContent(source, body) {
  const label = String(source).replace(/[^\p{L}\p{N} :._\-/@]/gu, "_").slice(0, 120);
  const text = String(body)
    .replace(/<\s*\/\s*external_content\b[^>]*>/gi, `<\\/${TAG}>`)
    .replace(/<\s*external_content\b/gi, `<\\${TAG}`);
  return `<${TAG} source="${label}">\n${text.trimEnd()}\n</${TAG}>`;
}

/** A tool's output with every text the model reads in an envelope. */
export function wrapToolOutput(source, output) {
  if (typeof output === "string") return externalContent(source, output);
  if (Array.isArray(output)) {
    return output.map((item) => {
      if (typeof item === "string") return externalContent(source, item);
      if (item && typeof item === "object" && typeof item.text === "string") {
        return { ...item, text: externalContent(source, item.text) };
      }
      return item;
    });
  }
  if (output && typeof output === "object") {
    if (Array.isArray(output.content)) return { ...output, content: wrapToolOutput(source, output.content) };
    if (typeof output.text === "string") return { ...output, text: externalContent(source, output.text) };
  }
  return externalContent(source, JSON.stringify(output ?? null));
}

/** The task's MCP servers in the Claude Agent SDK's form, and the tools to pre-approve. */
export function claudeMcp(mcp) {
  const servers = {};
  const allowed = [];
  for (const [name, server] of Object.entries(mcp ?? {})) {
    const { tools = [], ...config } = server;
    // Tool search isn't pre-approved, so the server's tools must be loaded up front.
    servers[name] = { ...config, alwaysLoad: true };
    if (tools.length === 0) allowed.push(`mcp__${mcpName(name)}`);
    else allowed.push(...tools.map((t) => `mcp__${mcpName(name)}__${mcpName(t)}`));
  }
  return { servers, allowed };
}

/**
 * Claude's OS sandbox for Bash at full access: no network, no unsandboxed escape,
 * credential folders unreadable, and the task fails if the sandbox can't start.
 */
export function claudeSandbox(access) {
  if (access !== "full") return undefined;
  const home = homedir();
  const denyRead = HOME_SECRETS.map((rel) => join(home, rel)).filter((p) => existsSync(p));
  for (const root of [process.env.PROOFKIN_HOME, process.env.P2PAGENTS_HOME]) {
    if (root && existsSync(root)) denyRead.push(root);
  }
  return {
    enabled: true,
    failIfUnavailable: true,
    allowUnsandboxedCommands: false,
    autoAllowBashIfSandboxed: true,
    network: { allowedDomains: [] },
    filesystem: { denyRead },
  };
}

/** A reasoning effort as the Claude Agent SDK takes it (it has no "minimal"). */
export function claudeEffort(effort) {
  if (!effort) return undefined;
  return effort === "minimal" ? "low" : ["low", "medium", "high", "xhigh", "max"].includes(effort) ? effort : undefined;
}

async function runClaude({ prompt, cwd, model, effort, access, mcp }, abortController) {
  const { query } = await import("@anthropic-ai/claude-agent-sdk");
  const servers = claudeMcp(mcp);
  const level = claudeEffort(effort);
  const guard = async (input) => {
    const reason = toolRefusal(cwd, access, input.tool_name, input.tool_input, mcp);
    if (!reason) return {};
    console.error(`  claude: denied ${input.tool_name} (${reason})`);
    return {
      hookSpecificOutput: {
        hookEventName: "PreToolUse",
        permissionDecision: "deny",
        permissionDecisionReason: `denied by proofkin: ${reason}`,
      },
    };
  };
  // MCP results come from outside the task: the model gets them in an envelope.
  const spotlight = async (input) => {
    const parts = mcpParts(input.tool_name);
    if (!parts) return {};
    return {
      hookSpecificOutput: {
        hookEventName: "PostToolUse",
        updatedToolOutput: wrapToolOutput(`mcp:${parts.server}/${parts.tool}`, input.tool_response),
      },
    };
  };
  let result;
  let used;
  for await (const message of query({
    prompt,
    options: {
      cwd,
      model: model ?? undefined,
      effort: level,
      // Deny anything not pre-approved; no user/project settings can widen it.
      permissionMode: "dontAsk",
      allowedTools: [...CLAUDE_TOOLS[access], ...servers.allowed],
      disallowedTools: ["WebFetch", "WebSearch", "Task"],
      settingSources: [],
      // Only the owner's servers for this task; never a project's .mcp.json.
      mcpServers: servers.servers,
      strictMcpConfig: true,
      sandbox: claudeSandbox(access),
      abortController,
      // Checks every tool call's paths, including pre-approved tools.
      hooks: { PreToolUse: [{ hooks: [guard] }], PostToolUse: [{ hooks: [spotlight] }] },
    },
  })) {
    // The session's first message says which model answers.
    if (message.type === "system" && message.subtype === "init") used = message.model;
    if (message.type === "result") result = message;
  }
  if (!result) throw new Error("Claude returned no result");
  if (result.subtype !== "success" || result.is_error) {
    const detail = result.result ?? (result.errors ?? []).join("; ");
    throw new Error(`Claude ${result.subtype}: ${detail}`);
  }
  return { text: result.result, model: used ?? model ?? undefined, effort: level, usage: claudeUsage(result) };
}

/** A whole count, or undefined. */
const count = (n) => (Number.isSafeInteger(n) && n >= 0 ? n : undefined);

/** What a Claude query() used, from its result: the tokens of every model it called
 * (cache reads and writes are input too) and its cost estimate. None if it says nothing. */
export function claudeUsage(result) {
  const models = Object.values(result?.modelUsage ?? {});
  if (models.length === 0) return undefined;
  const sum = (key) => models.reduce((n, m) => n + (count(m?.[key]) ?? 0), 0);
  const cached = sum("cacheReadInputTokens");
  const cost = result.total_cost_usd;
  return {
    input_tokens: sum("inputTokens") + cached + sum("cacheCreationInputTokens"),
    cached_input_tokens: cached,
    output_tokens: sum("outputTokens"),
    cost_usd: Number.isFinite(cost) && cost >= 0 ? cost : undefined,
  };
}

/** What a Codex turn used, as it reports it. None if it says nothing. */
export function codexUsage(turn) {
  const usage = turn?.usage;
  if (!usage) return undefined;
  return {
    input_tokens: count(usage.input_tokens),
    cached_input_tokens: count(usage.cached_input_tokens),
    output_tokens: count(usage.output_tokens),
  };
}

/** The model Codex uses when a task names none: `model` at the top of this agent's own
 * `config.toml` (in `CODEX_HOME`), if it sets one. */
export function codexConfigModel(home = process.env.CODEX_HOME ?? join(homedir(), ".codex")) {
  let text;
  try {
    text = readFileSync(join(home, "config.toml"), "utf8");
  } catch {
    return undefined;
  }
  for (const line of text.split(/\r?\n/)) {
    // Keys after a [table] header belong to it, not to the top level.
    if (/^\s*\[/.test(line)) break;
    const found = line.match(/^\s*model\s*=\s*(?:"([^"]+)"|'([^']+)')/);
    if (found) return found[1] ?? found[2];
  }
  return undefined;
}

let codex;
async function runCodex({ prompt, cwd, model, effort, access, mcp }, abortController) {
  if (Object.keys(mcp ?? {}).length > 0) throw new Error("Codex tasks can't use MCP servers here");
  const { Codex } = await import("@openai/codex-sdk");
  codex ??= new Codex({
    // Credentials live in this agent's own CODEX_HOME (set by proofkin), never the OS keyring.
    config: { cli_auth_credentials_store: "file" },
  });
  const thread = codex.startThread({
    workingDirectory: cwd,
    model: model ?? undefined,
    modelReasoningEffort: effort ?? undefined,
    sandboxMode: CODEX_SANDBOX[access],
    approvalPolicy: "never",
    networkAccessEnabled: false,
    webSearchMode: "disabled",
    skipGitRepoCheck: true,
  });
  const turn = await thread.run(prompt, { signal: abortController.signal });
  return {
    text: turn.finalResponse,
    model: model ?? codexConfigModel(),
    effort: effort ?? undefined,
    usage: codexUsage(turn),
  };
}

const PROVIDERS = { claude: runClaude, codex: runCodex };
/** Abort controllers of running requests, by id. */
const running = new Map();

function reply(message) {
  process.stdout.write(JSON.stringify(message) + "\n");
}

async function handle(line) {
  let request;
  try {
    request = JSON.parse(line);
  } catch {
    console.error(`bridge: ignoring malformed line: ${line}`);
    return;
  }
  if (request.cancel !== undefined) {
    // {"cancel": id}: the agent dropped the task; stop the model run.
    running.get(request.cancel)?.abort();
    return;
  }
  const { id, provider, access } = request;
  const controller = new AbortController();
  running.set(id, controller);
  try {
    const run = PROVIDERS[provider];
    if (!run) throw new Error(`unknown provider: ${provider}`);
    if (!CLAUDE_TOOLS[access]) throw new Error(`unknown access: ${access}`);
    const { text, model, effort, usage } = await run(request, controller);
    reply({ id, ok: true, text, model: model || undefined, effort: effort || undefined, usage });
  } catch (err) {
    reply({ id, ok: false, error: controller.signal.aborted ? "cancelled" : String(err?.message ?? err) });
  } finally {
    running.delete(id);
  }
}

// Tests import this module for its policy functions and set PROOFKIN_BRIDGE_IMPORT_ONLY.
if (!process.env.PROOFKIN_BRIDGE_IMPORT_ONLY && !process.env.P2PAGENTS_BRIDGE_IMPORT_ONLY) {
  createInterface({ input: process.stdin }).on("line", (line) => {
    if (line.trim()) handle(line);
  });
}
