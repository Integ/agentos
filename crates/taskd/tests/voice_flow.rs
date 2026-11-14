//! Push-to-talk and read-aloud on the task service, over a real socket.
//!
//! The recorder, recognizer, synthesizer and player are small shell
//! commands, so nothing here touches a microphone, a speaker or a network.

#![cfg(unix)]

use std::fs;
use std::io;
use std::os::unix::fs::DirBuilderExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{AgentId, RiskLevel};
use agentos_ipc::{
    read_frame, write_frame, Command, Event, EventKind, ListenAction, MicState, Reply, Request,
    Response, ResponseBody, SpeakMode, VERSION,
};
use agentos_planner::RulePlanner;
use agentos_policy::PolicyEngine;
use agentos_runtime::{TaskId, TaskState, TaskStore};
use agentos_services::expression::ExpressionHub;
use agentos_services::registry::CapabilityRegistry;
use agentos_taskd::builtin;
use agentos_taskd::butler::{grant_manifest, Butler};
use agentos_taskd::mic_led::MicLed;
use agentos_taskd::service::Service;
use agentos_taskd::voice::{CommandStt, CommandTts, SpeechOutput, Voice, VoiceConfig};
use tokio::io::BufReader;
use tokio::net::unix::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::UnixStream;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio::time::{sleep, timeout};

static SEQUENCE: AtomicU64 = AtomicU64::new(0);

struct TempDir(PathBuf);
impl TempDir {
    fn new() -> Self {
        // Short, so the socket path stays under the Unix limit on macOS.
        let base = if cfg!(target_os = "macos") {
            PathBuf::from("/private/tmp")
        } else {
            std::env::temp_dir()
        };
        let path = base.join(format!(
            "aos-voice-{}-{}",
            std::process::id(),
            SEQUENCE.fetch_add(1, Ordering::Relaxed)
        ));
        fs::DirBuilder::new().mode(0o700).create(&path).unwrap();
        Self(path)
    }
}
impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

struct Harness {
    dir: TempDir,
    path: PathBuf,
    audit: PathBuf,
    tasks: Arc<TaskStore>,
    _shutdown: oneshot::Sender<()>,
    server: JoinHandle<io::Result<()>>,
}

impl Harness {
    /// A service whose microphone "hears" a loud tone and whose recognizer
    /// "hears" `words`. Read-aloud audio is the reply's text, appended to
    /// `spoken.txt`, so a test can see what was read.
    async fn start(words: &str) -> Self {
        let dir = TempDir::new();
        let path = dir.0.join("control.sock");
        let audit_path = dir.0.join("audit.jsonl");
        let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
        let spoken = dir.0.join("spoken.txt");
        let config = VoiceConfig {
            stt: Some(Arc::new(CommandStt::new(vec![
                "sh".into(),
                "-c".into(),
                format!("test -s \"$0\" && echo ' {words} '"),
                "{wav}".into(),
            ]))),
            tts: Some(Arc::new(CommandTts::new(sh("cat")))),
            // Loud samples (0x7f7f), then wait to be stopped like arecord.
            recorder: sh("head -c 32000 /dev/zero | tr '\\0' '\\177'; sleep 30"),
            player: sh(&format!("cat >> '{}'", spoken.display())),
            speak: SpeakMode::Auto,
        };
        let voice = Voice::with_led(config, audit.clone(), MicLed::disabled());

        let tasks = Arc::new(TaskStore::memory());
        let mut registry = CapabilityRegistry::new();
        builtin::register_all(&mut registry);
        let mut policy = PolicyEngine::new();
        let butler_id = AgentId::new("butler");
        let names: Vec<_> = registry.specs().into_iter().map(|s| s.name).collect();
        grant_manifest(&mut policy, &butler_id, &names);
        let mut expr = ExpressionHub::new(audit.clone());
        expr.add_output(Arc::new(SpeechOutput::new(voice.clone())));
        let butler = Butler::with_store(
            butler_id,
            registry,
            policy,
            Box::new(RulePlanner),
            audit.clone(),
            expr,
            tasks.clone(),
        );
        let server = Service::with_butler(butler, audit, None)
            .with_voice(voice)
            .bind(&path)
            .await
            .unwrap();
        let (shutdown, receiver) = oneshot::channel();
        let server = tokio::spawn(server.run_until(async {
            let _ = receiver.await;
        }));
        Self {
            dir,
            path,
            audit: audit_path,
            tasks,
            _shutdown: shutdown,
            server,
        }
    }

    async fn connect(&self, caller: Option<&str>) -> Connection {
        let stream = UnixStream::connect(&self.path).await.unwrap();
        let (reader, writer) = stream.into_split();
        Connection {
            reader: BufReader::new(reader),
            writer,
            next: 0,
            caller: caller.map(str::to_string),
        }
    }

