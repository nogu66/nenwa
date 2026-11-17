// 状態ファイルの場所と、デーモンへの問い合わせ（1 行 1 JSON の unix ソケット）。
import fs from "node:fs";
import net from "node:net";
import os from "node:os";
import path from "node:path";
import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

export const HOME_DIR = process.env.NENWA_HOME || path.join(os.homedir(), ".nenwa");
export const SOCK = path.join(HOME_DIR, "sock");
export const CONFIG = path.join(HOME_DIR, "config.json");
export const OUTBOX = path.join(HOME_DIR, "outbox.json");
export const LOG = path.join(HOME_DIR, "daemon.log");
export const INBOX_DIR = path.join(HOME_DIR, "inbox");
export const MAILBOXES = path.join(HOME_DIR, "mailboxes.json");
export const AGENTS = path.join(HOME_DIR, "agents.json");
export const CLI = fileURLToPath(new URL("./cli.mjs", import.meta.url));

export function loadConfig() {
  try {
    return JSON.parse(fs.readFileSync(CONFIG, "utf8"));
  } catch {
    return null;
  }
}

export function saveConfig(config) {
  fs.mkdirSync(HOME_DIR, { recursive: true, mode: 0o700 });
  fs.writeFileSync(CONFIG, JSON.stringify(config, null, 2) + "\n", { mode: 0o600 });
}

// 届いた断片を改行で区切り、1 行ずつ onLine に渡す。ソケットと stdin の両方で使う。
export function lineReader(onLine, maxBytes = 1 << 20) {
  let buf = "";
  return (chunk) => {
    buf += chunk;
    if (buf.length > maxBytes) throw new Error("line too long");
    let i;
    while ((i = buf.indexOf("\n")) >= 0) {
      const line = buf.slice(0, i).trim();
      buf = buf.slice(i + 1);
      if (line) onLine(line);
    }
  };
}

function once(req) {
  return new Promise((resolve, reject) => {
    const c = net.createConnection(SOCK);
    c.setEncoding("utf8");
    c.on("connect", () => c.write(JSON.stringify(req) + "\n"));
    c.on("data", lineReader((line) => {
      c.end();
      resolve(JSON.parse(line));
    }));
    c.on("error", reject);
  });
}

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

// デーモンが動いていれば聞き、動いていなければ null（起動はしない）。
export async function tryRequest(req) {
  try {
    return await once(req);
  } catch {
    return null;
  }
}

// node の場所。Homebrew だと process.execPath がバージョン番号入り（.../Cellar/node/26.12.1_1/...）になり、
// 更新すると消える。同じ node を指す、バージョン番号のない場所があればそちらを使う。
export function stableNode() {
  const real = fs.realpathSync(process.execPath);
  for (const p of ["/opt/homebrew/bin/node", "/usr/local/bin/node"]) {
    try {
      if (fs.realpathSync(p) === real) return p;
    } catch {}
  }
  return process.execPath;
}

// デーモンが動いていなければ裏で起動してから聞く。
export async function request(req) {
  try {
    return await once(req);
  } catch (e) {
    if (!["ENOENT", "ECONNREFUSED"].includes(e.code)) throw e;
  }
  if (!loadConfig()) throw new Error("まだチームがありません。nenwa init か nenwa join <招待> を先に実行してください");
  const log = fs.openSync(LOG, "a");
  spawn(process.execPath, [CLI, "daemon"], { detached: true, stdio: ["ignore", log, log], env: process.env }).unref();
  for (let i = 0; i < 50; i++) {
    await sleep(100);
    try {
      return await once(req);
    } catch {}
  }
  throw new Error(`デーモンが起動しませんでした。${LOG} を見てください`);
}
