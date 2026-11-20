// The HTTP MCP endpoint for peers that cannot start a command on this computer (Muse and so on). Runs inside the daemon.
// - Takes POST /mcp in streamable HTTP form. The body is one JSON-RPC message, the reply is one application/json. SSE and session IDs are not used.
// - Listens on 127.0.0.1 only. From outside, only what comes through Tailscale Funnel arrives (muse.rs).
// - Authorization: Bearer <key> decides the inbox. The key itself is not kept; only its SHA-256 is written to http/keys.json.
// - The content is the same as the stdio MCP (respond in mcp.rs). The sender is fixed to that inbox and cannot use another name.
use anyhow::{Result, anyhow};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use http_body_util::{BodyExt, Full, LengthLimitError, Limited};
use hyper::body::{Bytes, Incoming};
use hyper::header::{ALLOW, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, HeaderValue, RETRY_AFTER, WWW_AUTHENTICATE};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response, StatusCode};
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, VecDeque};
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinSet;

use crate::daemon::log;
use crate::mcp::{Server, parse_error};
use crate::os;
use crate::state::{self, now_iso, now_ms, read_json};

pub const DEFAULT_PORT: u16 = 8787;
const MAX_BODY: usize = 64 * 1024;
const PER_KEY_PER_MIN: usize = 30; // requests accepted from one key per minute
const FAILS_PER_MIN: usize = 60; // stop when wrong-key requests exceed this per minute in total
const LOCKOUT_MS: u64 = 10 * 60_000;
const MAX_WAIT: u32 = 25; // check_inbox wait (returns before Funnel or the client's HTTP times out)
const MAX_CONNS: usize = 64;

pub fn dir() -> PathBuf {
    state::home().join("http")
}
fn keys_file() -> PathBuf {
    dir().join("keys.json")
}
fn enabled_file() -> PathBuf {
    dir().join("enabled")
}

// The record that it is open. When the daemon restarts, it reopens from this.
#[derive(Serialize, Deserialize, Clone)]
pub struct Enabled {
    pub port: u16,
    pub expires: Option<u64>, // close after this time (milliseconds). None means never close
    #[serde(default)]
    pub funnel: bool, // whether Tailscale Funnel was opened (closed together when closing)
}

impl Enabled {
    pub fn expired(&self, now: u64) -> bool {
        self.expires.is_some_and(|e| now >= e)
    }
}

pub fn load_enabled() -> Option<Enabled> {
    read_json(&enabled_file())
}

fn save_enabled(e: &Enabled) -> Result<()> {
    os::make_private_dir(&dir())?;
    let mut v = serde_json::to_value(e)?;
    if let Some(ms) = e.expires {
        v["expiresAt"] = json!(state::iso(ms));
    }
    os::write_private(&enabled_file(), &serde_json::to_string_pretty(&v)?)?;
    os::set_private(&enabled_file());
    Ok(())
}

// Delete the key and the open record (messages in the inbox are kept)
pub fn clear() {
    let _ = std::fs::remove_file(keys_file());
    let _ = std::fs::remove_file(enabled_file());
}

// ---- Keys ----
pub fn new_key() -> String {
    URL_SAFE_NO_PAD.encode(state::random_bytes::<32>())
}

fn hash_key(key: &str) -> [u8; 32] {
    let mut out = [0u8; 32];
    out.copy_from_slice(&Sha256::digest(key.as_bytes()));
    out
}

// One key per inbox. Making a new one disables the old key (when the daemon rereads).
pub fn save_key(mailbox: &str, key: &str) -> Result<()> {
    let mut keys: Map<String, Value> = read_json(&keys_file()).unwrap_or_default();
    keys.insert(mailbox.into(), json!({ "sha256": state::hex(&hash_key(key)), "created": now_iso() }));
    os::make_private_dir(&dir())?;
    os::write_private(&keys_file(), &serde_json::to_string_pretty(&keys)?)?;
    os::set_private(&keys_file());
    Ok(())
}

fn load_keys() -> Vec<([u8; 32], String)> {
    let keys: Map<String, Value> = read_json(&keys_file()).unwrap_or_default();
    keys.into_iter()
        .filter_map(|(name, v)| {
            let h: [u8; 32] = state::unhex(v["sha256"].as_str()?)?.try_into().ok()?;
            Some((h, name))
        })
        .collect()
}