    async fn wait(&self, id: TaskId, state: TaskState) {
        timeout(Duration::from_secs(5), async {
            while self.tasks.get(id).map(|t| t.state) != Some(state) {
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("task state deadline");
    }

    fn spoken(&self) -> String {
        fs::read_to_string(self.dir.0.join("spoken.txt")).unwrap_or_default()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.server.abort();
    }
}

struct Connection {
    reader: BufReader<OwnedReadHalf>,
    writer: OwnedWriteHalf,
    next: u64,
    caller: Option<String>,
}

impl Connection {
    async fn request(&mut self, command: Command) -> Response {
        self.next += 1;
        let request = Request {
            version: VERSION,
            id: self.next,
            command,
            caller: self.caller.clone(),
        };
        write_frame(&mut self.writer, &request).await.unwrap();
        let frame = timeout(Duration::from_secs(5), read_frame(&mut self.reader))
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        serde_json::from_slice(&frame).unwrap()
    }

    async fn ok(&mut self, command: Command) -> Reply {
        match self.request(command).await.body {
            ResponseBody::Ok { data } => data,
            ResponseBody::Error { error } => panic!("{}: {}", error.code, error.message),
        }
    }

    async fn error(&mut self, command: Command) -> String {
        match self.request(command).await.body {
            ResponseBody::Error { error } => error.code,
            ResponseBody::Ok { data } => panic!("expected an error, got {data:?}"),
        }
    }

    async fn event(&mut self) -> Event {
        let frame = timeout(Duration::from_secs(5), read_frame(&mut self.reader))
            .await
            .expect("event deadline")
            .unwrap()
            .unwrap();
        serde_json::from_slice(&frame).unwrap()
    }

    /// Watch, with or without voice events; the initial resync is consumed.
    async fn watch(&mut self, voice: bool) {
        let Reply::Watching { .. } = self.ok(Command::Watch { after: None, voice }).await else {
            panic!("expected watching");
        };
        assert!(matches!(
            self.event().await.kind,
            EventKind::ResyncRequired { .. }
        ));
    }

    async fn submit(&mut self, text: &str, spoken: bool) -> TaskId {
        let Reply::Accepted { task } = self
            .ok(Command::Submit {
                text: text.into(),
                thread: None,
                spoken,
            })
            .await
        else {
            panic!("expected accepted");
        };
        task.id
    }
}

async fn talk(client: &mut Connection) -> u64 {
    let Reply::Listening { recording } = client
        .ok(Command::Listen {
            action: ListenAction::Start,
        })
        .await
    else {
        panic!("expected listening");
    };
    // Longer than an accidental tap.
    sleep(Duration::from_millis(400)).await;
    let Reply::Listening { recording: stopped } = client
        .ok(Command::Listen {
            action: ListenAction::Stop,
        })
        .await
    else {
        panic!("expected listening");
    };
    assert_eq!(stopped, recording);
    recording
}

#[tokio::test]
async fn what_was_said_comes_back_to_watchers_that_asked_and_the_recording_is_audited() {
    let harness = Harness::start("记一下 买牛奶").await;
    let mut voice_watch = harness.connect(None).await;
    voice_watch.watch(true).await;
    let mut task_watch = harness.connect(None).await;
    task_watch.watch(false).await;
    let mut client = harness.connect(None).await;

    let recording = talk(&mut client).await;
    // Guards the microphone indicator: the state follows the recording.
    let mut states = Vec::new();
    let text = loop {
        match voice_watch.event().await.kind {
            EventKind::VoiceState { mic, .. } => states.push(mic),
            EventKind::Transcript {
                recording: heard,
                text,
            } => {
                assert_eq!(heard, recording);
                break text;
            }
            EventKind::VoiceLevel { level } => assert!((0.0..=1.0).contains(&level)),
            other => panic!("unexpected {other:?}"),
        }
    };
    assert_eq!(text, "记一下 买牛奶");
    assert_eq!(states, vec![MicState::Listening, MicState::Transcribing]);

    // The words are the client's to submit; nothing became a task by itself.
    assert!(harness.tasks.list().is_empty());

    // Guards protocol compatibility: a watch that did not ask for voice
    // events sees none, only the task change that comes next.
    let id = client.submit(&text, true).await;
    assert!(matches!(
        task_watch.event().await.kind,
        EventKind::TaskChanged { id: changed } if changed == id
    ));

    // Guards "every use of the microphone is audited": how long, and where
    // the audio went, before it left.
    let used: Vec<_> = AuditLog::replay(&harness.audit)
        .unwrap()
        .into_iter()
        .filter_map(|record| match record.event {
            AuditEvent::DeviceUsed { device, detail, .. } => Some((device, detail)),
            _ => None,
        })
        .collect();
    assert_eq!(used.len(), 1);
    assert_eq!(used[0].0.as_str(), "mic");
    assert_eq!(used[0].1["sent_to"], "本机");
    assert!(used[0].1["duration_ms"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn a_cancelled_recording_is_thrown_away_and_only_one_records_at_a_time() {
    let harness = Harness::start("不该出现").await;
    let mut voice_watch = harness.connect(None).await;
    voice_watch.watch(true).await;
    let mut client = harness.connect(None).await;
    let Reply::Listening { recording } = client
        .ok(Command::Listen {
            action: ListenAction::Start,
        })
        .await
    else {
        panic!("expected listening");
    };
    assert_eq!(
        client
            .error(Command::Listen {
                action: ListenAction::Start
            })
            .await,
        "busy"
    );
    sleep(Duration::from_millis(400)).await;
    let Reply::Listening {
        recording: cancelled,
    } = client
        .ok(Command::Listen {
            action: ListenAction::Cancel,
        })
        .await
    else {
        panic!("expected listening");
    };
    assert_eq!(cancelled, recording);
    assert_eq!(
        client
            .error(Command::Listen {
                action: ListenAction::Stop
            })
            .await,
        "not_listening"
    );
    // Back to idle without a transcript.
    loop {
        match voice_watch.event().await.kind {
            EventKind::VoiceState {
                mic: MicState::Idle,
                ..
            } => break,
            EventKind::Transcript { .. } => panic!("a cancelled recording was transcribed"),
            _ => {}
        }
    }
    let Reply::Voice { voice } = client.ok(Command::Voice {}).await else {
        panic!("expected voice");
    };
    assert_eq!(voice.mic, MicState::Idle);
    assert!(voice.input && voice.output);
    assert_eq!(voice.destination.as_deref(), Some("本机"));
}

#[tokio::test]
async fn replies_are_read_aloud_in_auto_mode_only_for_what_was_spoken() {
    let harness = Harness::start("unused").await;
    let mut client = harness.connect(None).await;
    let typed = client.submit("记一下 打字的", false).await;
    harness.wait(typed, TaskState::Succeeded).await;
    let spoken = client.submit("记一下 说出来的", true).await;
    harness.wait(spoken, TaskState::Succeeded).await;
    timeout(Duration::from_secs(5), async {
        while !harness.spoken().contains("说出来的") {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the spoken intent's reply is read");
    assert!(!harness.spoken().contains("打字的"));

    // `off` reads nothing, `always` reads typed intents too.
    client
        .ok(Command::Speech {
            mode: Some(SpeakMode::Always),
            stop: false,
        })
        .await;
    let typed = client.submit("记一下 总是读", false).await;
    harness.wait(typed, TaskState::Succeeded).await;
    timeout(Duration::from_secs(5), async {
        while !harness.spoken().contains("总是读") {
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("always reads typed intents");
}

#[tokio::test]
async fn an_outside_caller_can_neither_listen_nor_speak() {
    let harness = Harness::start("unused").await;
    let mut user = harness.connect(None).await;
    user.ok(Command::RegisterCaller {
        id: "helper".into(),
        name: "测试助手".into(),
        capabilities: vec!["notes.list".into()],
        max_risk: RiskLevel::ReadOnly,
        pin: None,
    })
    .await;
    let mut caller = harness.connect(Some("helper")).await;
    // Guards "the microphone is the human's at this machine".
    for command in [
        Command::Listen {
            action: ListenAction::Start,
        },
        Command::Voice {},
        Command::Speech {
            mode: Some(SpeakMode::Always),
            stop: false,
        },
        Command::Watch {
            after: None,
            voice: true,
        },
    ] {
        assert_eq!(caller.error(command).await, "forbidden");
    }
}

#[tokio::test]
async fn without_voice_configured_the_service_says_so_instead_of_recording() {
    let dir = TempDir::new();
    let path = dir.0.join("control.sock");
    let audit = Arc::new(AuditLog::open(dir.0.join("audit.jsonl")).unwrap());
    let mut registry = CapabilityRegistry::new();
    builtin::register_all(&mut registry);
    let service = Service::new(
        Arc::new(TaskStore::memory()),
        audit,
        registry,
        PolicyEngine::new(),
        Box::new(RulePlanner),
    );
    let server = service.bind(&path).await.unwrap();
    let handle = tokio::spawn(server.run_until(std::future::pending::<()>()));
    let stream = UnixStream::connect(&path).await.unwrap();
    let (reader, writer) = stream.into_split();
    let mut client = Connection {
        reader: BufReader::new(reader),
        writer,
        next: 0,
        caller: None,
    };
    let Reply::Voice { voice } = client.ok(Command::Voice {}).await else {
        panic!("expected voice");
    };
    assert!(!voice.input && !voice.output);
    assert_eq!(
        client
            .error(Command::Listen {
                action: ListenAction::Start
            })
            .await,
        "voice_unavailable"
    );
    handle.abort();
}
