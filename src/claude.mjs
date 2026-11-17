// Claude Code のセッションを見つけて、メッセージを押し込む。
// 開いているセッションは ~/.claude/sessions/<pid>.json に自分の受信ソケット（messagingSocketPath）を書いている。
// そこに {"type":"user",...} を 1 行書くと、ツール呼び出しの合間に届き、待機中ならそこでターンが始まる。
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { execFileSync } from "node:child_process";

const sessionsDir = () =>
  path.join(process.env.CLAUDE_CONFIG_DIR || path.join(os.homedir(), ".claude"), "sessions");

export function alive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch (e) {
    return e.code === "EPERM";
  }
}

function readSessions() {
  let names = [];
  try {
    names = fs.readdirSync(sessionsDir()).filter((n) => /^\d+\.json$/.test(n));
  } catch {
    return [];
  }
  return names
    .map((n) => {
      try {
        return JSON.parse(fs.readFileSync(path.join(sessionsDir(), n), "utf8"));
      } catch {
        return null;
      }
    })
    .filter((s) => s && s.sessionId && s.messagingSocketPath && alive(s.pid));
}

const slug = (s) =>
  s.toLowerCase().replace(/[^a-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40) || "session";

// 宛先に使う名前は「フォルダ名-セッション ID の先頭 4 文字」。ほかのセッションが開いても閉じても変わらない。
export function claudeSessions() {
  return readSessions()
    .map((s) => ({ ...s, label: `${slug(path.basename(s.cwd || ""))}-${s.sessionId.slice(0, 4)}` }))
    .sort((a, b) => a.label.localeCompare(b.label));
}

// MCP サーバーを起動した Claude Code の会話。環境変数が一番確か。なければ親プロセスをさかのぼる。
export function callerSessionId() {
  if (process.env.CLAUDE_CODE_SESSION_ID) return process.env.CLAUDE_CODE_SESSION_ID;
  let pid = process.ppid;
  for (let i = 0; i < 6 && pid > 1; i++) {
    try {
      return JSON.parse(fs.readFileSync(path.join(sessionsDir(), `${pid}.json`), "utf8")).sessionId;
    } catch {}
    try {
      pid = Number(execFileSync("ps", ["-o", "ppid=", "-p", String(pid)], { encoding: "utf8" }).trim());
    } catch {
      return null;
    }
  }
  return null;
}

export function post(socketPath, text) {
  return new Promise((resolve, reject) => {
    const c = net.createConnection(socketPath, () => {
      c.end(JSON.stringify({ type: "user", message: { role: "user", content: text } }) + "\n");
    });
    c.on("error", reject);
    c.on("close", resolve);
  });
}
