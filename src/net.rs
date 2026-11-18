// P2P（iroh）。
// - 身元: パソコンごとに iroh の Endpoint を 1 つ持つ。秘密鍵は config.json の seed から作るので、身元は変わらない。
// - 招待: チーム鍵と、招いた側の住所（公開鍵、relay、LAN の住所）を合わせて base64url にしたもの。
// - つなぎ方: つないだら HMAC の hello で合言葉を確かめ、互いの知っている仲間の住所を交換して、知らない仲間にもつなぐ。
//   仲間の一覧は peers.json に残し、起動のたびにつなぎ直す。
// - 回線の中身: 1 行 1 JSON（hello、presence、msg、ack、peers）を、1 本の双方向ストリームに流す。
// - 相手探し: iroh の標準（n0 の DNS と relay、ホールパンチ）。NENWA_RELAY=disabled なら relay も DNS も使わず、
//   招待と peers.json にある LAN の住所だけでつなぐ（テスト用）。
use anyhow::Result;
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hmac::{Hmac, KeyInit, Mac};
use iroh::endpoint::{Connection, presets};
use iroh::{Endpoint, EndpointAddr, PublicKey, RelayMode, RelayUrl, SecretKey, TransportAddr};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::collections::BTreeMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::mpsc;

use crate::daemon::{Daemon, Peer, log, remember_peer};
use crate::state::{self, hex, now_ms, read_json, unhex, write_json_private};

pub const ALPN: &[u8] = b"nenwa/1";
const MAX_LINE: u64 = 256 * 1024;

// 相手の住所
#[derive(Serialize, Deserialize, Clone, Default, Debug, PartialEq)]
pub struct PeerAddr {
    pub id: String,
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub addrs: Vec<String>,
}

// peers.json の 1 件（鍵 hex → これ）
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct Known {
    #[serde(default)]
    pub person: String,
    #[serde(default)]
    pub machine: String,
    #[serde(default)]
    pub relay: Option<String>,
    #[serde(default)]
    pub addrs: Vec<String>,
    #[serde(default)]
    pub seen: String,
}
pub type Peers = BTreeMap<String, Known>;

pub fn load_peers() -> Peers {
    read_json(&state::peers_file()).unwrap_or_default()
}
pub fn save_peers(p: &Peers) {
    write_json_private(&state::peers_file(), p);
}

fn known_addr(id: &str, k: &Known) -> PeerAddr {
    PeerAddr { id: id.into(), relay: k.relay.clone(), addrs: k.addrs.clone() }
}

fn relay_disabled() -> bool {
    std::env::var("NENWA_RELAY").is_ok_and(|v| v == "disabled")
}

pub async fn bind(seed: [u8; 32]) -> Result<Endpoint> {
    let builder =
        if relay_disabled() { Endpoint::builder(presets::Minimal).relay_mode(RelayMode::Disabled) } else { Endpoint::builder(presets::N0) };
    Ok(builder.secret_key(SecretKey::from_bytes(&seed)).alpns(vec![ALPN.to_vec()]).bind().await?)
}

fn to_peer_addr(a: &EndpointAddr) -> PeerAddr {
    PeerAddr {
        id: hex(a.id.as_bytes()),
        relay: a.relay_urls().next().map(|u| u.to_string()),
        addrs: a.ip_addrs().map(|s| s.to_string()).collect(),
    }
}

fn to_endpoint_addr(p: &PeerAddr) -> Option<EndpointAddr> {
    let id: [u8; 32] = unhex(&p.id)?.try_into().ok()?;
    let mut addrs: Vec<TransportAddr> = p.addrs.iter().filter_map(|a| a.parse().ok()).map(TransportAddr::Ip).collect();
    if let Some(r) = p.relay.as_ref().and_then(|r| r.parse::<RelayUrl>().ok()) {
        addrs.push(TransportAddr::Relay(r));
    }
    Some(EndpointAddr::from_parts(PublicKey::from_bytes(&id).ok()?, addrs))
}

