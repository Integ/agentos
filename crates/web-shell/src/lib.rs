//! Bounded, loopback-only HTTP presentation bridge. The daemon remains the
//! authority for planning, task persistence, policy, and approval validation.
//! API requests require an unguessable per-launch capability, carried in a URL
//! fragment at launch and an HTTP header thereafter; it is never put in a URL
//! query, cookie, Referer, source file, or persistent gateway configuration.

use std::collections::BTreeMap;
use std::io::{self, Read};
use std::path::PathBuf;
use std::time::Duration;

use agentos_cli::client::Client;
use agentos_core::CapabilityName;
use agentos_ipc::{
    default_socket_path, default_state_dir, Command, MAX_CAPABILITY_NAME_BYTES, MAX_PAGE_SIZE,
    MAX_TEXT_BYTES,
};
use agentos_runtime::{TaskId, TaskState};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

pub mod model_scene;
pub mod realtime;

const MAX_HEADERS: usize = 16_384;
const MAX_BODY: usize = MAX_TEXT_BYTES + 1_024;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const INDEX: &str = include_str!("../assets/index.html");
const CSS: &str = include_str!("../assets/shell.css");
const JS: &str = include_str!("../assets/shell.js");
const SCENE_JS: &str = include_str!("../assets/scene.js");
const LAYOUT_CLIENT_JS: &str = include_str!("../assets/layout-client.js");
const REALTIME_VOICE_JS: &str = include_str!("../assets/realtime-voice.js");
const VOICE_TOOLS_JS: &str = include_str!("../assets/voice-tools.js");

pub const HELP: &str = "AgentOS · 语音优先情景工作台\n\n用法：agentos-web-shell [--state-dir 路径 | --socket 路径] [--port 4317]\n\n仅绑定 127.0.0.1。打开启动时显示的完整链接连接本地服务。\n未启动 agentos-taskd 时可体验界面演示；真实任务需要后台服务。\n--help, -h 查看帮助；--version, -V 查看版本。";

pub struct Options {
    pub socket: PathBuf,
    pub port: u16,
}

impl Options {
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Option<Self>, String> {
        let mut args = args.into_iter();
        let mut socket = None;
        let mut directory = None;
        let mut port = 4317;
        while let Some(argument) = args.next() {
            if argument == "--help" || argument == "-h" {
                return Ok(None);
            }
            let value = args
                .next()
                .filter(|value| !value.is_empty() && !value.starts_with("--"))
                .ok_or_else(|| format!("{argument} 缺少值"))?;
            match argument.as_str() {
                "--socket" => socket = Some(PathBuf::from(value)),
                "--state-dir" => directory = Some(PathBuf::from(value)),
                "--port" => {
                    port = value
                        .parse::<u16>()
                        .ok()
                        .filter(|value| *value > 0)
                        .ok_or_else(|| "端口必须为 1–65535".to_string())?;
                }
                _ => return Err(format!("未知参数：{argument}")),
            }
        }
        if socket.is_some() && directory.is_some() {
            return Err("--socket 与 --state-dir 请只指定一个".into());
        }
        let socket = match socket {
            Some(path) => path,
            None => default_socket_path(&match directory {
                Some(path) => path,
                None => default_state_dir().map_err(|error| error.to_string())?,
            }),
        };
        Ok(Some(Self { socket, port }))
    }
}

pub struct Config {
    client: Client,
    port: u16,
    session: String,
    layout: Option<model_scene::SceneGenerator>,
    voice: Option<realtime::VoiceService>,
}

impl Config {
    pub fn new(client: Client, port: u16) -> io::Result<Self> {
        let mut bytes = [0_u8; 32];
        std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
        let session = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
        Ok(Self {
            client,
            port,
            session,
            layout: model_scene::SceneGenerator::from_env()?,
            voice: realtime::VoiceService::from_env()?,
        })
    }

    pub fn session(&self) -> &str {
        &self.session
    }

