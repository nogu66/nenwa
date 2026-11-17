// 受信箱にメッセージが入ったとき、その持ち主（Grok Bot の routine など）を webhook で起こす。
// 設定は ~/.nenwa/wake/<受信箱>.json に { url, key }。
// 鍵が whsec_ で始まれば Standard Webhooks の署名、それ以外は Authorization: Bearer と X-Automation-Key で送る。
import fs from "node:fs";
import path from "node:path";
import { createHmac, randomUUID } from "node:crypto";
import { HOME_DIR } from "./state.mjs";

export const WAKE_DIR = path.join(HOME_DIR, "wake");
const file = (name) => path.join(WAKE_DIR, `${name}.json`);

export function loadWake(name) {
  try {
    const c = JSON.parse(fs.readFileSync(file(name), "utf8"));
    return c.url && c.key ? c : null;
  } catch {
    return null;
  }
}

export function saveWake(name, url, key) {
  if (!/^https?:\/\//.test(url || "")) throw new Error("webhook の URL が正しくありません");
  if (!key) throw new Error("鍵が空です");
  if (/^https?:\/\//.test(key) || key === url)
    throw new Error("クリップボードに入っているのは鍵ではなく URL のようです。sender key をコピーし直してから実行してください");
  fs.mkdirSync(WAKE_DIR, { recursive: true, mode: 0o700 });
  fs.writeFileSync(file(name), JSON.stringify({ url, key }, null, 2) + "\n", { mode: 0o600 });
}

export function wakeHeaders(key, body) {
  const headers = { "content-type": "application/json" };
  if (key.startsWith("whsec_")) {
    const id = `msg_${randomUUID().replaceAll("-", "")}`;
    const ts = String(Math.floor(Date.now() / 1000));
    const sig = createHmac("sha256", Buffer.from(key.slice(6), "base64")).update(`${id}.${ts}.${body}`).digest("base64");
    Object.assign(headers, { "webhook-id": id, "webhook-timestamp": ts, "webhook-signature": `v1,${sig}` });
  } else {
    Object.assign(headers, { authorization: `Bearer ${key}`, "x-automation-key": key });
  }
  return headers;
}

export async function wake(name, payload) {
  const c = loadWake(name);
  if (!c) return null;
  const body = JSON.stringify(payload);
  const res = await fetch(c.url, { method: "POST", headers: wakeHeaders(c.key, body), body, signal: AbortSignal.timeout(15_000) });
  return res.status;
}
