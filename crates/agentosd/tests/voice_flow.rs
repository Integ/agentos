//! Push-to-talk and spoken replies, with fake recorder, recognizer and player.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, AgentId, Expression, ExpressionKind, IntentId, Output, PerceptKind, Urgency,
};
use agentosd::shell_api;
use agentosd::voice::{
    SpeakMode, SpeechOutput, SpeechToText, TextToSpeech, Voice, VoiceConfig, VoiceControl,
    VoiceResult, MIC_DEVICE,
};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};

struct FakeStt {
    got: Mutex<Option<Vec<u8>>>,
}

#[async_trait]
impl SpeechToText for FakeStt {
    fn label(&self) -> String {
        "fake".into()
    }
    fn destination(&self) -> String {
        "stt.example".into()
    }
    async fn transcribe(&self, wav: Vec<u8>) -> VoiceResult<String> {
        *self.got.lock().unwrap() = Some(wav);
        Ok(" 记一下 买牛奶 ".into())
    }
}

struct FakeTts;

#[async_trait]
impl TextToSpeech for FakeTts {
    fn label(&self) -> String {
        "fake".into()
    }
    async fn synthesize(&self, text: &str) -> VoiceResult<Vec<u8>> {
        Ok(format!("[{text}]").into_bytes())
    }
}

fn temp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "agentosd-voice-{name}-{}-{}",
        std::process::id(),
        now_ms()
    ))
}

fn sh(script: &str) -> Vec<String> {
    vec!["sh".into(), "-c".into(), script.into()]
}

async fn next_of(rx: &mut broadcast::Receiver<Value>, ty: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let v = rx.recv().await.unwrap();
            if v["type"] == ty {
                return v;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("no {ty} event"))
}

async fn mic_state(rx: &mut broadcast::Receiver<Value>) -> String {
    next_of(rx, "voice").await["mic"]
        .as_str()
        .unwrap()
        .to_string()
}