// 招待に入れる自分の住所。relay を使うときは relay につながるまで少し待つ。
pub async fn my_addr(ep: &Endpoint) -> PeerAddr {
    if !relay_disabled() {
        let _ = tokio::time::timeout(Duration::from_secs(5), ep.online()).await;
    }
    for _ in 0..30 {
        let a = ep.addr();
        if a.ip_addrs().next().is_some() {
            return to_peer_addr(&a);
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    to_peer_addr(&ep.addr())
}

// ---- 招待の文字列 ----
// [チーム鍵 32][公開鍵 32][relay の長さ 1][relay][住所の数 1][住所: 4 か 6、IP、ポート 2]...
pub fn encode_invite(team_key: &[u8; 32], a: &PeerAddr) -> String {
    let mut b = team_key.to_vec();
    b.extend(unhex(&a.id).unwrap_or_default());
    let relay = a.relay.clone().unwrap_or_default();
    let relay = if relay.len() < 256 { relay } else { String::new() };
    b.push(relay.len() as u8);
    b.extend(relay.as_bytes());
    let addrs: Vec<SocketAddr> = a.addrs.iter().filter_map(|x| x.parse().ok()).take(255).collect();
    b.push(addrs.len() as u8);
    for s in addrs {
        match s.ip() {
            IpAddr::V4(ip) => {
                b.push(4);
                b.extend(ip.octets());
            }
            IpAddr::V6(ip) => {
                b.push(6);
                b.extend(ip.octets());
            }
        }
        b.extend(s.port().to_be_bytes());
    }
    URL_SAFE_NO_PAD.encode(b)
}

pub fn decode_invite(code: &str) -> Option<([u8; 32], PeerAddr)> {
    let b = URL_SAFE_NO_PAD.decode(code.trim()).ok()?;
    let mut i = 0;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = b.get(i..i + n)?;
        i += n;
        Some(s)
    };
    let key: [u8; 32] = take(32)?.try_into().ok()?;
    let id = hex(take(32)?);
    let n = take(1)?[0] as usize;
    let relay = String::from_utf8(take(n)?.to_vec()).ok().filter(|r| !r.is_empty());
    let count = take(1)?[0];
    let mut addrs = vec![];
    for _ in 0..count {
        let ip: IpAddr = match take(1)?[0] {
            4 => <[u8; 4]>::try_from(take(4)?).ok()?.into(),
            6 => <[u8; 16]>::try_from(take(16)?).ok()?.into(),
            _ => return None,
        };
        let port = u16::from_be_bytes(take(2)?.try_into().ok()?);
        addrs.push(SocketAddr::new(ip, port).to_string());
    }
    Some((key, PeerAddr { id, relay, addrs }))
}

// ---- 合言葉 ----
// 送る側は (自分の鍵, 相手の鍵)、受ける側は (相手の鍵, 自分の鍵) で同じ値になる。チーム鍵を知らないと作れない。
fn hello_hmac(team_key: &[u8], from: &[u8], to: &[u8]) -> Hmac<Sha256> {
    let mut mac = Hmac::<Sha256>::new_from_slice(team_key).expect("HMAC は鍵の長さを選ばない");
    mac.update(b"nenwa/hello/");
    mac.update(from);
    mac.update(to);
    mac
}

pub fn hello_mac(team_key: &[u8], from: &[u8], to: &[u8]) -> String {
    hex(&hello_hmac(team_key, from, to).finalize().into_bytes())
}

pub fn verify_hello(team_key: &[u8], mac: &Value, remote: &[u8], local: &[u8]) -> bool {
    let Some(m) = mac.as_str().filter(|m| m.len() == 64 && m.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))) else {
        return false;
    };
    hello_hmac(team_key, remote, local).verify_slice(&unhex(m).unwrap_or_default()).is_ok()
}

// ---- つなぐ ----
pub fn start(d: &Arc<Daemon>) {
    let Some(ep) = d.ep.get().cloned() else { return };
    let dd = d.clone();
    tokio::spawn(async move {
        while let Some(incoming) = ep.accept().await {
            let d = dd.clone();
            tokio::spawn(async move {
                if let Ok(conn) = incoming.await {
                    run_conn(d, conn, false).await;
                }
            });
        }
    });
    // peers.json の仲間に、起動したときと、その後も 10 秒ごとにつなぎ直す。
    let dd = d.clone();
    tokio::spawn(async move {
        loop {
            let list: Vec<PeerAddr> = dd.lock().known.iter().map(|(id, k)| known_addr(id, k)).collect();
            for a in list {
                dial(&dd, a);
            }
            tokio::time::sleep(Duration::from_secs(10)).await;
        }
    });
}