    pub async fn shutdown_voice(&self) {
        if let Some(voice) = self.voice.as_ref() {
            voice.shutdown().await;
        }
    }
}

#[derive(Debug)]
struct Request {
    method: String,
    target: String,
    headers: BTreeMap<String, String>,
    body: Vec<u8>,
}

#[derive(Debug)]
struct HttpError {
    status: u16,
    code: &'static str,
    message: String,
}

impl HttpError {
    fn new(status: u16, code: &'static str, message: impl Into<String>) -> Self {
        Self {
            status,
            code,
            message: message.into(),
        }
    }

    fn bad(message: impl Into<String>) -> Self {
        Self::new(400, "invalid_request", message)
    }

    fn response(self) -> Response {
        Response::json(
            self.status,
            json!({"error": {
                "code": self.code, "message": self.message, "outcome_unknown": false
            }}),
        )
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn json(status: u16, value: Value) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body: value.to_string().into_bytes(),
        }
    }

    fn asset(content_type: &'static str, text: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: text.as_bytes().to_vec(),
        }
    }
}

fn parse_headers(raw: &[u8]) -> Result<Request, HttpError> {
    let text = std::str::from_utf8(raw).map_err(|_| HttpError::bad("请求头必须为有效文本"))?;
    let mut lines = text.split("\r\n");
    let start = lines.next().ok_or_else(|| HttpError::bad("缺少请求行"))?;
    let parts: Vec<_> = start.split(' ').collect();
    if parts.len() != 3
        || parts[2] != "HTTP/1.1"
        || !parts[1].starts_with('/')
        || parts[1].starts_with("//")
        || parts[1].bytes().any(|b| b <= 32 || b >= 127)
    {
        return Err(HttpError::bad("无效的 HTTP 请求行"));
    }
    let mut headers = BTreeMap::new();
    for line in lines.filter(|line| !line.is_empty()) {
        let (name, value) = line
            .split_once(':')
            .ok_or_else(|| HttpError::bad("无效的请求头"))?;
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
            || value
                .bytes()
                .any(|byte| byte < 32 && byte != b'\t' || byte == 127)
        {
            return Err(HttpError::bad("无效的请求头"));
        }
        if headers
            .insert(name.to_ascii_lowercase(), value.trim().to_string())
            .is_some()
        {
            return Err(HttpError::bad("不允许重复请求头"));
        }
    }
    if headers.contains_key("transfer-encoding") {
        return Err(HttpError::bad("不支持分块请求"));
    }
    Ok(Request {
        method: parts[0].into(),
        target: parts[1].into(),
        headers,
        body: Vec::new(),
    })
}

fn validate_boundary(request: &Request, config: &Config) -> Result<(), HttpError> {
    let host = request
        .headers
        .get("host")
        .ok_or_else(|| HttpError::bad("缺少 Host"))?;
    let allowed = [
        format!("127.0.0.1:{}", config.port),
        format!("localhost:{}", config.port),
    ];
    if !allowed.contains(host) {
        return Err(HttpError::new(403, "invalid_host", "仅接受本机地址"));
    }
    let is_api = request.target.starts_with("/api/");
    if (is_api || request.method == "POST")
        && (request
            .headers
            .get("origin")
            .is_some_and(|origin| origin != &format!("http://{host}"))
            || request
                .headers
                .get("sec-fetch-site")
                .is_some_and(|site| site == "cross-site"))
    {
        return Err(HttpError::new(403, "invalid_origin", "拒绝跨站请求"));
    }
    if is_api {
        let supplied = request
            .headers
            .get("x-agentos-session")
            .map(String::as_bytes)
            .unwrap_or_default();
        let expected = config.session.as_bytes();
        // Do not expose a useful prefix oracle for the per-launch capability.
        let mut difference = supplied.len() ^ expected.len();
        for (index, byte) in expected.iter().enumerate() {
            difference |= usize::from(*byte ^ supplied.get(index).copied().unwrap_or(0));
        }
        if difference != 0 {
            return Err(HttpError::new(
                401,
                "session_required",
                "请打开本次启动显示的完整链接，以连接本机服务",
            ));
        }
    }
    Ok(())
}

