// 受信箱にメッセージが入ったとき、その持ち主（Grok Bot の routine など）を webhook で起こす。
// 設定は ~/.nenwa/wake/<受信箱>.json に { url, key }。
// 鍵が whsec_ で始まれば Standard Webhooks の署名、それ以外は Authorization: Bearer と X-Automation-Key で送る。
use anyhow::{Result, bail};
use base64::Engine;
use base64::engine::DecodePaddingMode;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use hmac::{Hmac, KeyInit, Mac};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::Sha256;
use std::path::PathBuf;
use std::time::Duration;

use crate::message::is_mailbox_name;
use crate::os;
use crate::state::{self, ask, now_iso, now_ms, uuid};

#[derive(Serialize, Deserialize)]
pub struct WakeConfig {
    pub url: String,
    pub key: String,
}

fn file(name: &str) -> PathBuf {
    state::wake_dir().join(format!("{name}.json"))
}

pub fn load_wake(name: &str) -> Option<WakeConfig> {
    let c: WakeConfig = state::read_json(&file(name))?;
    (!c.url.is_empty() && !c.key.is_empty()).then_some(c)
}

fn is_http(s: &str) -> bool {
    s.starts_with("http://") || s.starts_with("https://")
}

pub fn save_wake(name: &str, url: &str, key: &str) -> Result<()> {
    if !is_http(url) {
        bail!("webhook の URL が正しくありません");
    }
    if key.is_empty() {
        bail!("鍵が空です");
    }
    if is_http(key) || key == url {
        bail!("入れたのは鍵ではなく URL のようです。sender key を入れ直してください");
    }
    os::make_private_dir(&state::wake_dir())?;
    let text = serde_json::to_string_pretty(&WakeConfig { url: url.into(), key: key.into() })? + "\n";
    os::write_private(&file(name), &text)?;
    Ok(())
}

// Node の Buffer.from(s, "base64") と同じく、末尾の = の有無は問わない
const LENIENT: GeneralPurpose =
    GeneralPurpose::new(&base64::alphabet::STANDARD, GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent));

fn sign(secret: &str, id: &str, ts: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&LENIENT.decode(secret).unwrap_or_default()).expect("HMAC は鍵の長さを選ばない");
    mac.update(format!("{id}.{ts}.{body}").as_bytes());
    format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
}

pub fn wake_headers(key: &str, body: &str) -> Vec<(String, String)> {
    let mut h = vec![("content-type".to_string(), "application/json".to_string())];
    if let Some(secret) = key.strip_prefix("whsec_") {
        let id = format!("msg_{}", uuid().replace('-', ""));
        let ts = (now_ms() / 1000).to_string();
        let sig = sign(secret, &id, &ts, body);
        h.push(("webhook-id".into(), id));
        h.push(("webhook-timestamp".into(), ts));
        h.push(("webhook-signature".into(), sig));
    } else {
        h.push(("authorization".into(), format!("Bearer {key}")));
        h.push(("x-automation-key".into(), key.into()));
    }
    h
}

// 設定がなければ None。あれば HTTP の状態コード。止まって待つので、デーモンからは別のスレッドで呼ぶ。
pub fn wake(name: &str, payload: &Value) -> Result<Option<u16>> {
    let Some(c) = load_wake(name) else { return Ok(None) };
    let body = payload.to_string();
    let agent: ureq::Agent =
        ureq::Agent::config_builder().timeout_global(Some(Duration::from_secs(15))).http_status_as_error(false).build().into();
    let mut req = agent.post(&c.url);
    for (k, v) in wake_headers(&c.key, &body) {
        req = req.header(k, v);
    }
    Ok(Some(req.send(&body)?.status().as_u16()))
}

// URL のホスト（ポートつき）。new URL(url).host と同じ
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.rsplit_once('@').map_or(host, |x| x.1)
}

// 鍵は画面にも会話にも出さない。画面に出さない入力か、NENWA_WAKE_KEY か、既存の { url, key } の JSON から読む。
pub fn wake_command(args: &[String]) -> Result<()> {
    let name = args.first().cloned().unwrap_or_default();
    if !is_mailbox_name(&name) {
        bail!("受信箱の名前を指定してください（例: nenwa wake grok <URL>）");
    }
    let args = &args[1..];
    if args.first().map(String::as_str) == Some("--test") {
        if load_wake(&name).is_none() {
            bail!("{name} の webhook はまだ設定されていません");
        }
        let payload = json!({ "source": "nenwa", "mailbox": name, "test": true, "text": "nenwa からの試しの呼び出しです。何もしなくて大丈夫です。", "sent_at": now_iso() });
        let status = wake(&name, &payload)?.unwrap_or(0);
        println!("webhook の応答: HTTP {status}");
        return Ok(());
    }
    let (url, key) = if args.first().map(String::as_str) == Some("--import") {
        let p = args.get(1).cloned().unwrap_or_default();
        let p = match p.strip_prefix("~/") {
            Some(rest) => os::home_dir().join(rest),
            None => PathBuf::from(p),
        };
        let c: WakeConfig = serde_json::from_str(&std::fs::read_to_string(p)?)?;
        (c.url, c.key)
    } else {
        let url = args.first().cloned().unwrap_or_default();
        let key = match std::env::var("NENWA_WAKE_KEY") {
            Ok(k) if !k.is_empty() => k,
            _ => rpassword::prompt_password("sender key を貼り付けて Enter（画面には出ません）: ")?.trim().to_string(),
        };
        (url, key)
    };
    save_wake(&name, &url, &key)?;
    ask(json!({ "op": "register", "name": name }))?;
    println!("{name} の受信箱に届いたら {} の webhook を叩きます（鍵は ~/.nenwa/wake/{name}.json、権限 600）", host_of(&url));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 署名の形() {
        // Standard Webhooks の仕様にある例
        let sig = sign("MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw", "msg_p5jXN8AQM9LWM0D4loKWxJek", "1614265330", r#"{"test": 2432232314}"#);
        assert_eq!(sig, "v1,g0hM9SsE+OTPJTGt/tmIKtSyZlE3uFJELVlNIOLJ1OE=");
        let h = wake_headers("whsec_MfKQ9r8GKYqrTwjUPD8ILPZIo2LaLaSw", "{}");
        let get = |k: &str| h.iter().find(|x| x.0 == k).map(|x| x.1.clone()).unwrap();
        assert!(get("webhook-id").starts_with("msg_"));
        assert!(get("webhook-signature").starts_with("v1,"));
        let b = wake_headers("abc", "{}");
        assert!(b.contains(&("authorization".into(), "Bearer abc".into())) && b.contains(&("x-automation-key".into(), "abc".into())));
        assert_eq!(host_of("https://u@example.com:8443/hook?x"), "example.com:8443");
    }
}
