//! `agentos-shell`: the AgentOS graphical shell.
//!
//! One full-screen window renders the whole desktop (menu bar, intent bar,
//! conversation panel, activity, dock, confirmation sheets) from `ui/index.html`
//! in a WebKit view. Drawing everything in one surface lets the glass materials
//! blur the wallpaper without compositor support.
//!
//! The shell holds no authority of its own: it forwards intents, approvals
//! and the talk button to the task service `agentos-taskd` over protocol v1
//! (see `kernel.rs`) and renders what comes back. Every action still goes
//! through the service's policy engine and audit log.
//!
//! Usage: `agentos-shell [--task-socket PATH] [--windowed | --fullscreen]`.

use std::path::PathBuf;

use agentos_ipc::{ListenAction, SpeakMode};
use serde_json::{json, Value};
use tao::dpi::LogicalSize;
use tao::event::{Event, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::window::{Fullscreen, WindowBuilder};
use wry::WebViewBuilder;

mod kernel;
mod setup;

const UI: &str = include_str!("../ui/index.html");

/// Internal event: the page can receive events now.
const READY: &str = "__ready";

const HELP: &str = "\
用法：agentos-shell [--task-socket 路径] [--windowed | --fullscreen]

  --task-socket 路径
                  连接任务服务 agentos-taskd 的指定 socket（或 AGENTOS_TASK_SOCKET；
                  默认是状态目录下的 control.sock）
  --windowed      以 1440×900 的窗口运行，便于开发
  --fullscreen    无边框全屏
  --help, -h      查看帮助
  --version, -V   查看版本

需要先启动任务服务 agentos-taskd。";

/// What the command line asks for other than opening the desktop: text to
/// print before exiting, or a usage error. Checked before any window exists,
/// so `--help` or a mistyped flag never takes over the screen.
fn command_line(args: impl IntoIterator<Item = String>) -> Result<Option<String>, String> {
    let mut args = args.into_iter();
    while let Some(name) = args.next() {
        match name.as_str() {
            "--help" | "-h" => {
                return Ok(Some(format!(
                    "AgentOS v{} · 图形桌面\n\n{HELP}",
                    env!("CARGO_PKG_VERSION")
                )))
            }
            "--version" | "-V" => {
                return Ok(Some(format!("agentos-shell {}", env!("CARGO_PKG_VERSION"))))
            }
            "--task-socket" => {
                if args.next().is_none_or(|value| value.starts_with("--")) {
                    return Err(format!("{name} 缺少路径，使用 --help 查看用法"));
                }
            }
            // `--taskd` chose the task service when there were two backends;
            // still accepted so older session scripts keep working.
            "--windowed" | "--fullscreen" | "--taskd" => {}
            _ => return Err(format!("未知参数：{name}，使用 --help 查看用法")),
        }
    }
    Ok(None)
}

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

/// The task service's socket: `--task-socket`, `AGENTOS_TASK_SOCKET`, or
/// the socket in the default state directory.
fn task_socket() -> PathBuf {
    if let Some(path) = arg("--task-socket") {
        return path.into();
    }
    if let Some(path) = std::env::var_os("AGENTOS_TASK_SOCKET").filter(|p| !p.is_empty()) {
        return path.into();
    }
    match agentos_ipc::default_state_dir() {
        Ok(state) => agentos_ipc::default_socket_path(&state),
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(2);
        }
    }
}

/// What the page asks of the butler, rebuilt from known fields only.
fn butler_request(v: &Value) -> Option<kernel::Request> {
    match v.get("type").and_then(Value::as_str)? {
        "intent" => Some(kernel::Request::Intent(
            v.get("text")?.as_str()?.to_string(),
        )),
        "confirm" => Some(kernel::Request::Confirm {
            approved: v.get("approved")?.as_bool()?,
            task: v.get("task").and_then(Value::as_u64),
            pin: pin_of(v),
        }),
        "listen" => Some(kernel::Request::Listen(
            match v.get("action")?.as_str()? {
                "start" => ListenAction::Start,
                "stop" => ListenAction::Stop,
                "cancel" => ListenAction::Cancel,
                _ => return None,
            },
        )),
        "speech" => {
            let mode = match v.get("mode").and_then(Value::as_str) {
                Some(mode) => Some(SpeakMode::parse(mode)?),
                None => None,
            };
            let stop = v.get("action").and_then(Value::as_str) == Some("stop");
            (mode.is_some() || stop).then_some(kernel::Request::Speech { mode, stop })
        }
        _ => None,
    }
}

/// A PIN as the confirmation sheet sends it: digits only, bounded.
fn pin_of(v: &Value) -> Option<String> {
    v.get("pin")
        .and_then(Value::as_str)
        .filter(|pin| (1..=12).contains(&pin.len()) && pin.bytes().all(|b| b.is_ascii_digit()))
        .map(str::to_string)
}