async fn read_request(stream: &mut TcpStream, config: &Config) -> Result<Request, HttpError> {
    let mut buffer = Vec::with_capacity(4096);
    let header_end = loop {
        if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            if end + 4 > MAX_HEADERS {
                return Err(HttpError::new(431, "headers_too_large", "请求头过大"));
            }
            break end + 4;
        }
        if buffer.len() >= MAX_HEADERS {
            return Err(HttpError::new(431, "headers_too_large", "请求头过大"));
        }
        let mut chunk = [0; 2048];
        let count = stream
            .read(&mut chunk)
            .await
            .map_err(|error| HttpError::bad(error.to_string()))?;
        if count == 0 {
            return Err(HttpError::bad("请求提前结束"));
        }
        buffer.extend_from_slice(&chunk[..count]);
    };
    let mut request = parse_headers(&buffer[..header_end - 4])?;
    validate_boundary(&request, config)?;
    let length = match request.headers.get("content-length") {
        Some(value) if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
            value
                .parse::<usize>()
                .map_err(|_| HttpError::bad("无效的 Content-Length"))?
        }
        Some(_) => return Err(HttpError::bad("无效的 Content-Length")),
        None => 0,
    };
    if length > MAX_BODY {
        return Err(HttpError::new(413, "body_too_large", "请求正文过大"));
    }
    if request.method == "POST" {
        let content_type = request
            .headers
            .get("content-type")
            .map(|value| value.split(';').next().unwrap_or("").trim());
        if content_type != Some("application/json") {
            return Err(HttpError::new(
                415,
                "json_required",
                "提交操作必须使用 application/json",
            ));
        }
    } else if length > 0 {
        return Err(HttpError::bad("此请求不接受正文"));
    }
    let present = (buffer.len() - header_end).min(length);
    request
        .body
        .extend_from_slice(&buffer[header_end..header_end + present]);
    request.body.resize(length, 0);
    stream
        .read_exact(&mut request.body[present..])
        .await
        .map_err(|error| HttpError::bad(error.to_string()))?;
    Ok(request)
}

fn task_id(value: &str) -> Result<TaskId, HttpError> {
    value
        .parse::<TaskId>()
        .ok()
        .filter(|id| id.0 > 0)
        .ok_or_else(|| HttpError::bad("无效的任务编号"))
}

