// パソコンに 1 つ常駐するデーモン。
// - 外側: Hyperswarm（Noise で暗号化した P2P）。チーム鍵のハッシュを topic にして仲間を探し、HMAC で合言葉を確かめる。
// - 内側: unix ソケット。CLI と MCP サーバーがここに who / send を聞きに来る。
// - 配送: 宛先のセッションの受信ソケットに書く（claude.mjs の post）。相手がオフラインなら outbox に置いて、つながったら送る。
// - 受信箱: Claude Code 以外（Grok Bot など）は、受信箱に貯めて check_inbox で読ませ、webhook で起こす。
import fs from "node:fs";
import net from "node:net";
import path from "node:path";
import { createHash, createHmac, randomUUID, timingSafeEqual } from "node:crypto";
import Hyperswarm from "hyperswarm";
import DHT from "hyperdht";
import { execFile } from "node:child_process";
import { HOME_DIR, SOCK, OUTBOX, INBOX_DIR, MAILBOXES, AGENTS, CLI, loadConfig, lineReader, stableNode } from "./state.mjs";
import { claudeSessions, post } from "./claude.mjs";
import { wake } from "./wake.mjs";
import { frame, parseAddress, MAILBOX_NAME } from "./message.mjs";

const MAX_BODY = 16 * 1024;
const MAX_DEPTH = 10; // 人の入力なしにエージェント同士が続けて返事できる回数
const RATE_PER_MIN = 20; // 1 人の送り主から 1 分に受け取る数
const ACK_TIMEOUT = 10_000;

const log = (...a) => console.error(new Date().toISOString(), ...a);
const hex = (b) => Buffer.from(b).toString("hex");

export const topicFor = (teamKey) => createHash("sha256").update("nenwa/topic/").update(Buffer.from(teamKey, "hex")).digest();

// 送る側は (自分の鍵, 相手の鍵)、受ける側は (相手の鍵, 自分の鍵) で同じ値になる。チーム鍵を知らないと作れない。
export function helloMac(teamKey, fromPub, toPub) {
  return createHmac("sha256", Buffer.from(teamKey, "hex")).update("nenwa/hello/").update(fromPub).update(toPub).digest("hex");
}

export function verifyHello(teamKey, hello, remotePub, localPub) {
  if (typeof hello?.mac !== "string" || !/^[0-9a-f]{64}$/.test(hello.mac)) return false;
  const want = Buffer.from(helloMac(teamKey, remotePub, localPub), "hex");
  return timingSafeEqual(want, Buffer.from(hello.mac, "hex"));
}

export { frame, parseAddress };

const MAX_WAIT = 50; // check_inbox が待てる秒数（MCP の呼び出しが切れないうちに返す）
const WAKE_RETRY = 10 * 60_000; // 起こしてもまだ読まれていない間は、この時間がたつまで起こし直さない