/// System actions the UI may ask for, run by the `agentos-system` helper that
/// the AgentOS image installs.
const SYSTEM_ACTIONS: [&str; 6] = [
    "terminal", "setup", "update", "install", "reboot", "poweroff",
];

/// `AGENTOS_SYSTEM`, else `/usr/local/bin/agentos-system` if it exists.
fn system_helper() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AGENTOS_SYSTEM") {
        return (!p.is_empty()).then(|| p.into());
    }
    let p = PathBuf::from("/usr/local/bin/agentos-system");
    p.exists().then_some(p)
}

/// The action named by a `{"type":"system","action":…}` request, if allowed.
fn system_action(v: &Value) -> Option<&'static str> {
    let a = v.get("action")?.as_str()?;
    SYSTEM_ACTIONS.iter().copied().find(|x| *x == a)
}

fn run_system(action: &str) {
    match system_helper() {
        Some(helper) => {
            if let Err(e) = std::process::Command::new(&helper).arg(action).spawn() {
                eprintln!("无法执行 {} {action}：{e}", helper.display());
            }
        }
        // Outside the AgentOS image only the terminal is available.
        None if action == "terminal" => launch_terminal(),
        None => eprintln!("没有 agentos-system，忽略 {action}"),
    }
}

/// Facts about this machine the UI needs for its system menu.
fn host_info() -> Value {
    json!({
        "type": "host",
        "managed": system_helper().is_some(),
        "installed": std::path::Path::new("/etc/agentos/installed").exists(),
        // The graphical setup assistant opens by itself until setup is done.
        "setup": setup::helper().is_some(),
        "setup_done": std::path::Path::new("/etc/agentos/setup-done").exists(),
    })
}

/// Once the page shows a connected daemon, leave a marker for the boot
/// self-check: `$XDG_RUNTIME_DIR/agentos-shell.ready`.
fn mark_ready(hello: &Value) {
    if let Ok(dir) = std::env::var("XDG_RUNTIME_DIR") {
        let planner = hello.get("planner").and_then(Value::as_str).unwrap_or("");
        let _ = std::fs::write(PathBuf::from(dir).join("agentos-shell.ready"), planner);
    }
}