#[tokio::test]
async fn push_to_talk_turns_speech_into_a_percept() {
    let audit_path = temp("ptt.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
    let bus = shell_api::event_bus();
    let mut events = bus.subscribe();
    let (tx, mut rx) = mpsc::channel(4);
    let stt = Arc::new(FakeStt {
        got: Mutex::new(None),
    });
    let voice = Voice::new(
        VoiceConfig {
            stt: Some(stt.clone()),
            tts: None,
            // 0.5 s of loud noise, then keep the "device" open like arecord does.
            recorder: sh("head -c 16000 /dev/urandom; exec sleep 30"),
            player: sh("cat > /dev/null"),
            speak: SpeakMode::Auto,
        },
        bus.clone(),
        tx,
        audit.clone(),
    );
    assert_eq!(voice.describe()["destination"], "stt.example");

    voice.control(VoiceControl::ListenStart).await;
    assert_eq!(mic_state(&mut events).await, "listening");
    let level = next_of(&mut events, "voice_level").await;
    assert!(level["level"].as_f64().unwrap() > 0.1);
    tokio::time::sleep(Duration::from_millis(400)).await;

    voice.control(VoiceControl::ListenStop).await;
    assert_eq!(mic_state(&mut events).await, "transcribing");
    assert_eq!(
        next_of(&mut events, "transcript").await["text"],
        "记一下 买牛奶"
    );

    let p = tokio::time::timeout(Duration::from_secs(5), rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert_eq!(p.device.as_str(), MIC_DEVICE);
    assert!(matches!(p.kind, PerceptKind::Text { ref text, .. } if text == "记一下 买牛奶"));

    let wav = stt.got.lock().unwrap().clone().unwrap();
    assert_eq!(&wav[..4], b"RIFF");
    assert_eq!(wav.len(), 44 + 16000);

    let used: Vec<Value> = AuditLog::replay(&audit_path)
        .unwrap()
        .into_iter()
        .filter_map(|e| match e.event {
            AuditEvent::DeviceUsed { device, detail, .. } if device.as_str() == MIC_DEVICE => {
                Some(detail)
            }
            _ => None,
        })
        .collect();
    assert_eq!(used.len(), 1);
    assert_eq!(used[0]["sent_to"], "stt.example");
    assert_eq!(used[0]["duration_ms"], 500);
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn silence_and_cancel_send_nothing() {
    let audit_path = temp("silence.jsonl");
    let audit = Arc::new(AuditLog::open(&audit_path).unwrap());
    let bus = shell_api::event_bus();
    let mut events = bus.subscribe();
    let (tx, mut rx) = mpsc::channel(4);
    let stt = Arc::new(FakeStt {
        got: Mutex::new(None),
    });
    let voice = Voice::new(
        VoiceConfig {
            stt: Some(stt.clone()),
            tts: None,
            recorder: sh("head -c 16000 /dev/zero; exec sleep 30"),
            player: sh("cat > /dev/null"),
            speak: SpeakMode::Auto,
        },
        bus.clone(),
        tx,
        audit,
    );

    voice.control(VoiceControl::ListenStart).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    voice.control(VoiceControl::ListenStop).await;
    let n = next_of(&mut events, "voice_notice").await;
    assert!(n["text"].as_str().unwrap().contains("没有听到声音"));

    voice.control(VoiceControl::ListenStart).await;
    tokio::time::sleep(Duration::from_millis(400)).await;
    voice.control(VoiceControl::ListenCancel).await;
    tokio::time::sleep(Duration::from_millis(100)).await;

    assert!(rx.try_recv().is_err());
    assert!(stt.got.lock().unwrap().is_none());
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn missing_recognizer_is_reported() {
    let audit_path = temp("nostt.jsonl");
    let bus = shell_api::event_bus();
    let mut events = bus.subscribe();
    let (tx, _rx) = mpsc::channel(4);
    let voice = Voice::new(
        VoiceConfig {
            stt: None,
            tts: None,
            recorder: sh("exit 1"),
            player: sh("exit 1"),
            speak: SpeakMode::Auto,
        },
        bus.clone(),
        tx,
        Arc::new(AuditLog::open(&audit_path).unwrap()),
    );
    voice.control(VoiceControl::ListenStart).await;
    let n = next_of(&mut events, "voice_notice").await;
    assert!(n["text"].as_str().unwrap().contains("agentos-setup"));
    let _ = std::fs::remove_file(&audit_path);
}

fn expr(intent: u64, kind: ExpressionKind, text: &str) -> Expression {
    Expression {
        from: AgentId::new("butler"),
        intent: Some(IntentId(intent)),
        kind,
        urgency: Urgency::Normal,
        text: text.into(),
    }
}

async fn wait_for(path: &std::path::Path, want: &str) -> String {
    for _ in 0..50 {
        let s = std::fs::read_to_string(path).unwrap_or_default();
        if s.contains(want) {
            return s;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    std::fs::read_to_string(path).unwrap_or_default()
}

#[tokio::test]
async fn replies_to_voice_intents_are_spoken() {
    let audit_path = temp("tts.jsonl");
    let played = temp("played.txt");
    let bus = shell_api::event_bus();
    let mut events = bus.subscribe();
    let (tx, _rx) = mpsc::channel(4);
    let voice = Voice::new(
        VoiceConfig {
            stt: None,
            tts: Some(Arc::new(FakeTts)),
            recorder: sh("exit 1"),
            player: sh(&format!("cat >> {}", played.display())),
            speak: SpeakMode::Auto,
        },
        bus.clone(),
        tx,
        Arc::new(AuditLog::open(&audit_path).unwrap()),
    );
    let out = SpeechOutput::new(voice.clone());
    voice.mark_voice_intent(IntentId(1));

    out.express(&expr(1, ExpressionKind::Plan, "1. notes.append"))
        .await
        .unwrap();
    out.express(&expr(2, ExpressionKind::Report, "打字问的"))
        .await
        .unwrap();
    out.express(&expr(1, ExpressionKind::Report, "已记下：买牛奶"))
        .await
        .unwrap();
    let v = next_of(&mut events, "voice").await;
    assert_eq!(v["speaking"], true);
    let s = wait_for(&played, "买牛奶").await;
    assert_eq!(s, "[已记下：买牛奶]");

    // always: typed intents are spoken too; off: nothing.
    voice
        .control(VoiceControl::SetSpeakMode(SpeakMode::Always))
        .await;
    out.express(&expr(3, ExpressionKind::Error, "出错了"))
        .await
        .unwrap();
    let s = wait_for(&played, "出错了").await;
    assert!(s.ends_with("[出错了]"), "{s}");
    voice
        .control(VoiceControl::SetSpeakMode(SpeakMode::Off))
        .await;
    out.express(&expr(1, ExpressionKind::Report, "不该说"))
        .await
        .unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!std::fs::read_to_string(&played).unwrap().contains("不该说"));

    let _ = std::fs::remove_file(&played);
    let _ = std::fs::remove_file(&audit_path);
}

#[tokio::test]
async fn stop_speaking_kills_playback() {
    let audit_path = temp("hush.jsonl");
    let bus = shell_api::event_bus();
    let mut events = bus.subscribe();
    let (tx, _rx) = mpsc::channel(4);
    let voice = Voice::new(
        VoiceConfig {
            stt: None,
            tts: Some(Arc::new(FakeTts)),
            recorder: sh("exit 1"),
            // A player that never finishes on its own.
            player: sh("cat > /dev/null; exec sleep 30"),
            speak: SpeakMode::Always,
        },
        bus.clone(),
        tx,
        Arc::new(AuditLog::open(&audit_path).unwrap()),
    );
    let out = SpeechOutput::new(voice.clone());
    out.express(&expr(1, ExpressionKind::Report, "很长的一段话"))
        .await
        .unwrap();
    assert_eq!(next_of(&mut events, "voice").await["speaking"], true);
    tokio::time::sleep(Duration::from_millis(100)).await;
    voice.control(VoiceControl::StopSpeaking).await;
    let v = next_of(&mut events, "voice").await;
    assert_eq!(v["speaking"], false);
    let _ = std::fs::remove_file(&audit_path);
}
