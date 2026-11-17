// Codex と Cursor の設定に meshlink の MCP サーバーとフックを足す（外す）。
// 書き換える前に一度だけ <ファイル>.bak-meshlink を残す。ほかのフックや MCP サーバーには触らない。
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { CLI, stableNode } from "./state.mjs";

const codexDir = () => process.env.MESHLINK_CODEX_DIR || path.join(os.homedir(), ".codex");
const cursorDir = () => process.env.MESHLINK_CURSOR_DIR || path.join(os.homedir(), ".cursor");

const q = (s) => `'${String(s).replaceAll("'", "'\\''")}'`;
const hookCommand = (agent, event) => `${q(stableNode())} ${q(CLI)} hook ${agent} ${event}`;
const isOurs = (h) => typeof h?.command === "string" && h.command.includes(`${q(CLI)} hook `);

function backup(file) {
  const bak = `${file}.bak-meshlink`;
  if (fs.existsSync(file) && !fs.existsSync(bak)) fs.copyFileSync(file, bak);
}

const readJson = (file, fallback) => {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return fallback;
  }
};
const writeJson = (file, data) => fs.writeFileSync(file, JSON.stringify(data, null, 2) + "\n");

// ---- Codex ----
const CODEX_EVENTS = [
  ["SessionStart", "session-start", { matcher: "*" }, 10],
  ["UserPromptSubmit", "user-prompt-submit", {}, 10],
  ["PostToolUse", "post-tool-use", { matcher: "*" }, 10],
  ["Stop", "stop", {}, 10],
  ["SessionEnd", "session-end", {}, 3],
];
const TOML_START = "# meshlink:start";
const TOML_END = "# meshlink:end";

function setupCodex(remove) {
  const dir = codexDir();
  if (!fs.existsSync(dir)) return null;
  const toml = path.join(dir, "config.toml");
  let text = fs.existsSync(toml) ? fs.readFileSync(toml, "utf8") : "";
  const block = new RegExp(`\\n?${TOML_START}[\\s\\S]*?${TOML_END}\\n?`, "g");
  const cleaned = text.replace(block, "\n").replace(/\n{3,}$/, "\n\n");
  const ours = remove
    ? ""
    : `\n${TOML_START}\n[mcp_servers.meshlink]\ncommand = ${JSON.stringify(stableNode())}\nargs = ${JSON.stringify([CLI, "mcp", "--agent", "codex"])}\ntool_timeout_sec = 120\n${TOML_END}\n`;
  const next = cleaned.replace(/\s*$/, "\n") + ours;
  if (next !== text) {
    backup(toml);
    fs.writeFileSync(toml, next);
  }

  const hooksFile = path.join(dir, "hooks.json");
  const data = readJson(hooksFile, { hooks: {} });
  data.hooks ||= {};
  for (const [event, name, extra, timeout] of CODEX_EVENTS) {
    const groups = (data.hooks[event] || [])
      .map((g) => ({ ...g, hooks: (g.hooks || []).filter((h) => !isOurs(h)) }))
      .filter((g) => g.hooks.length);
    if (!remove) groups.push({ ...extra, hooks: [{ type: "command", command: hookCommand("codex", name), timeout }] });
    if (groups.length) data.hooks[event] = groups;
    else delete data.hooks[event];
  }
  backup(hooksFile);
  writeJson(hooksFile, data);
  return { agent: "codex", files: [toml, hooksFile] };
}

// ---- Cursor ----
const CURSOR_EVENTS = [
  ["sessionStart", "session-start", {}],
  ["beforeSubmitPrompt", "user-prompt-submit", {}],
  ["postToolUse", "post-tool-use", { matcher: "*" }],
  ["stop", "stop", {}],
  ["sessionEnd", "session-end", {}],
];

function setupCursor(remove) {
  const dir = cursorDir();
  if (!fs.existsSync(dir)) return null;
  const mcpFile = path.join(dir, "mcp.json");
  const mcp = readJson(mcpFile, { mcpServers: {} });
  mcp.mcpServers ||= {};
  if (remove) delete mcp.mcpServers.meshlink;
  else mcp.mcpServers.meshlink = { type: "stdio", command: stableNode(), args: [CLI, "mcp", "--agent", "cursor"] };
  backup(mcpFile);
  writeJson(mcpFile, mcp);

  const hooksFile = path.join(dir, "hooks.json");
  const data = readJson(hooksFile, { version: 1, hooks: {} });
  data.version ||= 1;
  data.hooks ||= {};
  for (const [event, name, extra] of CURSOR_EVENTS) {
    const list = (data.hooks[event] || []).filter((h) => !isOurs(h));
    if (!remove) list.push({ command: hookCommand("cursor", name), timeout: 10, ...extra });
    if (list.length) data.hooks[event] = list;
    else delete data.hooks[event];
  }
  backup(hooksFile);
  writeJson(hooksFile, data);
  return { agent: "cursor", files: [mcpFile, hooksFile] };
}

export function setupAgents({ remove = false } = {}) {
  return [setupCodex(remove), setupCursor(remove)].filter(Boolean);
}
