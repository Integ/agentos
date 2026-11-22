//! WeChat as a channel: `agentos wechat login`, then `agentos wechat`.
//!
//! This uses WeChat's official agent channel (the ClawBot plugin and its
//! iLink protocol): the user binds a bot to their own WeChat by scanning a QR
//! code, then talks to it in a chat. The bridge long-polls for messages, so no
//! public address is needed, and it is an ordinary client of the task
//! service: what the user writes is submitted like typed text and planned and
//! policed like any other intent.
//!
//! Two things it will not do. It never approves: a step that needs the human
//! is announced in the chat and approved at the machine, with its token, as
//! everywhere else, because a chat message cannot carry that binding. And it
//! answers only the WeChat user it is bound to: the first one to write after
//! login (or the one the login names), so a forwarded bot or a leaked token
//! does not hand someone else the user's agent.

use std::collections::VecDeque;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use agentos_ipc::{Command, Reply};
use agentos_runtime::{Task, TaskId, TaskState};
use serde_json::{json, Value};
use tokio::sync::Mutex;

use crate::client::Client;
use crate::wait::{wait_for_task, WaitOutcome};

pub const API: &str = "https://ilinkai.weixin.qq.com";
/// The login, the bound user and the read position; private to the user.
pub const FILE: &str = "wechat.json";
const CHANNEL_VERSION: &str = "2.1.10";
const APP_CLIENT_VERSION: u32 = (2 << 16) | (1 << 8) | 10;
/// How long one long poll may stay open.
const POLL: Duration = Duration::from_secs(35);
/// How long one task is followed before the chat is told to look later.
const FOLLOW: Duration = Duration::from_secs(30 * 60);
/// Longest single message sent; longer replies go in parts.
const MAX_MESSAGE_CHARS: usize = 1500;
const USER_MESSAGE: i64 = 1;
const BOT_MESSAGE: i64 = 2;
const TEXT_ITEM: i64 = 1;
const FINISHED: i64 = 2;

const HELP: &str = "直接发文字就是交给 AgentOS 的一句话，接着上一句说。\n/new 开始新的对话\n/help 查看帮助\n需要批准的步骤请在电脑上批准（桌面确认面板或 agentos approve）；微信里不能批准。";

/// What `wechat.json` keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Login {
    pub token: String,
    pub bot_id: String,
    /// The only WeChat user whose messages are taken.
    pub owner: Option<String>,
    /// Where the long poll continues.
    pub cursor: String,
}

impl Login {
    pub fn load(dir: &Path) -> io::Result<Self> {
        let bytes = match std::fs::read(dir.join(FILE)) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Self::default()),
            Err(e) => return Err(e),
        };
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
        let text = |key: &str| v.get(key).and_then(Value::as_str).unwrap_or("").to_string();
        Ok(Self {
            token: text("bot_token"),
            bot_id: text("bot_id"),
            owner: Some(text("owner")).filter(|o| !o.is_empty()),
            cursor: text("cursor"),
        })
    }

    pub fn save(&self, dir: &Path) -> io::Result<()> {
        let v = json!({
            "bot_token": self.token,
            "bot_id": self.bot_id,
            "owner": self.owner.clone().unwrap_or_default(),
            "cursor": self.cursor,
        });
        agentos_core::durable::write_atomic(&dir.join(FILE), v.to_string().as_bytes())
    }
}

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut bytes = [0u8; N];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = f.read_exact(&mut bytes);
    }
    bytes
}

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// The iLink HTTP API.
pub struct Ilink {
    base: String,
    http: reqwest::Client,
}