// Compare to the end without leaving early
fn same(a: &[u8; 32], b: &[u8; 32]) -> bool {
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

// ---- Endpoint ----
#[derive(Default)]
struct Limits {
    fails: VecDeque<u64>,                      // times of wrong-key requests (last minute)
    locked_until: u64,                         // everything gets 503 until this time
    per_key: HashMap<[u8; 32], VecDeque<u64>>, // key → request times in the last minute
}

struct Gate {
    keys: Vec<([u8; 32], String)>,
    expires: Option<u64>,
    limits: Mutex<Limits>,
}

type Res = Response<Full<Bytes>>;

fn text(status: StatusCode, body: &str) -> Res {
    let mut r = Response::new(Full::new(Bytes::from(body.to_string())));
    *r.status_mut() = status;
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("text/plain; charset=utf-8"));
    r
}

fn json_res(status: StatusCode, v: &Value) -> Res {
    let mut r = Response::new(Full::new(Bytes::from(v.to_string())));
    *r.status_mut() = status;
    r.headers_mut().insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    r
}

// Even when refusing, read and discard a small body before replying.
// If the connection is closed with data unread, Windows sends the client a forced disconnect (10053), and the response may not arrive.
async fn reject(req: Request<Incoming>, res: Res) -> Res {
    let declared = req.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    if declared.is_none_or(|n| n <= MAX_BODY as u64) {
        let _ = tokio::time::timeout(Duration::from_secs(2), Limited::new(req.into_body(), MAX_BODY).collect()).await;
    }
    res
}

impl Gate {
    fn limits(&self) -> std::sync::MutexGuard<'_, Limits> {
        self.limits.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn locked(&self, now: u64) -> bool {
        now < self.limits().locked_until
    }

    fn failed(&self, now: u64) {
        let mut l = self.limits();
        l.fails.push_back(now);
        while l.fails.front().is_some_and(|t| now - t >= 60_000) {
            l.fails.pop_front();
        }
        if l.fails.len() > FAILS_PER_MIN {
            l.locked_until = now + LOCKOUT_MS;
            l.fails.clear();
            log!("http: too many wrong-key requests, stopping for 10 minutes");
        }
    }

    fn allowed(&self, key: &[u8; 32], now: u64) -> bool {
        let mut l = self.limits();
        let recent = l.per_key.entry(*key).or_default();
        while recent.front().is_some_and(|t| now - t >= 60_000) {
            recent.pop_front();
        }
        if recent.len() >= PER_KEY_PER_MIN {
            return false;
        }
        recent.push_back(now);
        true
    }

    // From the key to the inbox name. Compare against every key.
    fn mailbox(&self, req: &Request<Incoming>) -> Option<([u8; 32], String)> {
        let auth = req.headers().get(AUTHORIZATION)?.to_str().ok()?.trim();
        let (scheme, token) = auth.split_once(' ')?;
        if !scheme.eq_ignore_ascii_case("bearer") {
            return None;
        }
        let h = hash_key(token.trim());
        let mut found = None;
        for (k, name) in &self.keys {
            if same(k, &h) {
                found = Some((h, name.clone()));
            }
        }
        found
    }

