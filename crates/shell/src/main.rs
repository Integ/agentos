//! `agentos-shell`: the AgentOS graphical shell.
//!
//! One full-screen window renders the whole desktop (menu bar, intent bar,
//! conversation panel, activity, dock, confirmation sheets) from `ui/index.html`
//! in a WebKit view. Drawing everything in one surface lets the glass materials
//! blur the wallpaper without compositor support.
//!
//! The shell holds no authority of its own: it forwards intents and
//! confirmation answers to `agentosd` over its Unix socket (see
//! `agentosd::shell_api`) and renders the events that come back. Every action
//! still goes through the daemon's policy engine and audit log.
//!
//! Usage: `agentos-shell [--socket PATH] [--windowed | --fullscreen]`.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Fullscreen, WindowBuilder};
use wry::WebViewBuilder;

const UI: &str = include_str!("../ui/index.html");

/// Internal event: the page can receive events now.
const READY: &str = "__ready";

/// The daemon connection, shared by the reader thread and the IPC handler.
type Conn = Arc<Mutex<Option<UnixStream>>>;

fn arg(name: &str) -> Option<String> {
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        if a == name {
            return args.next();
        }
    }
    None
}

fn flag(name: &str) -> bool {
    std::env::args().skip(1).any(|a| a == name)
}

/// `--socket`, `AGENTOS_SOCKET`, or `$XDG_RUNTIME_DIR/agentos.sock`.
fn socket_path() -> PathBuf {
    if let Some(p) = arg("--socket").or_else(|| std::env::var("AGENTOS_SOCKET").ok()) {
        return p.into();
    }
    let dir = std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(dir).join("agentos.sock")
}

/// Keep a connection to agentosd, reconnecting every second, and forward each
/// event line to the UI.
fn daemon_loop(path: PathBuf, conn: Conn, proxy: EventLoopProxy<Value>) {
    loop {
        if let Ok(stream) = UnixStream::connect(&path) {
            if let Ok(reader) = stream.try_clone() {
                *conn.lock().unwrap() = Some(stream);
                for line in BufReader::new(reader).lines() {
                    let Ok(line) = line else { break };
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        if proxy.send_event(v).is_err() {
                            return;
                        }
                    }
                }
                *conn.lock().unwrap() = None;
                let gone = json!({"type": "connection", "connected": false});
                if proxy.send_event(gone).is_err() {
                    return;
                }
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// What the UI may ask for. Only intents, confirmation answers and voice
/// controls (talk button, read-aloud mode) reach the daemon, rebuilt from known
/// fields; `launch` opens a fixed set of programs. The shell never touches the
/// microphone itself: agentosd records, so it can audit and show every use.
fn handle_ipc(body: &str, conn: &Conn, proxy: &EventLoopProxy<Value>) {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return;
    };
    let out = match v.get("type").and_then(Value::as_str) {
        Some("intent") => match v.get("text").and_then(Value::as_str) {
            Some(t) => json!({"type": "intent", "text": t}),
            None => return,
        },
        Some("confirm") => match v.get("approved").and_then(Value::as_bool) {
            Some(a) => json!({"type": "confirm", "approved": a}),
            None => return,
        },
        Some("listen") => match v.get("action").and_then(Value::as_str) {
            Some(a @ ("start" | "stop" | "cancel")) => json!({"type": "listen", "action": a}),
            _ => return,
        },
        Some("speech") => match (
            v.get("mode").and_then(Value::as_str),
            v.get("action").and_then(Value::as_str),
        ) {
            (Some(m @ ("auto" | "always" | "off")), _) => json!({"type": "speech", "mode": m}),
            (_, Some("stop")) => json!({"type": "speech", "action": "stop"}),
            _ => return,
        },
        // The page finished loading: start delivering daemon events.
        Some("ready") => {
            let _ = proxy.send_event(json!({"type": READY}));
            return;
        }
        Some("launch") => {
            if v.get("app").and_then(Value::as_str) == Some("terminal") {
                launch_terminal();
            }
            return;
        }
        _ => return,
    };
    let mut guard = conn.lock().unwrap();
    if let Some(stream) = guard.as_mut() {
        let mut line = out.to_string();
        line.push('\n');
        if stream.write_all(line.as_bytes()).is_err() {
            *guard = None;
        }
    }
}

/// `AGENTOS_TERMINAL` (a program name, no arguments) or `foot`.
fn launch_terminal() {
    let program = std::env::var("AGENTOS_TERMINAL").unwrap_or_else(|_| "foot".into());
    if let Err(e) = std::process::Command::new(&program).spawn() {
        eprintln!("无法启动终端 {program}：{e}");
    }
}

fn main() -> wry::Result<()> {
    let path = socket_path();
    let event_loop = EventLoopBuilder::<Value>::with_user_event().build();

    let mut wb = WindowBuilder::new().with_title("AgentOS");
    if flag("--windowed") {
        wb = wb.with_inner_size(LogicalSize::new(1440.0, 900.0));
    } else if flag("--fullscreen") {
        wb = wb
            .with_decorations(false)
            .with_fullscreen(Some(Fullscreen::Borderless(None)));
    } else {
        // Under sway the shell is the one tiled window; other programs float above it.
        wb = wb.with_decorations(false).with_maximized(true);
    }
    let window = wb.build(&event_loop).expect("无法创建窗口");
    if !flag("--windowed") {
        // Without a window manager (or before it maximizes us) still cover the screen.
        if let Some(m) = window.current_monitor() {
            window.set_inner_size(m.size());
        }
    }

    let conn: Conn = Arc::new(Mutex::new(None));
    {
        let (conn, proxy, path) = (conn.clone(), event_loop.create_proxy(), path.clone());
        std::thread::spawn(move || daemon_loop(path, conn, proxy));
    }

    let (ipc_conn, ipc_proxy) = (conn.clone(), event_loop.create_proxy());
    let builder = WebViewBuilder::new()
        .with_html(UI)
        .with_background_color((11, 15, 28, 255))
        .with_ipc_handler(move |req| handle_ipc(req.body(), &ipc_conn, &ipc_proxy));

    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().expect("GTK vbox"))?
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;

    eprintln!("agentos-shell：连接 {}", path.display());
    // Events that arrive before the page has loaded wait here.
    let mut queued: Option<Vec<Value>> = Some(Vec::new());
    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::Wait;
        match event {
            Event::UserEvent(v) => {
                let deliver = |v: &Value| {
                    // `v` is parsed JSON, so its serialization is a safe JS literal.
                    let js = format!("window.agentos && window.agentos.receive({v})");
                    if let Err(e) = webview.evaluate_script(&js) {
                        eprintln!("UI 更新失败：{e}");
                    }
                };
                if v.get("type").and_then(Value::as_str) == Some(READY) {
                    for v in queued.take().unwrap_or_default() {
                        deliver(&v);
                    }
                } else if let Some(q) = queued.as_mut() {
                    q.push(v);
                } else {
                    deliver(&v);
                }
            }
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => *control_flow = ControlFlow::Exit,
            _ => {}
        }
    });
}