impl Ilink {
    pub fn new(base: impl Into<String>) -> io::Result<Self> {
        let http = reqwest::Client::builder()
            // A proxy in the environment tends to break the long poll, and the
            // service is reached directly in the places it serves.
            .no_proxy()
            .user_agent(concat!("agentos/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| other(format!("无法创建网络客户端：{e}")))?;
        Ok(Self {
            base: base.into().trim_end_matches('/').to_string(),
            http,
        })
    }

    async fn post(
        &self,
        token: &str,
        path: &str,
        body: Value,
        timeout: Duration,
    ) -> io::Result<Value> {
        // The protocol wants a fresh random uin per request.
        let uin = u32::from_be_bytes(random_bytes::<4>()).to_string();
        let uin = base64(uin.as_bytes());
        let mut request = self
            .http
            .post(format!("{}/{path}", self.base))
            .timeout(timeout)
            .header("AuthorizationType", "ilink_bot_token")
            .header("X-WECHAT-UIN", uin)
            .header("iLink-App-Id", "bot")
            .header("iLink-App-ClientVersion", APP_CLIENT_VERSION.to_string())
            .json(&body);
        if !token.is_empty() {
            request = request.bearer_auth(token);
        }
        let response = request
            .send()
            .await
            .map_err(|e| other(format!("连不上微信：{e}")))?;
        if !response.status().is_success() {
            return Err(other(format!(
                "微信返回 HTTP {}",
                response.status().as_u16()
            )));
        }
        response
            .json()
            .await
            .map_err(|e| other(format!("微信的回应看不懂：{e}")))
    }

    async fn get(&self, path: &str, query: &[(&str, &str)]) -> io::Result<Value> {
        let mut url = reqwest::Url::parse(&format!("{}/{path}", self.base))
            .map_err(|e| other(format!("微信接口地址无效：{e}")))?;
        url.query_pairs_mut().extend_pairs(query);
        let response = self
            .http
            .get(url)
            .timeout(Duration::from_secs(40))
            .send()
            .await
            .map_err(|e| other(format!("连不上微信：{e}")))?;
        response
            .json()
            .await
            .map_err(|e| other(format!("微信的回应看不懂：{e}")))
    }

    /// New messages after `cursor`, and the cursor to continue from. A login
    /// that is no longer valid is `PermissionDenied`.
    pub async fn updates(&self, token: &str, cursor: &str) -> io::Result<(Vec<Value>, String)> {
        let body =
            json!({"get_updates_buf": cursor, "base_info": {"channel_version": CHANNEL_VERSION}});
        let v = match self
            .post(
                token,
                "ilink/bot/getupdates",
                body,
                POLL + Duration::from_secs(10),
            )
            .await
        {
            Ok(v) => v,
            // An empty long poll may just end in a timeout.
            Err(e) if e.to_string().contains("timed out") => {
                return Ok((Vec::new(), cursor.into()))
            }
            Err(e) => return Err(e),
        };
        match v.get("errcode").and_then(Value::as_i64).unwrap_or(0) {
            0 => {}
            -14 => {
                return Err(io::Error::new(
                    io::ErrorKind::PermissionDenied,
                    "微信登录已失效，请重新运行 agentos wechat login",
                ))
            }
            code => {
                return Err(other(format!(
                    "微信返回错误 {code}：{}",
                    v.get("errmsg").and_then(Value::as_str).unwrap_or("")
                )))
            }
        }
        let next = v
            .get("get_updates_buf")
            .and_then(Value::as_str)
            .filter(|c| !c.is_empty())
            .unwrap_or(cursor)
            .to_string();
        let msgs = v
            .get("msgs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        Ok((msgs, next))
    }

    pub async fn send_text(
        &self,
        token: &str,
        to: &str,
        context: &str,
        text: &str,
    ) -> io::Result<()> {
        for part in chunks(text, MAX_MESSAGE_CHARS) {
            let client_id = format!("agentos-{}", hex(&random_bytes::<8>()));
            let mut msg = json!({
                "from_user_id": "",
                "to_user_id": to,
                "client_id": client_id,
                "message_type": BOT_MESSAGE,
                "message_state": FINISHED,
                "item_list": [{"type": TEXT_ITEM, "text_item": {"text": part}}],
            });
            if !context.is_empty() {
                msg["context_token"] = json!(context);
            }
            let v = self
                .post(
                    token,
                    "ilink/bot/sendmessage",
                    json!({"msg": msg, "base_info": {"channel_version": CHANNEL_VERSION}}),
                    Duration::from_secs(15),
                )
                .await?;
            if let Some(code) = v.get("errcode").and_then(Value::as_i64).filter(|c| *c != 0) {
                return Err(other(format!("微信没有发出消息（错误 {code}）")));
            }
        }
        Ok(())
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (*chunk.get(1).unwrap_or(&0) as u32) << 8
            | *chunk.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(TABLE[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// `text` in parts of at most `max` characters, split at line ends where it can.
pub fn chunks(text: &str, max: usize) -> Vec<String> {
    let mut parts = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        for c in line.chars() {
            if current.chars().count() >= max {
                parts.push(std::mem::take(&mut current));
            }
            current.push(c);
        }
        if current.chars().count() >= max * 3 / 4 {
            parts.push(std::mem::take(&mut current));
        }
    }
    if !current.trim().is_empty() {
        parts.push(current);
    }
    parts
        .into_iter()
        .map(|p| p.trim_end().to_string())
        .filter(|p| !p.is_empty())
        .collect()
}

/// A text message from a user: who sent it, the token replies go with, its
/// id and its text. Anything else (the bot's own messages, media) is `None`.
pub fn user_text(msg: &Value) -> Option<(String, String, String, String)> {
    if msg.get("message_type").and_then(Value::as_i64) != Some(USER_MESSAGE) {
        return None;
    }
    let from = msg.get("from_user_id").and_then(Value::as_str)?.to_string();
    let context = msg
        .get("context_token")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let id = msg
        .get("message_id")
        .map(|v| v.to_string())
        .unwrap_or_default();
    let text: Vec<&str> = msg
        .get("item_list")
        .and_then(Value::as_array)?
        .iter()
        .filter(|item| item.get("type").and_then(Value::as_i64) == Some(TEXT_ITEM))
        .filter_map(|item| item.pointer("/text_item/text").and_then(Value::as_str))
        .collect();
    Some((from, context, id, text.join("\n")))
}

/// What the chat is told when a task stops for good.
pub fn settled_text(task: &Task) -> String {
    let message = task.message.clone().unwrap_or_default();
    match task.state {
        TaskState::Succeeded if message.is_empty() => "完成了。".into(),
        TaskState::Succeeded => message,
        TaskState::Unhandled => format!("没能处理（{}）：{message}", task.id),
        TaskState::Failed => format!("没有做成（{}）：{message}", task.id),
        TaskState::Cancelled => format!("{} 已取消。", task.id),
        TaskState::Interrupted => format!("{} 被中断了（服务重启过），没有接着做。", task.id),
        _ => format!("{} 还在进行。", task.id),
    }
}

/// What the chat is told when a step waits for the human's approval.
pub fn approval_text(task: &Task) -> String {
    let step = task.confirmation.as_ref().map(|c| c.step);
    let what = step
        .and_then(|s| task.steps.get(s))
        .map(|s| s.capability.to_string())
        .unwrap_or_else(|| "一个操作".into());
    format!(
        "{} 要执行「{what}」，需要你在电脑上批准（桌面的确认面板，或 agentos approve）。微信里不能批准。批准后结果会发到这里。",
        task.id
    )
}

/// One conversation: the thread it continues, and the question it waits on.
#[derive(Default)]
struct Chat {
    thread: Option<TaskId>,
    question: Option<(TaskId, usize)>,
}

struct Bridge {
    client: Client,
    ilink: Ilink,
    token: String,
    owner: String,
    chat: Mutex<Chat>,
}

impl Bridge {
    async fn say(&self, context: &str, text: &str) {
        if let Err(e) = self
            .ilink
            .send_text(&self.token, &self.owner, context, text)
            .await
        {
            eprintln!("发往微信失败：{e}");
        }
    }

    /// One message from the owner.
    async fn handle(self: Arc<Self>, context: String, text: String) {
        let text = text.trim().to_string();
        match text.as_str() {
            "" => return self.say(&context, "目前只能处理文字消息。").await,
            "/help" => return self.say(&context, HELP).await,
            "/new" => {
                *self.chat.lock().await = Chat::default();
                return self.say(&context, "好，开始新的对话。").await;
            }
            _ => {}
        }
        // Submitting holds the chat, so two quick messages join the same
        // thread in order; following the task does not.
        let id = {
            let mut chat = self.chat.lock().await;
            if let Some((id, step)) = chat.question.take() {
                match self
                    .client
                    .request(Command::Reply {
                        id,
                        step,
                        text: text.clone(),
                        spoken: false,
                    })
                    .await
                {
                    Ok(_) => id,
                    // Not resent: the answer may have arrived anyway.
                    Err(e) => return self.say(&context, &format!("没能交上回答：{e}")).await,
                }
            } else {
                match self
                    .client
                    .request(Command::Submit {
                        text: text.clone(),
                        thread: chat.thread,
                        spoken: false,
                    })
                    .await
                {
                    Ok(Reply::Accepted { task }) => {
                        chat.thread.get_or_insert(task.id);
                        task.id
                    }
                    Ok(_) => return,
                    // Not resent either: a submit whose answer was lost may
                    // still have created the task.
                    Err(e) => return self.say(&context, &format!("没能交给 AgentOS：{e}")).await,
                }
            }
        };
        self.follow(id, &context).await;
    }

    /// Tell the chat how task `id` goes until it ends or asks something.
    async fn follow(&self, id: TaskId, context: &str) {
        loop {
            let outcome = match wait_for_task(&self.client, id, FOLLOW).await {
                Ok(outcome) => outcome,
                Err(e) => return self.say(context, &format!("查不到 {id} 的进展：{e}")).await,
            };
            match outcome {
                WaitOutcome::Settled(task) => return self.say(context, &settled_text(&task)).await,
                WaitOutcome::NeedsInput(task) => {
                    if let Some(q) = &task.question {
                        self.chat.lock().await.question = Some((id, q.step));
                        let text = format!("{}\n（直接回复这条消息就是回答）", q.text);
                        return self.say(context, &text).await;
                    }
                }
                WaitOutcome::NeedsApproval(task) => {
                    self.say(context, &approval_text(&task)).await;
                    if !self.await_decision(id).await {
                        return;
                    }
                }
                WaitOutcome::TimedOut(_) => {
                    let text =
                        format!("{id} 还在进行；之后可以在电脑上用 agentos task {id} 查看。");
                    return self.say(context, &text).await;
                }
            }
        }
    }

    /// Wait while `id` waits for approval. False when it is still waiting
    /// at the end, so the chat is not told the same thing again and again.
    async fn await_decision(&self, id: TaskId) -> bool {
        let deadline = tokio::time::Instant::now() + FOLLOW;
        while tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_secs(3)).await;
            match self.client.request(Command::Get { id }).await {
                Ok(Reply::Task { task }) if task.state != TaskState::AwaitingConfirmation => {
                    return true
                }
                Ok(_) => {}
                Err(_) => return false,
            }
        }
        false
    }
}

/// Log in by QR code and remember the bot. A new login forgets the bound
/// user, unless the login itself says who it is.
pub async fn login(dir: &Path, base: &str) -> io::Result<()> {
    let ilink = Ilink::new(base)?;
    let v = ilink
        .get("ilink/bot/get_bot_qrcode", &[("bot_type", "3")])
        .await?;
    let (Some(qrcode), Some(content)) = (
        v.get("qrcode").and_then(Value::as_str),
        v.get("qrcode_img_content").and_then(Value::as_str),
    ) else {
        return Err(other("微信没有给出二维码（可能被限流），请稍后再试"));
    };
    let code = qrcode::QrCode::new(content.as_bytes())
        .map_err(|e| other(format!("无法生成二维码：{e}")))?;
    let picture = code
        .render::<qrcode::render::unicode::Dense1x2>()
        .quiet_zone(true)
        .build();
    println!("用微信扫描下面的二维码，在手机上确认（需要微信的 ClawBot 插件：我 → 设置 → 插件）：\n\n{picture}\n");
    let mut last = String::new();
    for _ in 0..150 {
        tokio::time::sleep(Duration::from_secs(2)).await;
        let Ok(s) = ilink
            .get("ilink/bot/get_qrcode_status", &[("qrcode", qrcode)])
            .await
        else {
            continue;
        };
        let status = s
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_string();
        if status != last {
            match status.as_str() {
                "scaned" => println!("已扫码，请在手机上确认…"),
                "expired" => return Err(other("二维码已过期，请重新运行 agentos wechat login")),
                _ => {}
            }
            last = status.clone();
        }
        if status == "confirmed" {
            let text = |key: &str| s.get(key).and_then(Value::as_str).unwrap_or("").to_string();
            let login = Login {
                token: text("bot_token"),
                bot_id: text("ilink_bot_id"),
                owner: Some(text("ilink_user_id")).filter(|o| !o.is_empty()),
                cursor: String::new(),
            };
            if login.token.is_empty() {
                return Err(other("微信确认了登录，但没有给出令牌"));
            }
            login.save(dir)?;
            println!(
                "已登录。运行 agentos wechat 开始收发消息；{}",
                if login.owner.is_some() {
                    "只接受这个微信号的消息。"
                } else {
                    "第一个给它发消息的微信号会被绑定，之后只接受它的消息。"
                }
            );
            return Ok(());
        }
    }
    Err(other("等待扫码超时，请重新运行 agentos wechat login"))
}

/// Relay messages until the login stops working or the process is stopped.
pub async fn run(client: Client, dir: &Path, base: &str) -> io::Result<()> {
    let mut login = Login::load(dir)?;
    if login.token.is_empty() {
        return Err(other("还没有登录微信：先运行 agentos wechat login"));
    }
    let ilink = Ilink::new(base)?;
    let mut bridge: Option<Arc<Bridge>> = None;
    let mut seen: VecDeque<String> = VecDeque::new();
    let mut backoff = Duration::from_secs(2);
    println!("正在接收微信消息（Ctrl+C 停止）。");
    loop {
        let (msgs, cursor) = match ilink.updates(&login.token, &login.cursor).await {
            Ok(update) => {
                backoff = Duration::from_secs(2);
                update
            }
            Err(e) if e.kind() == io::ErrorKind::PermissionDenied => {
                login.token.clear();
                login.cursor.clear();
                let _ = login.save(dir);
                return Err(e);
            }
            Err(e) => {
                eprintln!("{e}；{} 秒后重试", backoff.as_secs());
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(Duration::from_secs(60));
                continue;
            }
        };
        if cursor != login.cursor {
            login.cursor = cursor;
            login.save(dir)?;
        }
        for msg in msgs {
            let Some((from, context, id, text)) = user_text(&msg) else {
                continue;
            };
            if !id.is_empty() {
                if seen.contains(&id) {
                    continue;
                }
                seen.push_back(id);
                if seen.len() > 1000 {
                    seen.pop_front();
                }
            }
            let owner = match &login.owner {
                Some(owner) => owner.clone(),
                None => {
                    login.owner = Some(from.clone());
                    login.save(dir)?;
                    eprintln!("已绑定微信用户，之后只接受这个微信号的消息。");
                    from.clone()
                }
            };
            if from != owner {
                eprintln!("忽略了一条来自其他微信用户的消息");
                continue;
            }
            let bridge = bridge
                .get_or_insert_with(|| {
                    Arc::new(Bridge {
                        client: client.clone(),
                        ilink: Ilink::new(base).expect("the client was built once already"),
                        token: login.token.clone(),
                        owner: owner.clone(),
                        chat: Mutex::new(Chat::default()),
                    })
                })
                .clone();
            tokio::spawn(bridge.handle(context, text));
        }
    }
}

/// Where the WeChat login is kept: the state directory.
pub fn state_dir(explicit: Option<PathBuf>) -> io::Result<PathBuf> {
    match explicit {
        Some(dir) => Ok(dir),
        None => agentos_ipc::default_state_dir(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_core::{now_ms, DeviceId, Intent, IntentId, Urgency};

    fn task(state: TaskState, message: Option<&str>) -> Task {
        let store = agentos_runtime::TaskStore::memory();
        let task = store
            .create(Intent {
                id: IntentId(0),
                source: DeviceId::new("unix:uid:1000"),
                speaker: None,
                text: "看看天气".into(),
                urgency: Urgency::Normal,
                at_ms: now_ms(),
            })
            .unwrap();
        store
            .update(task.id, |t| {
                t.state = state;
                t.message = message.map(str::to_string);
                if state == TaskState::AwaitingConfirmation {
                    t.steps = vec![agentos_runtime::TaskStep::pending(
                        agentos_core::CapabilityName::new("mail.send"),
                        json!({}),
                    )];
                    t.confirmation = Some(agentos_runtime::Confirmation {
                        step: 0,
                        expires_at_ms: now_ms() + 60_000,
                        token: "t".into(),
                        verify_identity: false,
                    });
                }
            })
            .unwrap()
    }

    #[test]
    fn only_text_from_users_is_taken() {
        let msg = json!({
            "message_type": 1, "from_user_id": "u1", "context_token": "ctx", "message_id": 7,
            "item_list": [{"type": 1, "text_item": {"text": "记一下 买牛奶"}}, {"type": 2, "image_item": {}}]
        });
        assert_eq!(
            user_text(&msg),
            Some((
                "u1".into(),
                "ctx".into(),
                "7".into(),
                "记一下 买牛奶".into()
            ))
        );
        // The bot's own messages come back through the poll too.
        assert!(
            user_text(&json!({"message_type": 2, "from_user_id": "bot", "item_list": []}))
                .is_none()
        );
    }

    #[test]
    fn outcomes_are_told_as_they_are() {
        // Guards honest outcomes: the chat never hears success for a task
        // that failed or was not understood.
        assert_eq!(
            settled_text(&task(TaskState::Succeeded, Some("晴，25 度"))),
            "晴，25 度"
        );
        assert!(settled_text(&task(TaskState::Failed, Some("超出预算"))).starts_with("没有做成"));
        assert!(settled_text(&task(TaskState::Unhandled, Some("听不懂"))).starts_with("没能处理"));
        assert!(settled_text(&task(TaskState::Interrupted, None)).contains("中断"));
        // Guards "approval is bound to the request": the chat is told where
        // to approve, never offered a way to do it from there.
        let waiting = task(TaskState::AwaitingConfirmation, None);
        assert!(approval_text(&waiting).contains("微信里不能批准"));
        assert!(approval_text(&waiting).contains("mail.send"));
    }

    #[test]
    fn long_replies_go_in_parts() {
        let text = "一二三四五\n".repeat(400);
        let parts = chunks(&text, 100);
        assert!(parts.iter().all(|p| p.chars().count() <= 100));
        assert_eq!(parts.concat().replace('\n', ""), text.replace('\n', ""));
        assert_eq!(chunks("短", 100), vec!["短".to_string()]);
    }

    #[test]
    fn the_login_is_kept_private_and_survives_a_restart() {
        let dir = std::env::temp_dir().join(format!(
            "agentos-wechat-{}-{}",
            std::process::id(),
            now_ms()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(Login::load(&dir).unwrap(), Login::default());
        let login = Login {
            token: "tok".into(),
            bot_id: "bot".into(),
            owner: Some("u1".into()),
            cursor: "c1".into(),
        };
        login.save(&dir).unwrap();
        assert_eq!(Login::load(&dir).unwrap(), login);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(dir.join(FILE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        // Guards the token: readable by the user alone.
        assert_eq!(mode, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn uin_is_base64() {
        assert_eq!(base64(b"123"), "MTIz");
        assert_eq!(base64(b"1234"), "MTIzNA==");
        assert_eq!(base64(b"12345"), "MTIzNDU=");
    }
}