fn query(value: &str) -> Result<BTreeMap<&str, &str>, HttpError> {
    let mut result = BTreeMap::new();
    if value.is_empty() {
        return Ok(result);
    }
    for pair in value.split('&') {
        let (key, value) = pair
            .split_once('=')
            .ok_or_else(|| HttpError::bad("无效的查询参数"))?;
        if result.insert(key, value).is_some() {
            return Err(HttpError::bad("不允许重复查询参数"));
        }
    }
    Ok(result)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    text: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Approval {
    step: usize,
    confirmation: String,
    /// The PIN, for an irreversible step. Passed through, never kept.
    #[serde(default)]
    pin: Option<String>,
}

fn command(request: &Request) -> Result<Command, HttpError> {
    let (path, parameters) = request
        .target
        .split_once('?')
        .unwrap_or((&request.target, ""));
    match (request.method.as_str(), path) {
        ("GET", "/api/status") if parameters.is_empty() => Ok(Command::Ping {}),
        ("GET", "/api/capabilities") if parameters.is_empty() => Ok(Command::Capabilities {}),
        ("GET", "/api/tasks") => {
            let parameters = query(parameters)?;
            if parameters
                .keys()
                .any(|key| !["limit", "before", "state"].contains(key))
            {
                return Err(HttpError::bad("未知查询参数"));
            }
            let limit = parameters
                .get("limit")
                .map(|value| value.parse::<usize>())
                .transpose()
                .map_err(|_| HttpError::bad("无效的分页数量"))?
                .unwrap_or(20);
            if !(1..=MAX_PAGE_SIZE).contains(&limit) {
                return Err(HttpError::bad("分页数量必须为 1–50"));
            }
            let before = parameters
                .get("before")
                .map(|value| task_id(value))
                .transpose()?;
            let state = parameters
                .get("state")
                .map(|value| serde_json::from_value::<TaskState>(json!(value)))
                .transpose()
                .map_err(|_| HttpError::bad("无效的任务状态"))?;
            Ok(Command::List {
                before,
                limit,
                state,
            })
        }
        // Reading state is a query, not an intent: the browser may refresh
        // what it shows without submitting a task on the user's behalf. The
        // service only serves granted read-only capabilities this way.
        ("GET", path) if path.starts_with("/api/read/") && parameters.is_empty() => {
            let name = &path["/api/read/".len()..];
            if name.is_empty()
                || name.len() > MAX_CAPABILITY_NAME_BYTES
                || !name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
            {
                return Err(HttpError::bad("无效的能力名称"));
            }
            Ok(Command::Read {
                capability: CapabilityName::new(name),
                args: json!({}),
            })
        }
        ("POST", "/api/intent") if parameters.is_empty() => {
            let intent: Intent = serde_json::from_slice(&request.body)
                .map_err(|_| HttpError::bad("需要仅包含 text 字符串的 JSON 对象"))?;
            let text = intent.text.trim().to_string();
            if text.is_empty() || text.len() > MAX_TEXT_BYTES {
                return Err(HttpError::bad("意图不能为空，且不得超过 64 KiB"));
            }
            Ok(Command::Submit { text })
        }
        (method, path) if path.starts_with("/api/tasks/") && parameters.is_empty() => {
            let parts: Vec<_> = path["/api/tasks/".len()..].split('/').collect();
            let id = task_id(parts[0])?;
            match (method, parts.as_slice()) {
                ("GET", [_]) => Ok(Command::Get { id }),
                ("POST", [_, "cancel"]) => {
                    let body: Value = serde_json::from_slice(&request.body)
                        .map_err(|_| HttpError::bad("需要空 JSON 对象"))?;
                    if body != json!({}) {
                        return Err(HttpError::bad("取消操作需要空 JSON 对象"));
                    }
                    Ok(Command::Cancel { id })
                }
                ("POST", [_, "approve"]) => {
                    let approval: Approval = serde_json::from_slice(&request.body)
                        .map_err(|_| HttpError::bad("需要 step 与 confirmation"))?;
                    if approval.step == 0
                        || approval.confirmation.is_empty()
                        || approval.confirmation.len() > 1024
                    {
                        return Err(HttpError::bad("批准步骤从 1 开始，并需要有效的确认令牌"));
                    }
                    if approval.pin.as_ref().is_some_and(|pin| pin.len() > 64) {
                        return Err(HttpError::bad("PIN 无效"));
                    }
                    Ok(Command::Approve {
                        id,
                        step: approval.step,
                        confirmation: approval.confirmation,
                        pin: approval.pin,
                    })
                }
                _ => Err(HttpError::new(404, "not_found", "没有此接口")),
            }
        }
        _ => Err(HttpError::new(404, "not_found", "没有此接口")),
    }
}

async fn route(request: Request, config: &Config) -> Response {
    match (request.method.as_str(), request.target.as_str()) {
        ("GET", "/") | ("GET", "/index.html") => {
            return Response::asset("text/html; charset=utf-8", INDEX)
        }
        ("GET", "/shell.css") => return Response::asset("text/css; charset=utf-8", CSS),
        ("GET", "/shell.js") => return Response::asset("text/javascript; charset=utf-8", JS),
        ("GET", "/scene.js") => return Response::asset("text/javascript; charset=utf-8", SCENE_JS),
        ("GET", "/layout-client.js") => {
            return Response::asset("text/javascript; charset=utf-8", LAYOUT_CLIENT_JS)
        }
        ("GET", "/realtime-voice.js") => {
            return Response::asset("text/javascript; charset=utf-8", REALTIME_VOICE_JS)
        }
        ("GET", "/voice-tools.js") => {
            return Response::asset("text/javascript; charset=utf-8", VOICE_TOOLS_JS)
        }
        ("GET", "/api/voice/status") => {
            return Response::json(200, realtime::status(config.voice.as_ref()))
        }
        ("POST", "/api/voice/session") => {
            let request = match serde_json::from_slice::<realtime::SessionRequest>(&request.body) {
                Ok(request) => request,
                Err(_) => {
                    return HttpError::new(400, "invalid_voice_context", "实时语音连接参数格式无效")
                        .response()
                }
            };
            let Some(voice) = config.voice.as_ref() else {
                return HttpError::new(
                    503,
                    "voice_unavailable",
                    "未配置实时语音模型，可继续使用键盘",
                )
                .response();
            };
            return match voice.create(request, &config.client).await {
                Ok(response) => Response::json(200, response),
                Err(error) => HttpError::new(error.status, error.code, error.message).response(),
            };
        }
        ("POST", "/api/voice/hangup") => {
            let request = match serde_json::from_slice::<realtime::HangupRequest>(&request.body) {
                Ok(request) => request,
                Err(_) => {
                    return HttpError::new(400, "invalid_voice_context", "实时语音会话参数格式无效")
                        .response()
                }
            };
            let Some(voice) = config.voice.as_ref() else {
                return HttpError::new(503, "voice_unavailable", "未配置实时语音模型").response();
            };
            return match voice.hangup(&request.call_id).await {
                Ok(response) => Response::json(200, response),
                Err(error) => HttpError::new(error.status, error.code, error.message).response(),
            };
        }
        ("GET", "/api/layout/status") => {
            return Response::json(200, model_scene::status(config.layout.as_ref()))
        }
        ("POST", "/api/layout") => {
            let proposal = match serde_json::from_slice::<model_scene::LayoutRequest>(&request.body)
            {
                Ok(proposal) => proposal,
                Err(_) => {
                    return HttpError::new(400, "invalid_layout_context", "布局上下文格式无效")
                        .response()
                }
            };
            let Some(generator) = config.layout.as_ref() else {
                return HttpError::new(
                    503,
                    "layout_unavailable",
                    "未配置布局模型；保留本地情景布局",
                )
                .response();
            };
            return match generator.generate(proposal, &config.client).await {
                Ok(result) => Response::json(200, result),
                Err(error) => HttpError::new(error.status, error.code, error.message).response(),
            };
        }
        ("GET", "/favicon.ico") => {
            return Response {
                status: 204,
                content_type: "image/x-icon",
                body: Vec::new(),
            }
        }
        _ => {}
    }
    let command = match command(&request) {
        Ok(command) => command,
        Err(error) => return error.response(),
    };
    match config.client.request(command).await {
        Ok(reply) => match serde_json::to_value(reply) {
            Ok(reply) => Response::json(200, reply),
            Err(error) => HttpError::new(500, "serialization_error", error.to_string()).response(),
        },
        Err(error) => {
            let status = match &error {
                agentos_cli::client::ClientError::Remote(error) if error.code == "not_found" => 404,
                agentos_cli::client::ClientError::Remote(_) => 400,
                _ => 503,
            };
            Response::json(status, error.json())
        }
    }
}

async fn write_response(stream: &mut TcpStream, response: Response) -> io::Result<()> {
    let reason = match response.status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        408 => "Request Timeout",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        429 => "Too Many Requests",
        502 => "Bad Gateway",
        503 => "Service Unavailable",
        _ => "Internal Server Error",
    };
    let headers = format!(
        "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\nContent-Security-Policy: default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'; img-src 'self' data:; font-src 'self'; connect-src 'self'; media-src 'self' blob:; object-src 'none'; base-uri 'none'; form-action 'none'\r\n\r\n",
        response.status, reason, response.content_type, response.body.len()
    );
    stream.write_all(headers.as_bytes()).await?;
    stream.write_all(&response.body).await?;
    stream.shutdown().await
}

