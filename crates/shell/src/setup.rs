//! The graphical setup assistant's backend.
//!
//! The page (`ui/index.html`, "setup assistant") asks for a handful of fixed
//! operations; each one runs `agentos-setup --<op>` through `sudo -n` (the
//! image's desktop user may use sudo without a password), with the answers
//! on stdin as `KEY=VALUE` lines. Everything the page sends is checked here
//! first: only known fields, known choices, URLs that look like URLs, and no
//! control characters, so a value can never start a new line on stdin or in
//! `/etc/agentos/agentos.env`.
//!
//! Requests: `{"type":"setup","id":N,"op":OP,…}`, answered with
//! `{"type":"setup","id":N,"ok":BOOL,"data":…,"error":"…"}`.
//!
//! | op | does |
//! | --- | --- |
//! | `status` | current settings (never secrets, only whether they are set), network, PIN, install state |
//! | `wifi-scan` | nearby Wi-Fi networks |
//! | `wifi-connect` | join one (`ssid`, `password`) |
//! | `test-ha` | try a Home Assistant URL and token (`hass_url`, `hass_token`) |
//! | `apply` | write the settings, set or clear the PIN, mark setup done |
//! | `restart` | restart agentosd so it reads the new settings |

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{json, Map, Value};

/// `AGENTOS_SETUP`, else `/usr/local/bin/agentos-setup` if it exists.
pub fn helper() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AGENTOS_SETUP") {
        return (!p.is_empty()).then(|| p.into());
    }
    let p = PathBuf::from("/usr/local/bin/agentos-setup");
    p.exists().then_some(p)
}

/// A checked request: the helper's option and its stdin.
#[derive(Debug, PartialEq)]
pub enum Job {
    Helper { op: &'static str, input: String },
    Restart,
}

/// The longest value accepted (API keys and HA tokens are a few hundred bytes).
const MAX_LEN: usize = 4096;

fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    let s = v.get(key).and_then(Value::as_str).unwrap_or("").trim();
    if s.len() > MAX_LEN {
        return Err(format!("{key} 太长"));
    }
    if s.chars().any(char::is_control) {
        return Err(format!("{key} 含有控制字符"));
    }
    Ok(s)
}

fn url<'a>(v: &'a Value, key: &str) -> Result<&'a str, String> {
    let s = text(v, key)?;
    let ok = s.is_empty()
        || ((s.starts_with("http://") || s.starts_with("https://"))
            && !s.contains(char::is_whitespace));
    if ok {
        Ok(s.trim_end_matches('/'))
    } else {
        Err("地址要以 http:// 或 https:// 开头".into())
    }
}