/// What the UI may ask for. Only intents, confirmation answers and voice
/// controls (talk button, read-aloud mode) reach the task service, rebuilt
/// from known fields; `system` runs a fixed set of actions and `setup` the
/// setup assistant's checked operations (see `setup.rs`). The shell never
/// touches the microphone itself: the service records, so it can audit and
/// show every use.
fn handle_ipc(
    body: &str,
    proxy: &EventLoopProxy<Value>,
    tasks: &tokio::sync::mpsc::UnboundedSender<kernel::Request>,
) {
    let Ok(v) = serde_json::from_str::<Value>(body) else {
        return;
    };
    if let Some(request) = butler_request(&v) {
        let _ = tasks.send(request);
        return;
    }
    match v.get("type").and_then(Value::as_str) {
        // The page finished loading: start delivering service events.
        Some("ready") => {
            let _ = proxy.send_event(json!({"type": READY}));
        }
        Some("system") => {
            if let Some(action) = system_action(&v) {
                run_system(action);
            }
        }
        // Setup assistant: checked here, run off the UI thread (Wi-Fi scans
        // and connection tests take seconds), answered as a `setup` event.
        Some("setup") => {
            let id = v.get("id").cloned().unwrap_or(Value::Null);
            let answer = match setup::job(&v) {
                Ok(job) => {
                    let proxy = proxy.clone();
                    std::thread::spawn(move || {
                        let _ = proxy.send_event(setup::run(&id, job));
                    });
                    return;
                }
                Err(e) => json!({"type": "setup", "id": id, "ok": false, "error": e}),
            };
            let _ = proxy.send_event(answer);
        }
        _ => {}
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
    match command_line(std::env::args().skip(1)) {
        Ok(Some(text)) => {
            println!("{text}");
            return Ok(());
        }
        Ok(None) => {}
        Err(message) => {
            eprintln!("{message}");
            std::process::exit(2);
        }
    }
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

    let socket = task_socket();
    let (tasks, receiver) = tokio::sync::mpsc::unbounded_channel();
    let proxy = event_loop.create_proxy();
    eprintln!("agentos-shell：连接任务服务 {}", socket.display());
    std::thread::spawn(move || {
        kernel::run(socket, receiver, move |event| {
            proxy.send_event(event).is_ok()
        })
    });

    let ipc_proxy = event_loop.create_proxy();
    let builder = WebViewBuilder::new()
        .with_html(UI)
        .with_background_color((11, 15, 28, 255))
        .with_ipc_handler(move |req| handle_ipc(req.body(), &ipc_proxy, &tasks));

    #[cfg(target_os = "linux")]
    let webview = {
        use tao::platform::unix::WindowExtUnix;
        use wry::WebViewBuilderExtUnix;
        builder.build_gtk(window.default_vbox().expect("GTK vbox"))?
    };
    #[cfg(not(target_os = "linux"))]
    let webview = builder.build(&window)?;

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
                    } else if v.get("type").and_then(Value::as_str) == Some("hello") {
                        mark_ready(v);
                    }
                };
                if v.get("type").and_then(Value::as_str) == Some(READY) {
                    if queued.is_none() {
                        return; // the page reloaded; nothing is queued any more
                    }
                    deliver(&host_info());
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn help_version_and_mistakes_never_open_a_window() {
        let parse = |args: &[&str]| command_line(args.iter().map(|a| a.to_string()));
        assert!(parse(&["--help"])
            .unwrap()
            .unwrap()
            .contains("用法：agentos-shell"));
        assert_eq!(
            parse(&["-V"]).unwrap().unwrap(),
            format!("agentos-shell {}", env!("CARGO_PKG_VERSION"))
        );
        assert!(parse(&["--windowed", "--help"]).unwrap().is_some());
        for args in [
            vec![],
            vec!["--windowed"],
            vec!["--task-socket", "/tmp/control.sock", "--fullscreen"],
        ] {
            assert_eq!(parse(&args), Ok(None), "{args:?}");
        }
        for args in [
            vec!["--window"],
            vec!["--task-socket"],
            vec!["--task-socket", "--windowed"],
            // The agentosd socket is gone; a script still passing it is told.
            vec!["--socket", "/tmp/agentos.sock"],
        ] {
            assert!(parse(&args).is_err(), "{args:?}");
        }
    }

    #[test]
    fn only_known_system_actions_are_accepted() {
        let a = |s: &str| system_action(&serde_json::from_str(s).unwrap());
        assert_eq!(
            a(r#"{"type":"system","action":"poweroff"}"#),
            Some("poweroff")
        );
        assert_eq!(
            a(r#"{"type":"system","action":"terminal"}"#),
            Some("terminal")
        );
        assert_eq!(a(r#"{"type":"system","action":"rm -rf /"}"#), None);
        assert_eq!(a(r#"{"type":"system"}"#), None);
    }

    #[test]
    fn the_old_backend_flag_is_still_accepted() {
        let parse = |args: &[&str]| command_line(args.iter().map(|a| a.to_string()));
        assert_eq!(parse(&["--taskd"]), Ok(None));
    }

    #[test]
    fn the_talk_button_and_read_aloud_become_known_requests_only() {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(
            butler_request(&v(r#"{"type":"listen","action":"start"}"#)),
            Some(kernel::Request::Listen(ListenAction::Start))
        );
        assert_eq!(
            butler_request(&v(r#"{"type":"speech","mode":"always"}"#)),
            Some(kernel::Request::Speech {
                mode: Some(SpeakMode::Always),
                stop: false
            })
        );
        assert_eq!(
            butler_request(&v(r#"{"type":"speech","action":"stop"}"#)),
            Some(kernel::Request::Speech {
                mode: None,
                stop: true
            })
        );
        for other in [
            r#"{"type":"listen","action":"forever"}"#,
            r#"{"type":"speech","mode":"loud"}"#,
            r#"{"type":"speech"}"#,
        ] {
            assert_eq!(butler_request(&v(other)), None, "{other}");
        }
    }

    #[test]
    fn approvals_keep_their_task_and_pin_and_nothing_else_from_the_page() {
        let v = |s: &str| serde_json::from_str::<Value>(s).unwrap();
        assert_eq!(
            butler_request(&v(
                r#"{"type":"confirm","approved":true,"task":3,"pin":"1234","token":"forged"}"#
            )),
            Some(kernel::Request::Confirm {
                approved: true,
                task: Some(3),
                pin: Some("1234".into())
            })
        );
        assert_eq!(
            butler_request(&v(r#"{"type":"confirm","approved":false}"#)),
            Some(kernel::Request::Confirm {
                approved: false,
                task: None,
                pin: None
            })
        );
        assert_eq!(
            butler_request(&v(r#"{"type":"intent","text":"记一下 牛奶"}"#)),
            Some(kernel::Request::Intent("记一下 牛奶".into()))
        );
        // Not a PIN: too long, not digits, empty, or not a string.
        for pin in [
            json!("12345678901234"),
            json!("12ab"),
            json!(""),
            json!(1234),
        ] {
            let body = json!({"type": "confirm", "approved": true, "pin": pin});
            assert_eq!(pin_of(&body), None, "{body}");
        }
        for other in [
            r#"{"type":"system","action":"reboot"}"#,
            r#"{"type":"confirm"}"#,
            r#"{"type":"intent"}"#,
        ] {
            assert_eq!(butler_request(&v(other)), None, "{other}");
        }
    }
}