fn dial(d: &Arc<Daemon>, a: PeerAddr) {
    let Some(ep) = d.ep.get().cloned() else { return };
    if a.id == hex(ep.id().as_bytes()) {
        return;
    }
    {
        let mut st = d.lock();
        if st.peers.iter().any(|p| p.id == a.id && p.online) || !st.dialing.insert(a.id.clone()) {
            return;
        }
    }
    let d = d.clone();
    tokio::spawn(async move {
        let conn = match to_endpoint_addr(&a) {
            Some(addr) => tokio::time::timeout(Duration::from_secs(15), ep.connect(addr, ALPN)).await.ok().and_then(|r| r.ok()),
            None => None,
        };
        match conn {
            Some(conn) => run_conn(d, conn, true).await,
            None => {
                d.lock().dialing.remove(&a.id);
            }
        }
    });
}

// 仲間から教わった住所のうち、まだつながっていない相手にかける
pub fn on_peers(d: &Arc<Daemon>, list: &Value) {
    for a in list.as_array().into_iter().flatten().take(100) {
        if let Ok(a) = serde_json::from_value::<PeerAddr>(a.clone()) {
            dial(d, a);
        }
    }
}

async fn run_conn(d: Arc<Daemon>, conn: Connection, dialer: bool) {
    let remote = hex(conn.remote_id().as_bytes());
    serve_conn(&d, &conn, &remote, dialer).await;
    d.lock().dialing.remove(&remote);
}

async fn serve_conn(d: &Arc<Daemon>, conn: &Connection, remote: &str, dialer: bool) {
    let Some(ep) = d.ep.get() else { return };
    let (local_id, remote_id) = (*ep.id().as_bytes(), *conn.remote_id().as_bytes());
    let local = hex(&local_id);
    // かけた側がストリームを開き、先に hello を書く（書くまで相手にはストリームが見えない）
    let streams = if dialer {
        tokio::time::timeout(Duration::from_secs(10), conn.open_bi()).await
    } else {
        tokio::time::timeout(Duration::from_secs(10), conn.accept_bi()).await
    };
    let Ok(Ok((mut send, recv))) = streams else {
        conn.close(0u32.into(), b"no stream");
        return;
    };
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(line) = rx.recv().await {
            if send.write_all(line.as_bytes()).await.is_err() {
                break;
            }
        }
        let _ = send.finish();
    });
    let hello = json!({ "t": "hello", "person": d.person, "machine": d.machine, "mac": hello_mac(&d.team_key, &local_id, &remote_id), "addr": to_peer_addr(&ep.addr()) });
    let _ = tx.send(hello.to_string() + "\n");

    let mut r = BufReader::new(recv);
    let first = tokio::time::timeout(Duration::from_secs(10), read_line(&mut r)).await.ok().flatten();
    let Some(h) = first.filter(|h| h["t"] == "hello" && verify_hello(&d.team_key, &h["mac"], &remote_id, &local_id)) else {
        log!("refused peer {}", &remote[..8]);
        conn.close(0u32.into(), b"refused");
        return;
    };
    let (person, machine) = (state_str(&h["person"]), state_str(&h["machine"]));
    let serial = {
        let mut st = d.lock();
        st.next_conn += 1;
        let serial = st.next_conn;
        let who_dialed = if dialer { local.clone() } else { remote.to_string() };
        if let Some(old) = st.peers.iter().find(|p| p.id == remote && p.online) {
            if keep_old(&old.dialer, &who_dialed) {
                drop(st);
                conn.close(0u32.into(), b"duplicate");
                return;
            }
            if let Some(h) = &old.handle {
                h.close(0u32.into(), b"replaced");
            }
        }
        let i = match st.peers.iter().position(|p| p.id == remote) {
            Some(i) => i,
            None => {
                st.peers.push(Peer {
                    id: remote.into(),
                    person: String::new(),
                    machine: String::new(),
                    online: false,
                    last_seen: 0,
                    sessions: vec![],
                    tx: None,
                    conn: 0,
                    dialer: String::new(),
                    handle: None,
                });
                st.peers.len() - 1
            }
        };
        let p = &mut st.peers[i];
        (p.person, p.machine, p.online, p.last_seen, p.tx, p.conn, p.dialer, p.handle) =
            (person.clone(), machine.clone(), true, now_ms(), Some(tx.clone()), serial, who_dialed, Some(conn.clone()));
        remember_peer(&mut st, remote, &person, &machine, serde_json::from_value(h["addr"].clone()).ok());
        st.dialing.remove(remote);
        let presence = json!({ "t": "presence", "sessions": st.last_presence });
        let _ = tx.send(presence.to_string() + "\n");
        let others: Vec<PeerAddr> =
            st.known.iter().filter(|(id, _)| id.as_str() != remote && **id != local).map(|(id, k)| known_addr(id, k)).collect();
        let _ = tx.send(json!({ "t": "peers", "peers": others }).to_string() + "\n");
        serial
    };
    log!("peer up {person}@{machine}");
    let (dd, id) = (d.clone(), remote.to_string());
    tokio::spawn(async move { dd.flush_outbox(&id).await });

    loop {
        tokio::select! {
            line = read_line(&mut r) => {
                let Some(m) = line else { break };
                if let Some(p) = d.lock().peers.iter_mut().find(|p| p.id == remote) {
                    p.last_seen = now_ms();
                }
                d.on_peer_line(remote, m);
            }
            _ = conn.closed() => break,
        }
    }
    conn.close(0u32.into(), b"bye");
    let mut st = d.lock();
    if let Some(p) = st.peers.iter_mut().find(|p| p.id == remote && p.conn == serial) {
        p.online = false;
        p.last_seen = now_ms();
        p.tx = None;
        p.handle = None;
        log!("peer down {}@{}", p.person, p.machine);
    }
}