fn choice(v: &Value, key: &str, allowed: &[&'static str]) -> Result<&'static str, String> {
    let s = v.get(key).and_then(Value::as_str).unwrap_or("");
    allowed
        .iter()
        .copied()
        .find(|a| *a == s)
        .ok_or_else(|| format!("{key} 无效"))
}

fn flag(v: &Value, key: &str) -> bool {
    v.get(key).and_then(Value::as_bool).unwrap_or(false)
}

/// Append `NAME=value`, or `KEEP=NAME` when the page left a saved secret as is.
fn secret(out: &mut String, v: &Value, field: &str, name: &str) -> Result<(), String> {
    let s = text(v, field)?;
    if s.is_empty() && flag(v, &format!("keep_{field}")) {
        out.push_str(&format!("KEEP={name}\n"));
    } else {
        out.push_str(&format!("{name}={s}\n"));
    }
    Ok(())
}

/// The env lines for `apply`.
fn apply_input(v: &Value) -> Result<String, String> {
    let mut out = String::new();
    let planner = choice(v, "planner", &["claude", "openai", "rule"])?;
    out.push_str(&format!("AGENTOS_PLANNER={planner}\n"));
    match planner {
        "claude" => {
            secret(&mut out, v, "anthropic_key", "ANTHROPIC_API_KEY")?;
            let model = text(v, "model")?;
            if !model.is_empty() {
                out.push_str(&format!("AGENTOS_MODEL={model}\n"));
            }
        }
        "openai" => {
            let base = url(v, "openai_base")?;
            let model = text(v, "openai_model")?;
            if base.is_empty() || model.is_empty() {
                return Err("OpenAI 兼容接口需要地址和模型名".into());
            }
            out.push_str(&format!("OPENAI_BASE_URL={base}\n"));
            secret(&mut out, v, "openai_key", "OPENAI_API_KEY")?;
            out.push_str(&format!("OPENAI_MODEL={model}\n"));
        }
        _ => {}
    }
    if choice(v, "voice", &["openai", "off"])? == "openai" {
        let speak = choice(v, "speak", &["auto", "always", "off"])?;
        out.push_str(&format!(
            "AGENTOS_STT=openai\nAGENTOS_TTS=openai\nAGENTOS_SPEAK={speak}\n"
        ));
        // Empty base and key fall back to the planner's OPENAI_* settings.
        let base = url(v, "voice_base")?;
        if !base.is_empty() {
            out.push_str(&format!("AGENTOS_VOICE_BASE_URL={base}\n"));
        }
        if !text(v, "voice_key")?.is_empty() || flag(v, "keep_voice_key") {
            secret(&mut out, v, "voice_key", "AGENTOS_VOICE_API_KEY")?;
        }
    }
    let hass = url(v, "hass_url")?;
    if !hass.is_empty() {
        out.push_str(&format!("HASS_URL={hass}\n"));
        secret(&mut out, v, "hass_token", "HASS_TOKEN")?;
    }
    match v.get("pin").and_then(Value::as_str) {
        Some(p) if valid_pin(p) => out.push_str(&format!("PIN={p}\n")),
        Some(_) => return Err("PIN 必须是 4 到 12 位数字".into()),
        None if flag(v, "pin_clear") => out.push_str("PIN_CLEAR=1\n"),
        None => {}
    }
    Ok(out)
}

fn valid_pin(p: &str) -> bool {
    (4..=12).contains(&p.len()) && p.bytes().all(|b| b.is_ascii_digit())
}

/// Check a page request and turn it into a job.
pub fn job(v: &Value) -> Result<Job, String> {
    let op = v.get("op").and_then(Value::as_str).unwrap_or("");
    let helper = |op, input: String| Ok(Job::Helper { op, input });
    match op {
        "status" => helper("status", String::new()),
        "wifi-scan" => helper("wifi-scan", String::new()),
        "wifi-connect" => {
            let ssid = text(v, "ssid")?;
            if ssid.is_empty() || ssid.len() > 32 {
                return Err("无效的网络名".into());
            }
            let pw = text(v, "password")?;
            helper("wifi-connect", format!("SSID={ssid}\nPASSWORD={pw}\n"))
        }
        "test-ha" => {
            let mut input = format!("HASS_URL={}\n", url(v, "hass_url")?);
            secret(&mut input, v, "hass_token", "HASS_TOKEN")?;
            helper("test-ha", input)
        }
        "apply" => helper("apply", apply_input(v)?),
        "restart" => Ok(Job::Restart),
        _ => Err(format!("未知操作 {op}")),
    }
}

/// `KEY=VALUE` lines → a JSON object of strings.
fn parse_pairs(out: &str) -> Value {
    let mut m = Map::new();
    for line in out.lines() {
        if let Some((k, v)) = line.split_once('=') {
            m.insert(k.to_string(), Value::String(v.to_string()));
        }
    }
    Value::Object(m)
}

/// `nmcli -t -f IN-USE,SIGNAL,SECURITY,SSID dev wifi list` → networks, the
/// strongest entry per name, strongest first.
fn parse_wifi(out: &str) -> Value {
    let mut nets: Vec<(String, u64, bool, bool)> = Vec::new();
    for line in out.lines() {
        let mut f = line.splitn(4, ':');
        let (Some(inuse), Some(signal), Some(sec), Some(ssid)) =
            (f.next(), f.next(), f.next(), f.next())
        else {
            continue;
        };
        // Terse mode escapes ':' and '\' in values.
        let ssid = ssid.replace("\\:", ":").replace("\\\\", "\\");
        if ssid.is_empty() {
            continue;
        }
        let signal = signal.parse().unwrap_or(0);
        let secure = !sec.trim().is_empty() && sec.trim() != "--";
        let active = inuse.trim() == "*";
        match nets.iter_mut().find(|n| n.0 == ssid) {
            Some(n) => {
                n.1 = n.1.max(signal);
                n.3 |= active;
            }
            None => nets.push((ssid, signal, secure, active)),
        }
    }
    nets.sort_by(|a, b| b.3.cmp(&a.3).then(b.1.cmp(&a.1)));
    Value::Array(
        nets.into_iter()
            .map(|(ssid, signal, secure, active)| {
                json!({"ssid": ssid, "signal": signal, "secure": secure, "active": active})
            })
            .collect(),
    )
}

/// Run a job (blocking) and build the answer for the page.
pub fn run(id: &Value, job: Job) -> Value {
    let result = match job {
        Job::Restart => restart_daemon().map(|_| Value::Null),
        Job::Helper { op, input } => run_helper(op, &input).map(|out| match op {
            "wifi-scan" => parse_wifi(&out),
            _ => parse_pairs(&out),
        }),
    };
    match result {
        Ok(data) => json!({"type": "setup", "id": id, "ok": true, "data": data}),
        Err(e) => json!({"type": "setup", "id": id, "ok": false, "error": e}),
    }
}

fn run_helper(op: &str, input: &str) -> Result<String, String> {
    let helper = helper().ok_or("这台电脑上没有 agentos-setup")?;
    // Already root (containers, tests): no sudo needed.
    let mut cmd = if uid() == Some(0) {
        Command::new(&helper)
    } else {
        let mut c = Command::new("sudo");
        c.arg("-n").arg(&helper);
        c
    };
    let mut child = cmd
        .arg(format!("--{op}"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("无法运行 agentos-setup：{e}"))?;
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(input.as_bytes());
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("agentos-setup 出错：{e}"))?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    } else {
        let err = String::from_utf8_lossy(&out.stderr);
        let msg = err
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("");
        Err(if msg.is_empty() {
            format!("agentos-setup --{op} 失败（{}）", out.status)
        } else {
            msg.trim().to_string()
        })
    }
}

fn uid() -> Option<u32> {
    use std::os::unix::fs::MetadataExt;
    std::fs::metadata("/proc/self").ok().map(|m| m.uid())
}

/// agentosd runs as this user under agentos-daemon, which starts it again
/// with the new `/etc/agentos/agentos.env`.
fn restart_daemon() -> Result<(), String> {
    let uid = uid().ok_or("不知道当前用户")?;
    Command::new("pkill")
        .args(["-x", "-u", &uid.to_string(), "agentosd"])
        .status()
        .map(|_| ())
        .map_err(|e| format!("无法重启 agentosd：{e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(v: Value) -> String {
        match job(&v).unwrap() {
            Job::Helper { input, .. } => input,
            Job::Restart => unreachable!(),
        }
    }

    #[test]
    fn apply_builds_env_lines() {
        let s = input(json!({
            "op": "apply", "planner": "openai", "openai_base": "https://api.deepseek.com/v1/",
            "openai_key": "sk-1", "openai_model": "deepseek-chat", "voice": "openai",
            "speak": "auto", "voice_base": "", "voice_key": "",
            "hass_url": "http://ha.local:8123", "hass_token": "", "keep_hass_token": true,
            "pin": "2468",
        }));
        assert_eq!(
            s,
            "AGENTOS_PLANNER=openai\nOPENAI_BASE_URL=https://api.deepseek.com/v1\n\
             OPENAI_API_KEY=sk-1\nOPENAI_MODEL=deepseek-chat\nAGENTOS_STT=openai\n\
             AGENTOS_TTS=openai\nAGENTOS_SPEAK=auto\nHASS_URL=http://ha.local:8123\n\
             KEEP=HASS_TOKEN\nPIN=2468\n"
        );
    }

    #[test]
    fn saved_claude_key_is_kept() {
        let s = input(
            json!({"op": "apply", "planner": "claude", "anthropic_key": "",
            "keep_anthropic_key": true, "voice": "off", "pin_clear": true}),
        );
        assert_eq!(
            s,
            "AGENTOS_PLANNER=claude\nKEEP=ANTHROPIC_API_KEY\nPIN_CLEAR=1\n"
        );
    }

    #[test]
    fn rejects_bad_input() {
        let bad = |v: Value| job(&v).unwrap_err();
        // A newline would add a line to the env file.
        bad(
            json!({"op": "apply", "planner": "claude", "anthropic_key": "k\nAGENTOS_PLANNER=x", "voice": "off"}),
        );
        bad(json!({"op": "apply", "planner": "shell", "voice": "off"}));
        bad(json!({"op": "apply", "planner": "rule", "voice": "off", "pin": "12a4"}));
        bad(
            json!({"op": "apply", "planner": "openai", "openai_base": "file:///etc", "openai_model": "m", "voice": "off"}),
        );
        bad(json!({"op": "wifi-connect", "ssid": ""}));
        bad(json!({"op": "rm"}));
    }

    #[test]
    fn wifi_list_is_deduplicated_and_sorted() {
        let out =
            "  :40:WPA2:Cafe\n*:62:WPA2:Home\\:5G\n  :71:WPA2:Cafe\n  :30::Open\n  :80:WPA2:\n";
        assert_eq!(
            parse_wifi(out),
            json!([
                {"ssid": "Home:5G", "signal": 62, "secure": true, "active": true},
                {"ssid": "Cafe", "signal": 71, "secure": true, "active": false},
                {"ssid": "Open", "signal": 30, "secure": false, "active": false},
            ])
        );
    }

    #[test]
    fn status_pairs_become_an_object() {
        assert_eq!(
            parse_pairs("planner=claude\nhass_url=http://x=y\n"),
            json!({"planner": "claude", "hass_url": "http://x=y"})
        );
    }
}