export async function startDaemon() {
  const config = loadConfig();
  if (!config) throw new Error("config.json がありません。nenwa init か join を先に");
  const { person, machine, teamKey, seed } = config;
  const me = { person, machine };
  const ownAddress = (label) => `${person}/${label}@${machine}`;

  // ---- 状態 ----
  const peers = new Map(); // 相手の公開鍵 hex → { person, machine, conn, online, lastSeen, sessions }
  const pendingAcks = new Map(); // メッセージ id → resolve
  const depths = new Map(); // メッセージ id → 何往復目か（暴走よけ）
  const seen = new Map(); // 受け取ったメッセージ id → 時刻（再送の重複よけ）
  const rate = new Map(); // 送り主 → 直近 1 分の受信時刻
  let outbox = [];
  try {
    outbox = JSON.parse(fs.readFileSync(OUTBOX, "utf8"));
  } catch {}
  const saveOutbox = () => fs.writeFileSync(OUTBOX, JSON.stringify(outbox, null, 2), { mode: 0o600 });

  const remember = (map, key, value) => {
    map.set(key, value);
    if (map.size > 5000) map.delete(map.keys().next().value);
  };

  // ---- 受信箱（Claude Code 以外の相手。terminal はいつもある） ----
  // 受信箱を作れるのはこのパソコンの中からだけ（register / send --as / check_inbox）。外からは作れない。
  let mailboxes = {};
  try {
    mailboxes = JSON.parse(fs.readFileSync(MAILBOXES, "utf8"));
  } catch {}
  mailboxes.terminal ||= { created: new Date().toISOString() };
  const waiters = new Map(); // 受信箱 → check_inbox で待っている resolve の集まり
  const wakeState = new Map(); // 受信箱 → { at: 最後に起こした時刻 }
  const inboxFile = (name) => path.join(INBOX_DIR, `${name}.json`);
  const readInbox = (name) => {
    try {
      return JSON.parse(fs.readFileSync(inboxFile(name), "utf8"));
    } catch {
      return [];
    }
  };
  const writeInbox = (name, list) => {
    fs.mkdirSync(INBOX_DIR, { recursive: true, mode: 0o700 });
    fs.writeFileSync(inboxFile(name), JSON.stringify(list, null, 2), { mode: 0o600 });
  };

  function registerMailbox(name) {
    if (!MAILBOX_NAME.test(name || "")) throw new Error("受信箱の名前は英小文字・数字・. _ - で 40 文字まで（例: grok）");
    if (mailboxes[name]) return;
    mailboxes[name] = { created: new Date().toISOString() };
    fs.writeFileSync(MAILBOXES, JSON.stringify(mailboxes, null, 2), { mode: 0o600 });
    log("mailbox", name);
  }

  function storeInInbox(name, msg) {
    const list = readInbox(name);
    list.push(msg);
    writeInbox(name, list.slice(-500));
    for (const resolve of waiters.get(name) || []) resolve();
    waiters.delete(name);
    maybeWake(name, list);
  }

  function maybeWake(name, list) {
    const now = Date.now();
    const last = wakeState.get(name);
    if (last && !last.read && now - last.at < WAKE_RETRY) return;
    wakeState.set(name, { at: now, read: false });
    const address = ownAddress(name);
    const payload = {
      source: "nenwa",
      mailbox: address,
      unread: list.length,
      from: [...new Set(list.map((m) => m.from))],
      text:
        `nenwa の受信箱（${address}）に新しいメッセージが ${list.length} 件あります。` +
        `nenwa の check_inbox で読み、返事が要るものには send_message で reply_to を付けて返してください。` +
        `MCP が使えないときは、下の commands をこのパソコンのローカル実行で使ってください。`,
      commands: {
        read: `${stableNode()} ${CLI} inbox --as ${name}`,
        reply: `${stableNode()} ${CLI} send --as ${name} --reply-to <id> <宛先> "<返事>"`,
      },
      sent_at: new Date(now).toISOString(),
    };
    wake(name, payload)
      .then((status) => status !== null && log("wake", name, status))
      .catch((e) => log("wake failed", name, e.message));
  }

  async function takeInbox(name, wait, { register = true } = {}) {
    if (register) registerMailbox(name);
    let list = readInbox(name);
    const secs = Math.min(Math.max(Number(wait) || 0, 0), MAX_WAIT);
    if (!list.length && secs) {
      await new Promise((resolve) => {
        const set = waiters.get(name) || new Set();
        const done = () => {
          clearTimeout(timer);
          set.delete(done);
          resolve();
        };
        const timer = setTimeout(done, secs * 1000);
        set.add(done);
        waiters.set(name, set);
      });
      list = readInbox(name);
    }
    if (list.length) writeInbox(name, []);
    const w = wakeState.get(name);
    if (w) w.read = true;
    for (const m of list) remember(depths, m.id, m.depth || 0);
    return list;
  }

  // ---- フックでつながるエージェント（Codex、Cursor） ----
  // フックが呼ばれるたびにセッションを覚える。1 日見えなければ忘れる。
  let agents = {};
  try {
    agents = JSON.parse(fs.readFileSync(AGENTS, "utf8"));
  } catch {}
  const DAY = 24 * 3600_000;
  const saveAgents = () => fs.writeFileSync(AGENTS, JSON.stringify(agents, null, 2), { mode: 0o600 });
  const liveAgents = () => Object.values(agents).filter((a) => Date.now() - a.seen < DAY);
  const agentByLabel = (label) => liveAgents().find((a) => a.label === label);
  const slugOf = (s) => String(s).toLowerCase().replace(/[^a-z0-9._-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 40) || "session";

  function touchAgent(agent, input, status) {
    const id = String(input?.session_id || input?.conversation_id || "");
    if (!id) return null;
    const key = `${agent}:${id}`;
    const cwd = input.cwd || input.workspace_roots?.[0] || "";
    // Codex の ID は先頭が時刻なので、ばらつく末尾の 4 文字を使う。
    const tail = id.replace(/[^a-z0-9]/gi, "").slice(-4).toLowerCase();
    const a = (agents[key] ||= { agent, id, cwd, label: `${slugOf(path.basename(cwd))}-${tail}` });
    a.status = status;
    a.seen = Date.now();
    if (!a.cwd && cwd) a.cwd = cwd;
    for (const [k, v] of Object.entries(agents)) if (Date.now() - v.seen > DAY) delete agents[k];
    saveAgents();
    return a;
  }

  // MCP サーバーはどの会話から呼ばれたかを知らない。作業中（ツールを呼べるのは作業中）で、フォルダが合い、最近動いたものを選ぶ。
  function resolveAgent(agent, cwd) {
    const list = liveAgents().filter((a) => a.agent === agent && a.status !== "offline");
    const score = (a) => (a.status === "busy" ? 2 : 0) + (cwd && a.cwd === cwd ? 1 : 0);
    list.sort((x, y) => score(y) - score(x) || y.seen - x.seen);
    if (!list[0]) throw new Error(`この ${agent} のセッションがまだ nenwa に見えていません。nenwa install のあとに開いたセッションか、フックが信頼済みか（Codex は /hooks）を確かめてください`);
    return list[0];
  }

  const codexBin = process.env.NENWA_CODEX_BIN || "codex";
  async function deliverToAgent(a, msg) {
    if (a.agent === "codex") {
      // 待機中でも作業中でも、codex queue なら会話に入る（待機中ならそこで動き出す）。だめならフックで渡す。
      const ok = await new Promise((resolve) =>
        execFile(codexBin, ["queue", "--thread", a.id, "--message", frame(msg)], { timeout: 20_000 }, (err) => resolve(!err)),
      );
      if (ok) return "delivered";
    }
    storeInInbox(a.label, msg);
    return "inbox";
  }

  // フックの出力。届いているメッセージがあれば、そのエージェントのやり方で会話に差し込む。
  async function hookEvent({ agent, event, input }) {
    if (!["codex", "cursor"].includes(agent)) throw new Error(`unknown agent: ${agent}`);
    const status = { "session-start": "idle", stop: "idle", "session-end": "offline" }[event] || "busy";
    const a = touchAgent(agent, input, status);
    if (!a || event === "session-end") return null;
    // Cursor の beforeSubmitPrompt は会話に足せないので、メッセージは次の postToolUse か stop まで取っておく。
    const canInject = !(agent === "cursor" && event === "user-prompt-submit");
    const mail = canInject ? await takeInbox(a.label, 0, { register: false }) : [];
    const intro = event === "session-start" ? `nenwa: このセッションの宛先は ${ownAddress(a.label)} です。` : "";
    const text = [intro, ...mail.map(frame)].filter(Boolean).join("\n\n");
    if (mail.length) log("hook", agent, event, a.label, mail.length);
    if (event === "stop" && mail.length) a.status = "busy";
    if (!text) return agent === "cursor" && event === "user-prompt-submit" ? { continue: true } : null;
    if (agent === "codex") {
      if (event === "stop") return { decision: "block", reason: text };
      const hookEventName = { "session-start": "SessionStart", "user-prompt-submit": "UserPromptSubmit", "post-tool-use": "PostToolUse" }[event];
      return { hookSpecificOutput: { hookEventName, additionalContext: text } };
    }
    if (event === "stop") return { followup_message: text };
    return { additional_context: text };
  }

  // ---- 自分のセッション ----
  const presenceOf = () => [
    ...claudeSessions().map((s) => ({ address: ownAddress(s.label), title: s.name || "", status: s.status || "" })),
    ...liveAgents()
      .filter((a) => a.status !== "offline")
      .map((a) => ({ address: ownAddress(a.label), title: a.agent, status: a.status })),
    ...Object.keys(mailboxes)
      .filter((n) => n !== "terminal")
      .map((n) => ({ address: ownAddress(n), title: "受信箱", status: "inbox" })),
  ];
  let lastPresence = JSON.stringify(presenceOf());

  async function deliverLocal(msg) {
    const to = parseAddress(msg.to);
    const s = claudeSessions().find((x) => x.label === to.label);
    if (!s) {
      const a = agentByLabel(to.label);
      if (a) return deliverToAgent(a, msg);
      if (!mailboxes[to.label]) return "no_session";
      storeInInbox(to.label, msg);
      return "inbox";
    }
    try {
      await post(s.messagingSocketPath, frame(msg));
      return "delivered";
    } catch (e) {
      log("deliver failed", msg.to, e.message);
      return "no_session";
    }
  }

  // ---- P2P ----
  const opts = { keyPair: DHT.keyPair(Buffer.from(seed, "hex")) };
  if (process.env.NENWA_BOOTSTRAP) opts.bootstrap = JSON.parse(process.env.NENWA_BOOTSTRAP);
  // network: false（install が最初に作る形）のあいだは P2P につながない。このパソコンの中だけで動く。
  const network = config.network !== false;
  const swarm = network ? new Hyperswarm(opts) : null;

  const sendTo = (conn, obj) => conn.write(JSON.stringify(obj) + "\n");

  function peerFor(p, m) {
    for (const peer of peers.values()) if (peer.online && peer.person === p && peer.machine === m) return peer;
    return null;
  }

  function transmit(peer, msg) {
    return new Promise((resolve) => {
      const timer = setTimeout(() => {
        pendingAcks.delete(msg.id);
        resolve(null);
      }, ACK_TIMEOUT);
      pendingAcks.set(msg.id, (state) => {
        clearTimeout(timer);
        pendingAcks.delete(msg.id);
        resolve(state);
      });
      sendTo(peer.conn, { t: "msg", ...msg });
    });
  }

  async function flushOutbox(peer) {
    for (const item of [...outbox]) {
      const to = parseAddress(item.to);
      if (to.person !== peer.person || to.machine !== peer.machine) continue;
      const state = await transmit(peer, item);
      if (state === null) return; // 返事がなければ次につながったときにまた送る
      outbox = outbox.filter((x) => x.id !== item.id);
      saveOutbox();
      log("outbox", item.id, "→", item.to, state);
    }
  }

  function allowed(from) {
    const now = Date.now();
    const recent = (rate.get(from) || []).filter((t) => now - t < 60_000);
    recent.push(now);
    rate.set(from, recent);
    return recent.length <= RATE_PER_MIN;
  }

  async function onPeerMessage(peer, m) {
    if (m.t === "presence") {
      peer.sessions = Array.isArray(m.sessions) ? m.sessions.slice(0, 100) : [];
      return;
    }
    if (m.t === "ack") return pendingAcks.get(m.id)?.(m.state);
    if (m.t !== "msg") return;
    const ack = (state) => sendTo(peer.conn, { t: "ack", id: m.id, state });
    const from = parseAddress(m.from);
    const to = parseAddress(m.to);
    // パソコンは自分の人の名前でしか送れない。
    if (!from || !to || from.person !== peer.person || from.machine !== peer.machine) return ack("refused");
    if (to.person !== person || to.machine !== machine) return ack("refused");
    if (typeof m.body !== "string" || m.body.length > MAX_BODY) return ack("refused");
    if (seen.has(m.id)) return ack("delivered");
    if (!allowed(m.from)) return ack("rate_limited");
    const msg = { id: m.id, from: m.from, to: m.to, reply_to: m.reply_to || null, body: m.body, sent_at: m.sent_at, depth: Number(m.depth) || 0 };
    const state = await deliverLocal(msg);
    if (state === "delivered" || state === "inbox") {
      remember(seen, m.id, Date.now());
      remember(depths, m.id, msg.depth);
    }
    log("in", m.id, m.from, "→", m.to, state);
    ack(state);
  }

  swarm?.on("connection", (conn) => {
    const key = hex(conn.remotePublicKey);
    let peer = null;
    conn.setEncoding?.("utf8");
    conn.on("error", () => {});
    const timer = setTimeout(() => peer || conn.destroy(), 10_000);
    sendTo(conn, { t: "hello", person, machine, mac: helloMac(teamKey, conn.publicKey, conn.remotePublicKey) });
    const read = lineReader((line) => {
      let m;
      try {
        m = JSON.parse(line);
      } catch {
        return conn.destroy();
      }
      if (!peer) {
        if (m.t !== "hello" || !verifyHello(teamKey, m, conn.remotePublicKey, conn.publicKey)) {
          log("refused peer", key.slice(0, 8));
          return conn.destroy();
        }
        clearTimeout(timer);
        peer = peers.get(key) || { sessions: [] };
        Object.assign(peer, { person: String(m.person), machine: String(m.machine), conn, online: true, lastSeen: Date.now() });
        peers.set(key, peer);
        log("peer up", `${peer.person}@${peer.machine}`);
        sendTo(conn, { t: "presence", sessions: JSON.parse(lastPresence) });
        flushOutbox(peer);
        return;
      }
      peer.lastSeen = Date.now();
      onPeerMessage(peer, m).catch((e) => log("peer message", e.message));
    }, 256 * 1024);
    conn.on("data", (chunk) => {
      try {
        read(chunk.toString());
      } catch {
        conn.destroy();
      }
    });
    conn.on("close", () => {
      clearTimeout(timer);
      if (peer && peer.conn === conn) {
        peer.online = false;
        peer.lastSeen = Date.now();
        log("peer down", `${peer.person}@${peer.machine}`);
      }
    });
  });

  const discovery = swarm?.join(topicFor(teamKey), { server: true, client: true });
  // 同時に起動すると、お互いが探した時点では相手の告知がまだない。仲間が 1 人もいない間は探し直す。
  const refreshTimer = setInterval(() => {
    if (![...peers.values()].some((p) => p.online)) discovery?.refresh({ server: true, client: true }).catch(() => {});
  }, 10_000);

  // 自分のセッションが変わったら仲間に知らせる。
  const presenceTimer = setInterval(() => {
    const now = JSON.stringify(presenceOf());
    if (now === lastPresence) return;
    lastPresence = now;
    for (const peer of peers.values()) if (peer.online) sendTo(peer.conn, { t: "presence", sessions: JSON.parse(now) });
  }, 2000);

  // ---- 内側の API ----
  async function send({ fromSession, as, agent: agentKind, cwd, to, body, reply_to }) {
    const target = parseAddress(to);
    if (!target) throw new Error(`宛先は <人>/<セッション>@<マシン> の形で（例: alice/api@alice-mbp）。nenwa who で確認できます`);
    if (typeof body !== "string" || !body.trim()) throw new Error("本文が空です");
    if (body.length > MAX_BODY) throw new Error(`本文は ${MAX_BODY} 文字まで`);
    // 送り主: Codex / Cursor のセッション > 受信箱の名前（Grok Bot など）> Claude Code のセッション > ターミナル
    const hooked = agentKind ? resolveAgent(agentKind, cwd) : null;
    if (as && !hooked) registerMailbox(as);
    const own = !hooked && !as && fromSession && claudeSessions().find((s) => s.sessionId === fromSession);
    const from = hooked ? ownAddress(hooked.label) : as ? ownAddress(as) : own ? ownAddress(own.label) : ownAddress("terminal");
    const agent = hooked ? true : as ? as !== "terminal" : Boolean(own);
    // 人（ターミナル）から送ると数え直し。エージェントの返事は 1 つ増える。
    const depth = agent ? (reply_to && depths.has(reply_to) ? depths.get(reply_to) + 1 : 0) : 0;
    if (depth >= MAX_DEPTH) throw new Error(`人の入力なしのやり取りが ${MAX_DEPTH} 回続いたので止めました。続けるならユーザーに確認してください`);
    const msg = { id: randomUUID(), from, to: `${target.person}/${target.label}@${target.machine}`, reply_to: reply_to || null, body, sent_at: new Date().toISOString(), depth };
    remember(depths, msg.id, depth);
    if (target.person === person && target.machine === machine) {
      const state = await deliverLocal(msg);
      return { id: msg.id, from, state };
    }
    const peer = peerFor(target.person, target.machine);
    const state = peer ? await transmit(peer, msg) : null;
    if (state !== null) return { id: msg.id, from, state };
    outbox.push(msg);
    saveOutbox();
    return { id: msg.id, from, state: "queued", note: `${target.person}@${target.machine} がつながったら送ります` };
  }

  function who() {
    return {
      me,
      network,
      local: JSON.parse(lastPresence),
      peers: [...peers.values()].map((p) => ({
        person: p.person,
        machine: p.machine,
        online: p.online,
        lastSeen: new Date(p.lastSeen).toISOString(),
        sessions: p.online ? p.sessions : [],
      })),
      outbox: outbox.length,
    };
  }

  // 前のデーモンが残したソケットは、つながらなければ消す。
  if (fs.existsSync(SOCK)) {
    const busy = await new Promise((r) => {
      const c = net.createConnection(SOCK, () => (c.end(), r(true)));
      c.on("error", () => r(false));
    });
    if (busy) throw Object.assign(new Error("デーモンはもう動いています"), { code: "ALREADY_RUNNING" });
    fs.unlinkSync(SOCK);
  }
  fs.mkdirSync(HOME_DIR, { recursive: true, mode: 0o700 });
  const server = net.createServer((c) => {
    c.setEncoding("utf8");
    c.on("error", () => {});
    c.on("data", lineReader(async (line) => {
      let res;
      try {
        const req = JSON.parse(line);
        if (req.op === "who") res = { ok: true, result: who() };
        else if (req.op === "send") res = { ok: true, result: await send(req) };
        else if (req.op === "shutdown") {
          c.end(JSON.stringify({ ok: true, result: { stopping: true } }) + "\n");
          log("shutdown requested");
          return stop().then(() => process.exit(0));
        } else if (req.op === "register") res = { ok: true, result: (registerMailbox(req.name), { address: ownAddress(req.name) }) };
        else if (req.op === "inbox" && req.agent) {
          const a = resolveAgent(req.agent, req.cwd);
          res = { ok: true, result: { address: ownAddress(a.label), messages: await takeInbox(a.label, req.wait, { register: false }) } };
        } else if (req.op === "inbox") res = { ok: true, result: { address: ownAddress(req.name), messages: await takeInbox(req.name, req.wait) } };
        else if (req.op === "hook") res = { ok: true, result: await hookEvent(req) };
        else res = { ok: false, error: `unknown op: ${req.op}` };
      } catch (e) {
        res = { ok: false, error: e.message };
      }
      c.end(JSON.stringify(res) + "\n");
    }));
  });
  await new Promise((r) => server.listen(SOCK, r));
  fs.chmodSync(SOCK, 0o600);
  log("daemon up", `${person}@${machine}`, network ? `p2p key ${hex(opts.keyPair.publicKey).slice(0, 8)}` : "local only");

  const stop = async () => {
    clearInterval(presenceTimer);
    clearInterval(refreshTimer);
    server.close();
    try {
      fs.unlinkSync(SOCK);
    } catch {}
    await discovery?.destroy().catch(() => {});
    await swarm?.destroy();
  };
  for (const sig of ["SIGINT", "SIGTERM"]) process.once(sig, () => stop().then(() => process.exit(0)));
  return { stop, swarm };
}