// 同じ相手とのつながりが 2 本できたとき、古いほうを残すか。
// 両側から同時にかけたときは、鍵の小さい側がかけたつながりを両側で残す。同じ側がかけ直したなら新しいほう。
fn keep_old(old_dialer: &str, new_dialer: &str) -> bool {
    old_dialer != new_dialer && old_dialer < new_dialer
}

fn state_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "undefined".into(),
        other => other.to_string(),
    }
}

// 1 行読んで JSON にする。切れた、長すぎる、JSON でないときは None（つながりを切る）
async fn read_line<R: tokio::io::AsyncRead + Unpin>(r: &mut BufReader<R>) -> Option<Value> {
    let mut line = String::new();
    loop {
        line.clear();
        let n = (&mut *r).take(MAX_LINE).read_line(&mut line).await.ok()?;
        if n == 0 || (!line.ends_with('\n') && n as u64 >= MAX_LINE) {
            return None;
        }
        if !line.trim().is_empty() {
            return serde_json::from_str(line.trim()).ok();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 合言葉hmacの確認() {
        let team: [u8; 32] = state::random_bytes();
        let (a, b): ([u8; 32], [u8; 32]) = (state::random_bytes(), state::random_bytes());
        let good = json!(hello_mac(&team, &a, &b));
        assert!(verify_hello(&team, &good, &a, &b));
        assert!(!verify_hello(&state::random_bytes::<32>(), &good, &a, &b), "別のチーム鍵");
        assert!(!verify_hello(&team, &good, &b, &a), "向きが逆");
        assert!(!verify_hello(&team, &json!("zz"), &a, &b));
    }

    #[test]
    fn 同時にかけたときに残すつながり() {
        // a < b。両側で同じ答え（a がかけたほう）になる
        assert!(keep_old("a", "b"), "a がかけたのを持っていて b のが来た: 古いのを残す");
        assert!(!keep_old("b", "a"), "b がかけたのを持っていて a のが来た: 新しいのにする");
        assert!(!keep_old("a", "a"), "同じ側のかけ直しは新しいほう");
    }

    #[test]
    fn 招待の文字列() {
        let key: [u8; 32] = state::random_bytes();
        let a = PeerAddr {
            id: hex(&state::random_bytes::<32>()),
            relay: Some("https://relay.example/".into()),
            addrs: vec!["192.168.1.2:4433".into(), "[fe80::1]:9".into()],
        };
        let code = encode_invite(&key, &a);
        assert!(code.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'-' || c == b'_'));
        assert_eq!(decode_invite(&code), Some((key, a)));
        assert_eq!(decode_invite("abc"), None);
    }
}
