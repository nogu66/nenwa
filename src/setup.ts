// Codex と Cursor の設定に nenwa の MCP サーバーとフックを足す（外す）。
// 書き換える前に一度だけ <ファイル>.bak-nenwa を残す。ほかのフックや MCP サーバーには触らない。
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { CLI, stableNode } from "./state.ts";

const codexDir = () => process.env.NENWA_CODEX_DIR || path.join(os.homedir(), ".codex");
const cursorDir = () => process.env.NENWA_CURSOR_DIR || path.join(os.homedir(), ".cursor");

const q = (s: string) => `'${String(s).replaceAll("'", "'\\''")}'`;
const hookCommand = (agent: string, event: string) => `${q(stableNode())} ${q(CLI)} hook ${agent} ${event}`;
// パスでは見分けない。フォルダを移したときや、手元の版から npm i -g の版に替えたときに、
// 古い場所を指すフックが残って重なってしまうため。
const OUR_HOOK = /[\\/]cli\.(?:mjs|js|ts)' hook (?:codex|cursor) /;
const isOurs = (h: HookEntry | undefined) => typeof h?.command === "string" && OUR_HOOK.test(h.command);

// フックの設定の 1 件（Codex と Cursor で形がほぼ同じ。使う項目だけ）
interface HookEntry {
  type?: string;
  command?: string;
  timeout?: number;
  matcher?: string;
}
interface HookGroup {
  matcher?: string;
  hooks?: HookEntry[];
}

export interface SetupResult {
  agent: string;
  files: string[];
}

function backup(file: string) {
  const bak = `${file}.bak-nenwa`;
  if (fs.existsSync(file) && !fs.existsSync(bak)) fs.copyFileSync(file, bak);
}

const readJson = <T>(file: string, fallback: T): T => {
  try {
    return JSON.parse(fs.readFileSync(file, "utf8"));
  } catch {
    return fallback;
  }
};
const writeJson = (file: string, data: unknown) => fs.writeFileSync(file, JSON.stringify(data, null, 2) + "\n");

// ---- Codex ----
const CODEX_EVENTS: [string, string, Record<string, string>, number][] = [
  ["SessionStart", "session-start", { matcher: "*" }, 10],
  ["UserPromptSubmit", "user-prompt-submit", {}, 10],
  ["PostToolUse", "post-tool-use", { matcher: "*" }, 10],
  ["Stop", "stop", {}, 10],
  ["SessionEnd", "session-end", {}, 3],
];
const TOML_START = "# nenwa:start";
const TOML_END = "# nenwa:end";

function setupCodex(remove: boolean): SetupResult | null {
  const dir = codexDir();
  if (!fs.existsSync(dir)) return null;
  const toml = path.join(dir, "config.toml");
  let text = fs.existsSync(toml) ? fs.readFileSync(toml, "utf8") : "";
  const block = new RegExp(`\\n?${TOML_START}[\\s\\S]*?${TOML_END}\\n?`, "g");
  const cleaned = text.replace(block, "\n").replace(/\n{3,}$/, "\n\n");
  const ours = remove
    ? ""
    : `\n${TOML_START}\n[mcp_servers.nenwa]\ncommand = ${JSON.stringify(stableNode())}\nargs = ${JSON.stringify([CLI, "mcp", "--agent", "codex"])}\ntool_timeout_sec = 120\n${TOML_END}\n`;
  const next = cleaned.replace(/\s*$/, "\n") + ours;
  if (next !== text) {
    backup(toml);
    fs.writeFileSync(toml, next);
  }

  const hooksFile = path.join(dir, "hooks.json");
  const data = readJson<{ hooks: Record<string, HookGroup[]> }>(hooksFile, { hooks: {} });
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
const CURSOR_EVENTS: [string, string, Record<string, string>][] = [
  ["sessionStart", "session-start", {}],
  ["beforeSubmitPrompt", "user-prompt-submit", {}],
  ["postToolUse", "post-tool-use", { matcher: "*" }],
  ["stop", "stop", {}],
  ["sessionEnd", "session-end", {}],
];

function setupCursor(remove: boolean): SetupResult | null {
  const dir = cursorDir();
  if (!fs.existsSync(dir)) return null;
  const mcpFile = path.join(dir, "mcp.json");
  const mcp = readJson<{ mcpServers: Record<string, unknown> }>(mcpFile, { mcpServers: {} });
  mcp.mcpServers ||= {};
  if (remove) delete mcp.mcpServers.nenwa;
  else mcp.mcpServers.nenwa = { type: "stdio", command: stableNode(), args: [CLI, "mcp", "--agent", "cursor"] };
  backup(mcpFile);
  writeJson(mcpFile, mcp);

  const hooksFile = path.join(dir, "hooks.json");
  const data = readJson<{ version: number; hooks: Record<string, HookEntry[]> }>(hooksFile, { version: 1, hooks: {} });
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

export function setupAgents({ remove = false }: { remove?: boolean } = {}): SetupResult[] {
  return [setupCodex(remove), setupCursor(remove)].filter((r): r is SetupResult => Boolean(r));
}