pub async fn handle_connection(mut stream: TcpStream, config: &Config) -> io::Result<()> {
    // Only time out parsing here: cancelling an in-flight mutation could hide
    // whether it was sent. Client::request owns bounded mutation transport and
    // reports its own outcome_unknown flag without ever replaying the command.
    let mut voice_creation = false;
    let response = match tokio::time::timeout(REQUEST_TIMEOUT, read_request(&mut stream, config))
        .await
    {
        Ok(Ok(request)) if request.method == "POST" && request.target == "/api/layout" => {
            // Layout generation is presentation-only. A browser abort may
            // safely cancel it, freeing its model permit and HTTP future.
            // Mutation routes below deliberately retain their no-cancel
            // acknowledgement semantics. Pipelining is not supported.
            let mut extra = [0_u8; 1];
            tokio::select! {
                response = route(request, config) => response,
                _ = stream.read(&mut extra) => return Ok(()),
            }
        }
        Ok(Ok(request)) if request.method == "POST" && request.target == "/api/voice/session" => {
            voice_creation = true;
            let creating = route(request, config);
            tokio::pin!(creating);
            let mut extra = [0_u8; 1];
            tokio::select! {
                response = &mut creating => response,
                _ = stream.read(&mut extra) => {
                    // Session creation may already have taken effect. Finish
                    // exactly this request and hang up the known call rather
                    // than replaying it or abandoning an unowned live call.
                    let response=creating.await;
                    cleanup_voice_response(&response,config).await;
                    return Ok(());
                }
            }
        }
        Ok(Ok(request)) => route(request, config).await,
        Ok(Err(error)) => error.response(),
        Err(_) => HttpError::new(408, "request_timeout", "读取请求超时").response(),
    };
    let cleanup = if voice_creation {
        voice_response_id(&response)
    } else {
        None
    };
    let result = write_response(&mut stream, response).await;
    if result.is_err() {
        if let (Some(id), Some(voice)) = (cleanup, config.voice.as_ref()) {
            let _ = voice.hangup(&id).await;
        }
    }
    result
}