    async fn handle(&self, req: Request<Incoming>) -> Res {
        let now = now_ms();
        if self.locked(now) {
            return reject(req, text(StatusCode::SERVICE_UNAVAILABLE, "temporarily unavailable\n")).await;
        }
        if req.uri().path() != "/mcp" {
            return reject(req, text(StatusCode::NOT_FOUND, "not found\n")).await;
        }
        if req.method() != Method::POST {
            let mut r = text(StatusCode::METHOD_NOT_ALLOWED, "method not allowed\n");
            r.headers_mut().insert(ALLOW, HeaderValue::from_static("POST"));
            return reject(req, r).await;
        }
        let who = self.mailbox(&req).filter(|_| !self.expires.is_some_and(|e| now >= e));
        let Some((key, name)) = who else {
            self.failed(now);
            let mut r = text(StatusCode::UNAUTHORIZED, "unauthorized\n");
            r.headers_mut().insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
            return reject(req, r).await;
        };
        if !self.allowed(&key, now) {
            let mut r = text(StatusCode::TOO_MANY_REQUESTS, "too many requests\n");
            r.headers_mut().insert(RETRY_AFTER, HeaderValue::from_static("60"));
            return reject(req, r).await;
        }
        let declared = req.headers().get(CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
        if declared.is_some_and(|n| n > MAX_BODY as u64) {
            return text(StatusCode::PAYLOAD_TOO_LARGE, "payload too large\n");
        }
        let body = match tokio::time::timeout(Duration::from_secs(15), Limited::new(req.into_body(), MAX_BODY).collect()).await {
            Ok(Ok(b)) => b.to_bytes(),
            Ok(Err(e)) if e.downcast_ref::<LengthLimitError>().is_some() => {
                return text(StatusCode::PAYLOAD_TOO_LARGE, "payload too large\n");
            }
            Ok(Err(_)) => return text(StatusCode::BAD_REQUEST, "bad request\n"),
            Err(_) => return text(StatusCode::REQUEST_TIMEOUT, "request timeout\n"),
        };
        let Ok(msg) = serde_json::from_slice::<Value>(&body) else {
            return json_res(StatusCode::BAD_REQUEST, &parse_error());
        };
        if !msg.is_object() {
            let err = json!({ "jsonrpc": "2.0", "id": null, "error": { "code": -32600, "message": "Invalid Request" } });
            return json_res(StatusCode::BAD_REQUEST, &err);
        }
        // Notifications, and replies to us (no method), are only accepted
        if msg.get("method").is_none() || msg.get("id").is_none() {
            let mut r = Response::new(Full::new(Bytes::new()));
            *r.status_mut() = StatusCode::ACCEPTED;
            return r;
        }
        // Inside, ask the daemon over the local socket (on another thread, because it may wait)
        let server = Server { as_: name, agent: String::new(), max_wait: MAX_WAIT };
        match tokio::task::spawn_blocking(move || server.respond(&msg)).await {
            Ok(Some(res)) => json_res(StatusCode::OK, &res),
            Ok(None) => {
                let mut r = Response::new(Full::new(Bytes::new()));
                *r.status_mut() = StatusCode::ACCEPTED;
                r
            }
            Err(_) => text(StatusCode::INTERNAL_SERVER_ERROR, "internal error\n"),
        }
    }
}

// The open endpoint. Stopping it closes the listener and every open connection.
pub struct Running {
    pub enabled: Enabled,
    task: tokio::task::JoinHandle<()>,
}

impl Running {
    pub async fn stop(self) {
        self.task.abort();
        let _ = self.task.await;
    }
}

pub async fn start(e: &Enabled) -> Result<Running> {
    let listener =
        TcpListener::bind(("127.0.0.1", e.port)).await.map_err(|err| anyhow!("Cannot listen on 127.0.0.1:{} ({err})", e.port))?;
    save_enabled(e)?;
    let gate = Arc::new(Gate { keys: load_keys(), expires: e.expires, limits: Mutex::new(Limits::default()) });
    let task = tokio::spawn(async move {
        // Connections are held in here. When this task stops, they all close.
        let mut conns = JoinSet::new();
        loop {
            tokio::select! {
                acc = listener.accept() => {
                    let Ok((stream, _)) = acc else {
                        tokio::time::sleep(Duration::from_millis(100)).await;
                        continue;
                    };
                    if conns.len() >= MAX_CONNS {
                        continue;
                    }
                    let g = gate.clone();
                    conns.spawn(async move {
                        let svc = service_fn(move |req| {
                            let g = g.clone();
                            async move { Ok::<_, Infallible>(g.handle(req).await) }
                        });
                        let _ = http1::Builder::new()
                            .timer(TokioTimer::new())
                            .header_read_timeout(Duration::from_secs(10))
                            .serve_connection(TokioIo::new(stream), svc)
                            .await;
                    });
                }
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    });
    Ok(Running { enabled: e.clone(), task })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn key_is_32_bytes_and_compared_by_hash() {
        let k = new_key();
        assert_eq!(URL_SAFE_NO_PAD.decode(&k).unwrap().len(), 32);
        assert_ne!(k, new_key());
        assert!(same(&hash_key(&k), &hash_key(&k)));
        assert!(!same(&hash_key(&k), &hash_key("x")));
    }
}
