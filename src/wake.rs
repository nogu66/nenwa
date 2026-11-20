// When a message enters an inbox, wake its owner (a Grok Bot routine, for example) through a webhook.
// The config is { url, key } in ~/.nenwa/wake/<inbox>.json.
// If the key starts with whsec_, send a Standard Webhooks signature; otherwise send Authorization: Bearer and X-Automation-Key.
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
        bail!("The webhook URL is not valid");
    }
    if key.is_empty() {
        bail!("The key is empty");
    }
    if is_http(key) || key == url {
        bail!("That looks like a URL, not a key. Enter the sender key again");
    }
    os::make_private_dir(&state::wake_dir())?;
    let text = serde_json::to_string_pretty(&WakeConfig { url: url.into(), key: key.into() })? + "\n";
    os::write_private(&file(name), &text)?;
    Ok(())
}

// Like Node's Buffer.from(s, "base64"), accept it with or without trailing =
const LENIENT: GeneralPurpose =
    GeneralPurpose::new(&base64::alphabet::STANDARD, GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent));

fn sign(secret: &str, id: &str, ts: &str, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&LENIENT.decode(secret).unwrap_or_default()).expect("HMAC accepts a key of any length");
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

// None if there is no config. Otherwise the HTTP status code. It blocks, so the daemon calls it on another thread.
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

// The host of the URL (with port). Same as new URL(url).host
fn host_of(url: &str) -> &str {
    let rest = url.split_once("://").map_or(url, |x| x.1);
    let host = rest.split(['/', '?', '#']).next().unwrap_or("");
    host.rsplit_once('@').map_or(host, |x| x.1)
}

// The key is never shown on screen or in a conversation. It is read from hidden input, NENWA_WAKE_KEY, or an existing { url, key } JSON.
pub fn wake_command(args: &[String]) -> Result<()> {
    let name = args.first().cloned().unwrap_or_default();
    if !is_mailbox_name(&name) {
        bail!("Give an inbox name (for example: nenwa wake grok <URL>)");
    }
    let args = &args[1..];
    if args.first().map(String::as_str) == Some("--test") {
        if load_wake(&name).is_none() {
            bail!("No webhook is set for {name} yet");
        }
        let payload = json!({ "source": "nenwa", "mailbox": name, "test": true, "text": "This is a test call from nenwa. You do not need to do anything.", "sent_at": now_iso() });
        let status = wake(&name, &payload)?.unwrap_or(0);
        println!("Webhook response: HTTP {status}");
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
            _ => rpassword::prompt_password("Paste the sender key and press Enter (it is not shown): ")?.trim().to_string(),
        };
        (url, key)
    };
    save_wake(&name, &url, &key)?;
    ask(json!({ "op": "register", "name": name }))?;
    println!(
        "When a message reaches the {name} inbox, nenwa calls the webhook at {} (the key is in ~/.nenwa/wake/{name}.json, mode 600)",
        host_of(&url)
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn signature_format() {
        // The example in the Standard Webhooks spec
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