fn voice_response_id(response: &Response) -> Option<String> {
    if response.status != 200 {
        return None;
    }
    serde_json::from_slice::<Value>(&response.body)
        .ok()?
        .get("call_id")?
        .as_str()
        .map(str::to_owned)
}

async fn cleanup_voice_response(response: &Response, config: &Config) {
    if let (Some(id), Some(voice)) = (voice_response_id(response), config.voice.as_ref()) {
        let _ = voice.hangup(&id).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    fn config() -> Config {
        Config {
            client: Client::new("/nonexistent/agentos.sock"),
            port: 4317,
            session: "test-session".into(),
            layout: None,
            voice: None,
        }
    }

    fn request(method: &str, path: &str, body: Value) -> Request {
        Request {
            method: method.into(),
            target: path.into(),
            headers: BTreeMap::from([
                ("host".into(), "127.0.0.1:4317".into()),
                ("x-agentos-session".into(), "test-session".into()),
            ]),
            body: body.to_string().into_bytes(),
        }
    }

    #[test]
    fn origin_host_and_session_are_independently_required() {
        let config = config();
        let mut request = request("POST", "/api/intent", json!({"text":"记一下 茶"}));
        assert!(validate_boundary(&request, &config).is_ok());
        request
            .headers
            .insert("host".into(), "evil.example:4317".into());
        assert_eq!(
            validate_boundary(&request, &config).unwrap_err().code,
            "invalid_host"
        );
        request
            .headers
            .insert("host".into(), "127.0.0.1:4317".into());
        request
            .headers
            .insert("origin".into(), "https://evil.example".into());
        assert_eq!(
            validate_boundary(&request, &config).unwrap_err().code,
            "invalid_origin"
        );
        request.headers.remove("origin");
        request.headers.remove("x-agentos-session");
        assert_eq!(
            validate_boundary(&request, &config).unwrap_err().code,
            "session_required"
        );
    }

    #[test]
    fn ambiguous_framing_is_rejected() {
        assert!(parse_headers(b"POST /api/intent HTTP/1.1\r\nHost: localhost:4317\r\nContent-Length: 1\r\nContent-Length: 2").is_err());
        assert!(parse_headers(
            b"POST /api/intent HTTP/1.1\r\nHost: localhost:4317\r\nTransfer-Encoding: chunked"
        )
        .is_err());
        assert!(parse_headers(
            b"GET http://evil.example/api/status HTTP/1.1\r\nHost: localhost:4317"
        )
        .is_err());
    }

    #[test]
    fn reads_are_side_effect_free_queries_that_never_become_intents() {
        assert!(matches!(
            command(&request("GET", "/api/read/notes.list", Value::Null)).unwrap(),
            Command::Read { capability, args }
                if capability.as_str() == "notes.list" && args == json!({})
        ));
        for (method, path) in [
            ("GET", "/api/read/"),
            ("GET", "/api/read/notes.list?text=x"),
            ("GET", "/api/read/notes/list"),
            ("GET", "/api/read/notes%2Eclear"),
            ("GET", "/api/read/笔记"),
            ("POST", "/api/read/notes.list"),
        ] {
            assert!(
                command(&request(method, path, json!({}))).is_err(),
                "{method} {path}"
            );
        }
        let long = format!("/api/read/{}", "n".repeat(129));
        assert!(command(&request("GET", &long, Value::Null)).is_err());
    }

    #[test]
    fn bounded_queries_and_exact_approval_tokens_map_to_protocol() {
        assert!(matches!(
            command(&request("GET", "/api/tasks?state=unhandled", Value::Null)).unwrap(),
            Command::List {
                state: Some(TaskState::Unhandled),
                ..
            }
        ));
        assert!(command(&request("GET", "/api/tasks?limit=51", Value::Null)).is_err());
        assert!(command(&request("GET", "/api/tasks?limit=1&limit=2", Value::Null)).is_err());
        assert!(command(&request("GET", "/api/tasks?before=task-0", Value::Null)).is_err());
        assert!(
            matches!(command(&request("POST", "/api/tasks/task-9/approve", json!({"step":2,"confirmation":"opaque"}))).unwrap(),
            Command::Approve { id: TaskId(9), step: 2, confirmation, pin: None } if confirmation == "opaque")
        );
        assert!(command(&request(
            "POST",
            "/api/tasks/9/approve",
            json!({"step":0,"confirmation":"opaque"})
        ))
        .is_err());
        assert!(command(&request(
            "POST",
            "/api/intent",
            json!({"text":" ","speaker":"fake"})
        ))
        .is_err());
    }

    async fn exchange(raw: Vec<u8>) -> String {
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).await.unwrap();
        let address = listener.local_addr().unwrap();
        let mut config = config();
        config.port = address.port();
        let raw = String::from_utf8(raw)
            .unwrap()
            .replace("PORT", &address.port().to_string());
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            handle_connection(stream, &config).await.unwrap();
        });
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(raw.as_bytes()).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        serving.await.unwrap();
        String::from_utf8(response).unwrap()
    }

    use std::net::Ipv4Addr;

    #[tokio::test]
    async fn socket_boundary_rejects_cross_site_non_json_and_oversized_bodies() {
        let response = exchange(b"POST /api/intent HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nOrigin: https://evil.example\r\nContent-Length: 2\r\n\r\n{}".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 403"));
        let response = exchange(b"POST /api/intent HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nX-AgentOS-Session: test-session\r\nContent-Type: text/plain\r\nContent-Length: 2\r\n\r\n{}".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 415"));
        let response = exchange(b"POST /api/intent HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nX-AgentOS-Session: test-session\r\nContent-Type: application/json\r\nContent-Length: 999999\r\n\r\n".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 413"));
    }

    #[tokio::test]
    async fn daemon_offline_is_distinct_from_missing_local_session() {
        let response =
            exchange(b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\r\n".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 401"));
        assert!(response.contains("session_required"));
        let response = exchange(b"GET /api/status HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nX-AgentOS-Session: test-session\r\n\r\n".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 503"));
        assert!(response.contains("connection_error"));
        assert!(response.contains("\"outcome_unknown\":false"));
        assert!(!response.contains("Access-Control-Allow-Origin"));
    }

    #[tokio::test]
    async fn voice_routes_require_local_session_and_keep_configuration_server_owned() {
        let response =
            exchange(b"GET /api/voice/status HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\r\n".to_vec())
                .await;
        assert!(response.starts_with("HTTP/1.1 401"));
        let response=exchange(b"GET /api/voice/status HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nX-AgentOS-Session: test-session\r\n\r\n".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("\"enabled\":false"));
        assert!(response.contains("media-src 'self' blob:"));
        let bad = route(
            request(
                "POST",
                "/api/voice/session",
                json!({"sdp":"v=0\r\nm=audio 9 RTP/AVP 111","model":"browser-override"}),
            ),
            &config(),
        )
        .await;
        assert_eq!(bad.status, 400);
        let unavailable = route(
            request(
                "POST",
                "/api/voice/session",
                json!({"sdp":"v=0\r\nm=audio 9 RTP/AVP 111"}),
            ),
            &config(),
        )
        .await;
        assert_eq!(unavailable.status, 503);
        assert!(String::from_utf8(unavailable.body)
            .unwrap()
            .contains("voice_unavailable"));
    }

    #[tokio::test]
    async fn layout_routes_share_session_boundary_and_disabled_state_is_explicit() {
        let response =
            exchange(b"GET /api/layout/status HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\n\r\n".to_vec())
                .await;
        assert!(response.starts_with("HTTP/1.1 401"));
        let response=exchange(b"GET /api/layout/status HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nX-AgentOS-Session: test-session\r\n\r\n".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 200"));
        assert!(response.contains("\"enabled\":false"));
        assert!(response.contains("\"protocol_version\":1"));
        let response=exchange(b"POST /api/layout HTTP/1.1\r\nHost: 127.0.0.1:PORT\r\nX-AgentOS-Session: test-session\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}".to_vec()).await;
        assert!(response.starts_with("HTTP/1.1 400"));
        assert!(response.contains("invalid_layout_context"));
        let body = json!({"kind":"overview","intent":"看看空间","selected_task_id":null,"local":{"notes":[],"focus":{"active":false,"duration":1500,"goal":""},"viewport":{"width":1440,"height":900},"time":{"iso":"2026-11-10T20:00:00Z","timezone":"UTC"},"recent_intents":[]}});
        let response = route(request("POST", "/api/layout", body), &config()).await;
        assert_eq!(response.status, 503);
        assert!(String::from_utf8(response.body)
            .unwrap()
            .contains("layout_unavailable"));
    }

    #[tokio::test]
    async fn a_dropped_mutation_acknowledgement_is_never_replayed() {
        use std::os::unix::fs::PermissionsExt;
        use tokio::io::BufReader;
        use tokio::net::UnixListener;

        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path =
            std::env::temp_dir().join(format!("aos-shell-{}-{nonce}.sock", std::process::id()));
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let serving = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let frame = agentos_ipc::read_frame(&mut reader).await.unwrap().unwrap();
            let request: agentos_ipc::Request = serde_json::from_slice(&frame).unwrap();
            assert!(matches!(request.command, Command::Submit { text } if text == "记一下 茶"));
            drop(reader);
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let mut config = config();
        config.client = Client::new(&path);
        let response = route(
            request("POST", "/api/intent", json!({"text":"记一下 茶"})),
            &config,
        )
        .await;
        assert_eq!(response.status, 503);
        let body: Value = serde_json::from_slice(&response.body).unwrap();
        assert_eq!(body["error"]["outcome_unknown"], true);
        serving.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }
}
