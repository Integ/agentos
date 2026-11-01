//! Voice: push-to-talk speech input and spoken replies.
//!
//! Two devices share one [`Voice`] hub:
//! - the **microphone** (a perception device): while the human holds the talk
//!   button, a recorder process streams 16 kHz mono PCM; on release the audio
//!   goes to a [`SpeechToText`] backend and the transcript becomes a text
//!   percept from device [`MIC_DEVICE`], exactly like a typed line;
//! - the **speaker** (an expression device, [`SpeechOutput`]): reports, errors
//!   and confirmation requests are synthesized by a [`TextToSpeech`] backend
//!   and played in order. Plans are never read aloud.
//!
//! Backends are pluggable: any OpenAI-compatible `/audio/transcriptions` and
//! `/audio/speech` endpoint, or local commands (whisper.cpp, Piper, ...).
//!
//! Privacy rules:
//! - The microphone only records while the talk button is held (60 s max) and
//!   never listens for a wake word. Every recording is announced to shells
//!   (`voice` events, so the menu bar shows a microphone indicator) and written
//!   to the audit log with its length and where the audio was sent.
//! - Audio leaves the machine only when the human chose a cloud recognizer
//!   (`AGENTOS_STT=openai`); there is no silent default.
//! - A voice transcript can cancel a pending step but cannot approve one that
//!   needs identity verification (L3); see the butler.

use std::collections::VecDeque;
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use agentos_audit::{AuditEvent, AuditLog};
use agentos_core::{
    now_ms, DeviceId, DeviceInfo, Expression, ExpressionKind, IntentId, Modality, Output,
    OutputError, Percept, PerceptKind, PrivacyClass,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Notify};

use crate::mic_led::MicLed;
use crate::shell_api::EventBus;

/// Device id of the microphone; intents from it are voice intents.
pub const MIC_DEVICE: &str = "mic";
/// Recorder output format: signed 16-bit little-endian, mono, 16 kHz.
pub const SAMPLE_RATE: u32 = 16_000;
/// The longest recording; the talk button is released for the human after this.
pub const MAX_RECORDING: Duration = Duration::from_secs(60);
/// Shorter recordings are treated as an accidental tap.
const MIN_RECORDING_MS: u64 = 300;
/// Below this peak level (about -40 dBFS) the recording is treated as silence.
const SILENCE_PEAK: f32 = 0.01;
/// Longer replies are cut when spoken; the full text stays on screen.
const MAX_SPOKEN_CHARS: usize = 240;

pub type VoiceResult<T> = Result<T, String>;

// ---------------------------------------------------------------------------
// Audio helpers
// ---------------------------------------------------------------------------

/// Wrap raw s16le PCM in a WAV container.
pub fn wav_from_pcm(pcm: &[u8], sample_rate: u32, channels: u16) -> Vec<u8> {
    let data_len = pcm.len() as u32;
    let byte_rate = sample_rate * channels as u32 * 2;
    let mut w = Vec::with_capacity(44 + pcm.len());
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data_len).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes()); // PCM
    w.extend_from_slice(&channels.to_le_bytes());
    w.extend_from_slice(&sample_rate.to_le_bytes());
    w.extend_from_slice(&byte_rate.to_le_bytes());
    w.extend_from_slice(&(channels * 2).to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data_len.to_le_bytes());
    w.extend_from_slice(pcm);
    w
}

/// Peak and RMS level (0..1) of a chunk of s16le samples.
pub fn levels(pcm: &[u8]) -> (f32, f32) {
    let mut peak = 0f32;
    let mut sum = 0f64;
    let mut n = 0usize;
    for s in pcm.chunks_exact(2) {
        let v = i16::from_le_bytes([s[0], s[1]]) as f32 / 32768.0;
        peak = peak.max(v.abs());
        sum += (v as f64) * (v as f64);
        n += 1;
    }
    let rms = if n == 0 {
        0.0
    } else {
        (sum / n as f64).sqrt() as f32
    };
    (peak, rms)
}

fn pcm_duration_ms(pcm: &[u8]) -> u64 {
    pcm.len() as u64 * 1000 / (SAMPLE_RATE as u64 * 2)
}

/// Split a command line on whitespace. No shell is involved; use a wrapper
/// script for anything fancier.
pub fn split_command(s: &str) -> Option<Vec<String>> {
    let v: Vec<String> = s.split_whitespace().map(str::to_string).collect();
    (!v.is_empty()).then_some(v)
}

/// `whisper-cli` for `/usr/local/bin/whisper-cli -m ...`.
fn program_name(argv: &[String]) -> &str {
    argv[0].rsplit('/').next().unwrap_or(&argv[0])
}

fn command(argv: &[String]) -> Command {
    let mut c = Command::new(&argv[0]);
    c.args(&argv[1..]).kill_on_drop(true);
    c
}

/// Make a reply suitable for listening: no plan outlines or typing hints, and
/// not too long.
pub fn spoken_text(e: &Expression) -> Option<String> {
    let text = match e.kind {
        ExpressionKind::Plan => return None,
        ExpressionKind::Report | ExpressionKind::Error => e.text.trim().to_string(),
        // "cap：描述。原因：…。回复 y 确认，其他任意内容取消。" → keep the description.
        ExpressionKind::AskConfirm => {
            let what = e.text.split('。').next().unwrap_or_default();
            let what = what.split_once('：').map(|(_, d)| d).unwrap_or(what);
            format!("这一步需要你确认：{what}。请看屏幕。")
        }
    };
    if text.is_empty() {
        return None;
    }
    if text.chars().count() <= MAX_SPOKEN_CHARS {
        return Some(text);
    }
    let cut: String = text.chars().take(MAX_SPOKEN_CHARS).collect();
    Some(format!("{cut}……完整内容在屏幕上。"))
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

#[async_trait]
pub trait SpeechToText: Send + Sync {
    /// Human-readable name, including where the audio goes.
    fn label(&self) -> String;
    /// Whether the audio leaves this machine, and to which host.
    fn destination(&self) -> String;
    async fn transcribe(&self, wav: Vec<u8>) -> VoiceResult<String>;
}

#[async_trait]
pub trait TextToSpeech: Send + Sync {
    fn label(&self) -> String;
    /// Audio in a format the player understands (WAV by default).
    async fn synthesize(&self, text: &str) -> VoiceResult<Vec<u8>>;
}

/// Endpoint settings shared by the OpenAI-compatible speech backends.
#[derive(Debug, Clone)]
pub struct OpenAiVoiceConfig {
    pub base_url: String,
    pub api_key: Option<String>,
}

impl OpenAiVoiceConfig {
    /// `AGENTOS_VOICE_BASE_URL` / `AGENTOS_VOICE_API_KEY`, falling back to
    /// `OPENAI_BASE_URL` / `OPENAI_API_KEY`. A separate endpoint is useful when
    /// the planner runs on a server without audio APIs (DeepSeek, ...).
    pub fn from_env() -> Option<Self> {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.is_empty());
        let base_url = get("AGENTOS_VOICE_BASE_URL").or_else(|| get("OPENAI_BASE_URL"));
        let api_key = get("AGENTOS_VOICE_API_KEY").or_else(|| get("OPENAI_API_KEY"));
        if base_url.is_none() && api_key.is_none() {
            return None;
        }
        Some(Self {
            base_url: base_url.unwrap_or_else(|| crate::openai_planner::DEFAULT_BASE_URL.into()),
            api_key,
        })
    }

    fn host(&self) -> String {
        let rest = self
            .base_url
            .split_once("://")
            .map(|(_, r)| r)
            .unwrap_or(&self.base_url);
        rest.split('/').next().unwrap_or(rest).to_string()
    }

    fn url(&self, path: &str) -> String {
        format!("{}/{path}", self.base_url.trim_end_matches('/'))
    }
}

fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap_or_default()
}

fn api_error(status: reqwest::StatusCode, body: &[u8]) -> String {
    let msg = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| String::from_utf8_lossy(body).chars().take(200).collect());
    format!("语音服务错误（HTTP {status}）：{msg}")
}

/// Build a `multipart/form-data` body: text fields, then one file.
pub fn multipart_body(
    boundary: &str,
    fields: &[(&str, &str)],
    file: (&str, &str, &str, &[u8]),
) -> Vec<u8> {
    let mut b = Vec::new();
    for (name, value) in fields {
        b.extend_from_slice(
            format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"\r\n\r\n{value}\r\n"
            )
            .as_bytes(),
        );
    }
    let (name, filename, mime, data) = file;
    b.extend_from_slice(
        format!(
            "--{boundary}\r\nContent-Disposition: form-data; name=\"{name}\"; filename=\"{filename}\"\r\nContent-Type: {mime}\r\n\r\n"
        )
        .as_bytes(),
    );
    b.extend_from_slice(data);
    b.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
    b
}

/// `POST {base}/audio/transcriptions` (OpenAI, Groq, SiliconFlow, a local
/// faster-whisper server, ...).
pub struct OpenAiStt {
    cfg: OpenAiVoiceConfig,
    model: String,
    language: Option<String>,
    http: reqwest::Client,
}

impl OpenAiStt {
    pub const DEFAULT_MODEL: &'static str = "gpt-4o-mini-transcribe";

    pub fn new(cfg: OpenAiVoiceConfig, model: String, language: Option<String>) -> Self {
        Self::with_client(cfg, model, language, http_client())
    }

    pub fn with_client(
        cfg: OpenAiVoiceConfig,
        model: String,
        language: Option<String>,
        http: reqwest::Client,
    ) -> Self {
        Self {
            cfg,
            model,
            language,
            http,
        }
    }
}

#[async_trait]
impl SpeechToText for OpenAiStt {
    fn label(&self) -> String {
        format!("{}（{}）", self.model, self.cfg.host())
    }

    fn destination(&self) -> String {
        self.cfg.host()
    }

    async fn transcribe(&self, wav: Vec<u8>) -> VoiceResult<String> {
        let boundary = format!("agentos{:x}", now_ms());
        let mut fields = vec![("model", self.model.as_str()), ("response_format", "json")];
        if let Some(lang) = &self.language {
            fields.push(("language", lang.as_str()));
            if lang.starts_with("zh") {
                // Nudges Whisper-family models towards simplified characters.
                fields.push(("prompt", "以下是普通话的句子，使用简体中文。"));
            }
        }
        let body = multipart_body(
            &boundary,
            &fields,
            ("file", "speech.wav", "audio/wav", &wav),
        );
        let mut req = self
            .http
            .post(self.cfg.url("audio/transcriptions"))
            .header(
                "content-type",
                format!("multipart/form-data; boundary={boundary}"),
            )
            .body(body);
        if let Some(key) = &self.cfg.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("无法连接语音识别服务 {}：{e}", self.cfg.host()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(api_error(status, &bytes));
        }
        let v: Value = serde_json::from_slice(&bytes)
            .map_err(|e| format!("语音识别服务返回了无法解析的响应：{e}"))?;
        Ok(v.get("text")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_string())
    }
}

/// `POST {base}/audio/speech`, asking for WAV.
pub struct OpenAiTts {
    cfg: OpenAiVoiceConfig,
    model: String,
    voice: String,
    http: reqwest::Client,
}

impl OpenAiTts {
    pub const DEFAULT_MODEL: &'static str = "gpt-4o-mini-tts";
    pub const DEFAULT_VOICE: &'static str = "alloy";

    pub fn new(cfg: OpenAiVoiceConfig, model: String, voice: String) -> Self {
        Self::with_client(cfg, model, voice, http_client())
    }

    pub fn with_client(
        cfg: OpenAiVoiceConfig,
        model: String,
        voice: String,
        http: reqwest::Client,
    ) -> Self {
        Self {
            cfg,
            model,
            voice,
            http,
        }
    }
}

#[async_trait]
impl TextToSpeech for OpenAiTts {
    fn label(&self) -> String {
        format!("{} · {}（{}）", self.model, self.voice, self.cfg.host())
    }

    async fn synthesize(&self, text: &str) -> VoiceResult<Vec<u8>> {
        let body = json!({
            "model": self.model,
            "voice": self.voice,
            "input": text,
            "response_format": "wav",
        });
        let mut req = self.http.post(self.cfg.url("audio/speech")).json(&body);
        if let Some(key) = &self.cfg.api_key {
            req = req.bearer_auth(key);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| format!("无法连接语音合成服务 {}：{e}", self.cfg.host()))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(api_error(status, &bytes));
        }
        Ok(bytes.to_vec())
    }
}

/// A local recognizer, e.g. whisper.cpp:
/// `whisper-cli -m ggml-small.bin -l zh -nt -np -f {wav}`. `{wav}` is replaced
/// by the path of a temporary WAV file; the transcript is read from stdout.
pub struct CommandStt {
    argv: Vec<String>,
}

impl CommandStt {
    pub fn new(argv: Vec<String>) -> Self {
        Self { argv }
    }
}

#[async_trait]
impl SpeechToText for CommandStt {
    fn label(&self) -> String {
        format!("{}（本机）", program_name(&self.argv))
    }

    fn destination(&self) -> String {
        "本机".into()
    }

    async fn transcribe(&self, wav: Vec<u8>) -> VoiceResult<String> {
        let path = std::env::temp_dir().join(format!(
            "agentos-speech-{}-{}.wav",
            std::process::id(),
            now_ms()
        ));
        tokio::fs::write(&path, &wav)
            .await
            .map_err(|e| format!("无法写入临时音频：{e}"))?;
        let argv: Vec<String> = self
            .argv
            .iter()
            .map(|a| a.replace("{wav}", &path.to_string_lossy()))
            .collect();
        let out = command(&argv)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|e| format!("无法运行语音识别程序 {}：{e}", argv[0]));
        let _ = tokio::fs::remove_file(&path).await;
        let out = out?;
        if !out.status.success() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "语音识别程序失败（{}）：{}",
                out.status,
                err.trim().chars().take(200).collect::<String>()
            ));
        }
        Ok(String::from_utf8_lossy(&out.stdout)
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" "))
    }
}

/// A local synthesizer reading text on stdin and writing WAV to stdout, e.g.
/// `piper --model zh_CN-huayan-medium.onnx --output_file -`.
pub struct CommandTts {
    argv: Vec<String>,
}

impl CommandTts {
    pub fn new(argv: Vec<String>) -> Self {
        Self { argv }
    }
}

#[async_trait]
impl TextToSpeech for CommandTts {
    fn label(&self) -> String {
        format!("{}（本机）", program_name(&self.argv))
    }

    async fn synthesize(&self, text: &str) -> VoiceResult<Vec<u8>> {
        let mut child = command(&self.argv)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| format!("无法运行语音合成程序 {}：{e}", self.argv[0]))?;
        let mut stdin = child.stdin.take().ok_or("无法写入语音合成程序")?;
        let input = format!("{text}\n");
        let writer = tokio::spawn(async move {
            let _ = stdin.write_all(input.as_bytes()).await;
        });
        let out = child.wait_with_output().await.map_err(|e| e.to_string())?;
        let _ = writer.await;
        if !out.status.success() || out.stdout.is_empty() {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!(
                "语音合成程序失败（{}）：{}",
                out.status,
                err.trim().chars().take(200).collect::<String>()
            ));
        }
        Ok(out.stdout)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

/// When replies are read aloud.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpeakMode {
    /// Only replies to intents the human spoke.
    Auto,
    Always,
    Off,
}

impl SpeakMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "auto" => Some(Self::Auto),
            "always" => Some(Self::Always),
            "off" => Some(Self::Off),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Auto => "auto",
            Self::Always => "always",
            Self::Off => "off",
        }
    }
}

pub struct VoiceConfig {
    pub stt: Option<Arc<dyn SpeechToText>>,
    pub tts: Option<Arc<dyn TextToSpeech>>,
    /// Recorder writing raw s16le mono 16 kHz PCM to stdout.
    pub recorder: Vec<String>,
    /// Player reading the synthesized audio from stdin.
    pub player: Vec<String>,
    pub speak: SpeakMode,
}

impl VoiceConfig {
    pub const DEFAULT_RECORDER: &'static str = "arecord -q -t raw -f S16_LE -r 16000 -c 1";
    pub const DEFAULT_PLAYER: &'static str = "aplay -q -";

    /// Read the configuration from the environment. Returns the config and
    /// notes for the console (misconfigurations are reported, never fatal).
    ///
    /// - `AGENTOS_STT=openai|command|off`. `openai` uploads recordings to the
    ///   voice endpoint (model `AGENTOS_STT_MODEL`, language
    ///   `AGENTOS_STT_LANGUAGE`, default `zh`). `command` runs
    ///   `AGENTOS_STT_CMD`. Default: `command` if `AGENTOS_STT_CMD` is set,
    ///   otherwise off: audio never goes to the cloud unless chosen.
    /// - `AGENTOS_TTS=openai|command|off` with `AGENTOS_TTS_MODEL`,
    ///   `AGENTOS_TTS_VOICE` or `AGENTOS_TTS_CMD`; same default rule.
    /// - `AGENTOS_MIC_CMD`, `AGENTOS_PLAY_CMD`: recorder and player commands.
    /// - `AGENTOS_SPEAK=auto|always|off`: when to read replies aloud.
    pub fn from_env() -> (Self, Vec<String>) {
        let get = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
        let mut notes = Vec::new();
        let endpoint = OpenAiVoiceConfig::from_env;

        let stt_cmd = get("AGENTOS_STT_CMD").and_then(|c| split_command(&c));
        let stt_choice = get("AGENTOS_STT").or_else(|| stt_cmd.as_ref().map(|_| "command".into()));
        let stt: Option<Arc<dyn SpeechToText>> = match stt_choice.as_deref() {
            None | Some("off") => None,
            Some("openai") => match endpoint() {
                Some(cfg) => Some(Arc::new(OpenAiStt::new(
                    cfg,
                    get("AGENTOS_STT_MODEL").unwrap_or_else(|| OpenAiStt::DEFAULT_MODEL.into()),
                    Some(get("AGENTOS_STT_LANGUAGE").unwrap_or_else(|| "zh".into()))
                        .filter(|l| l != "auto"),
                ))),
                None => {
                    notes.push("语音识别：AGENTOS_STT=openai 需要 AGENTOS_VOICE_API_KEY 或 OPENAI_API_KEY，已关闭".into());
                    None
                }
            },
            Some("command") => match stt_cmd {
                Some(argv) => Some(Arc::new(CommandStt::new(argv))),
                None => {
                    notes.push("语音识别：AGENTOS_STT=command 需要 AGENTOS_STT_CMD，已关闭".into());
                    None
                }
            },
            Some(other) => {
                notes.push(format!(
                    "语音识别：未知的 AGENTOS_STT={other}（可选 openai、command、off），已关闭"
                ));
                None
            }
        };

        let tts_cmd = get("AGENTOS_TTS_CMD").and_then(|c| split_command(&c));
        let tts_choice = get("AGENTOS_TTS").or_else(|| tts_cmd.as_ref().map(|_| "command".into()));
        let tts: Option<Arc<dyn TextToSpeech>> = match tts_choice.as_deref() {
            None | Some("off") => None,
            Some("openai") => match endpoint() {
                Some(cfg) => Some(Arc::new(OpenAiTts::new(
                    cfg,
                    get("AGENTOS_TTS_MODEL").unwrap_or_else(|| OpenAiTts::DEFAULT_MODEL.into()),
                    get("AGENTOS_TTS_VOICE").unwrap_or_else(|| OpenAiTts::DEFAULT_VOICE.into()),
                ))),
                None => {
                    notes.push("语音播报：AGENTOS_TTS=openai 需要 AGENTOS_VOICE_API_KEY 或 OPENAI_API_KEY，已关闭".into());
                    None
                }
            },
            Some("command") => match tts_cmd {
                Some(argv) => Some(Arc::new(CommandTts::new(argv))),
                None => {
                    notes.push("语音播报：AGENTOS_TTS=command 需要 AGENTOS_TTS_CMD，已关闭".into());
                    None
                }
            },
            Some(other) => {
                notes.push(format!(
                    "语音播报：未知的 AGENTOS_TTS={other}（可选 openai、command、off），已关闭"
                ));
                None
            }
        };

        let speak = match get("AGENTOS_SPEAK") {
            None => SpeakMode::Auto,
            Some(s) => SpeakMode::parse(&s).unwrap_or_else(|| {
                notes.push(format!(
                    "未知的 AGENTOS_SPEAK={s}（可选 auto、always、off），使用 auto"
                ));
                SpeakMode::Auto
            }),
        };
        let recorder = get("AGENTOS_MIC_CMD")
            .and_then(|c| split_command(&c))
            .unwrap_or_else(|| split_command(Self::DEFAULT_RECORDER).unwrap());
        let player = get("AGENTOS_PLAY_CMD")
            .and_then(|c| split_command(&c))
            .unwrap_or_else(|| split_command(Self::DEFAULT_PLAYER).unwrap());
        (
            Self {
                stt,
                tts,
                recorder,
                player,
                speak,
            },
            notes,
        )
    }
}

// ---------------------------------------------------------------------------
// The voice hub
// ---------------------------------------------------------------------------

/// Requests from a shell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VoiceControl {
    /// Talk button pressed.
    ListenStart,
    /// Talk button released: transcribe and send.
    ListenStop,
    /// Talk button released elsewhere / Esc: throw the recording away.
    ListenCancel,
    SetSpeakMode(SpeakMode),
    /// Stop reading aloud and drop queued replies.
    StopSpeaking,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MicState {
    Idle,
    Listening,
    Transcribing,
}

impl MicState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Listening => "listening",
            Self::Transcribing => "transcribing",
        }
    }
}

struct Recording {
    child: Child,
    reader: tokio::task::JoinHandle<(Vec<u8>, f32)>,
    started: Instant,
    /// Distinguishes this recording from later ones (for the 60 s limit).
    serial: u64,
}

struct Utterance {
    text: String,
    generation: u64,
}

pub struct Voice {
    stt: Option<Arc<dyn SpeechToText>>,
    tts: Option<Arc<dyn TextToSpeech>>,
    recorder: Vec<String>,
    player: Vec<String>,
    bus: EventBus,
    percepts: mpsc::Sender<Percept>,
    audit: Arc<AuditLog>,

    mic: Mutex<MicState>,
    /// Caps Lock LED, lit while recording.
    led: MicLed,
    recording: tokio::sync::Mutex<Option<Recording>>,
    serial: AtomicU64,

    speak: Mutex<SpeakMode>,
    voice_intents: Mutex<VecDeque<IntentId>>,
    speaking: AtomicBool,
    queue: mpsc::UnboundedSender<Utterance>,
    generation: AtomicU64,
    hush: Notify,
}

impl Voice {
    /// Create the hub and start its speech worker. Must run inside Tokio.
    pub fn new(
        cfg: VoiceConfig,
        bus: EventBus,
        percepts: mpsc::Sender<Percept>,
        audit: Arc<AuditLog>,
    ) -> Arc<Self> {
        let (queue, rx) = mpsc::unbounded_channel();
        let voice = Arc::new(Self {
            stt: cfg.stt,
            tts: cfg.tts,
            recorder: cfg.recorder,
            player: cfg.player,
            bus,
            percepts,
            audit,
            mic: Mutex::new(MicState::Idle),
            led: if cfg!(test) {
                MicLed::disabled()
            } else {
                MicLed::from_env()
            },
            recording: tokio::sync::Mutex::new(None),
            serial: AtomicU64::new(0),
            speak: Mutex::new(cfg.speak),
            voice_intents: Mutex::new(VecDeque::new()),
            speaking: AtomicBool::new(false),
            queue,
            generation: AtomicU64::new(0),
            hush: Notify::new(),
        });
        if voice.tts.is_some() {
            tokio::spawn(Self::speech_worker(Arc::downgrade(&voice), rx));
        }
        voice
    }

    pub fn input_enabled(&self) -> bool {
        self.stt.is_some()
    }

    pub fn output_enabled(&self) -> bool {
        self.tts.is_some()
    }

    pub fn speak_mode(&self) -> SpeakMode {
        *self.speak.lock().unwrap()
    }

    /// For the shell's `hello`, and the console banner.
    pub fn describe(&self) -> Value {
        json!({
            "input": self.input_enabled(),
            "input_label": self.stt.as_ref().map(|s| s.label()),
            "destination": self.stt.as_ref().map(|s| s.destination()),
            "output": self.output_enabled(),
            "output_label": self.tts.as_ref().map(|t| t.label()),
            "speak": self.speak_mode().as_str(),
        })
    }

    /// Remember that `id` came from the microphone, so its replies are spoken
    /// in `auto` mode.
    pub fn mark_voice_intent(&self, id: IntentId) {
        let mut q = self.voice_intents.lock().unwrap();
        q.push_back(id);
        while q.len() > 32 {
            q.pop_front();
        }
    }

    fn is_voice_intent(&self, id: Option<IntentId>) -> bool {
        id.is_some_and(|id| self.voice_intents.lock().unwrap().contains(&id))
    }

    fn emit(&self, v: Value) {
        let _ = self.bus.send(v);
    }

    fn emit_state(&self) {
        let mic = *self.mic.lock().unwrap();
        self.emit(json!({
            "type": "voice",
            "mic": mic.as_str(),
            "speaking": self.speaking.load(Ordering::SeqCst),
            "speak": self.speak_mode().as_str(),
        }));
    }

    fn notice(&self, level: &str, text: impl Into<String>) {
        let text = text.into();
        eprintln!("语音：{text}");
        self.emit(json!({"type": "voice_notice", "level": level, "text": text}));
    }

    fn set_mic(&self, s: MicState) {
        *self.mic.lock().unwrap() = s;
        self.led.set(s == MicState::Listening);
        self.emit_state();
    }

    pub async fn control(self: &Arc<Self>, c: VoiceControl) {
        match c {
            VoiceControl::ListenStart => self.listen_start().await,
            VoiceControl::ListenStop => self.listen_stop(true).await,
            VoiceControl::ListenCancel => self.listen_stop(false).await,
            VoiceControl::SetSpeakMode(m) => {
                *self.speak.lock().unwrap() = m;
                if m == SpeakMode::Off {
                    self.stop_speaking();
                }
                self.emit_state();
            }
            VoiceControl::StopSpeaking => self.stop_speaking(),
        }
    }

    async fn listen_start(self: &Arc<Self>) {
        if self.stt.is_none() {
            self.notice(
                "error",
                "还没有配置语音识别。运行 agentos-setup，或设置 AGENTOS_STT。",
            );
            return;
        }
        let mut rec = self.recording.lock().await;
        if rec.is_some() || *self.mic.lock().unwrap() != MicState::Idle {
            return;
        }
        // Barge-in: the human talking stops the butler talking.
        self.stop_speaking();

        let mut child = match command(&self.recorder)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                self.notice(
                    "error",
                    format!("无法打开麦克风（{}）：{e}", self.recorder[0]),
                );
                return;
            }
        };
        let Some(mut stdout) = child.stdout.take() else {
            return;
        };
        let bus = self.bus.clone();
        let reader = tokio::spawn(async move {
            let mut pcm = Vec::new();
            let mut peak = 0f32;
            // 100 ms of audio per level update.
            let mut buf = vec![0u8; (SAMPLE_RATE / 10 * 2) as usize];
            let limit = (SAMPLE_RATE as u64 * 2 * MAX_RECORDING.as_secs()) as usize;
            loop {
                match stdout.read(&mut buf).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        // Keep sample alignment if a read ends mid-sample.
                        pcm.extend_from_slice(&buf[..n]);
                        let (p, rms) = levels(&buf[..n & !1]);
                        peak = peak.max(p);
                        let _ = bus.send(json!({
                            "type": "voice_level",
                            "level": (rms * 4.0).min(1.0),
                        }));
                        if pcm.len() >= limit {
                            break;
                        }
                    }
                }
            }
            pcm.truncate(pcm.len() & !1);
            (pcm, peak)
        });
        let serial = self.serial.fetch_add(1, Ordering::SeqCst) + 1;
        *rec = Some(Recording {
            child,
            reader,
            started: Instant::now(),
            serial,
        });
        drop(rec);
        self.set_mic(MicState::Listening);

        // Release the button for the human after the limit.
        let me = Arc::downgrade(self);
        tokio::spawn(async move {
            tokio::time::sleep(MAX_RECORDING).await;
            if let Some(v) = me.upgrade() {
                let current = v.recording.lock().await.as_ref().map(|r| r.serial);
                if current == Some(serial) {
                    v.notice("info", "已录满 60 秒，自动发送");
                    v.listen_stop(true).await;
                }
            }
        });
    }

    async fn listen_stop(self: &Arc<Self>, send: bool) {
        let Some(mut rec) = self.recording.lock().await.take() else {
            return;
        };
        let _ = rec.child.start_kill();
        let _ = rec.child.wait().await;
        let (pcm, peak) = rec.reader.await.unwrap_or_default();
        let held_ms = rec.started.elapsed().as_millis() as u64;
        let audio_ms = pcm_duration_ms(&pcm);
        let stt = self.stt.clone().expect("recording implies a recognizer");

        if !send || held_ms < MIN_RECORDING_MS {
            self.audit_mic(audio_ms, "本机（已丢弃）");
            self.set_mic(MicState::Idle);
            return;
        }
        if pcm.is_empty() {
            self.audit_mic(0, "本机（已丢弃）");
            self.set_mic(MicState::Idle);
            self.notice(
                "error",
                format!("麦克风没有声音数据。检查录音设备，或用 AGENTOS_MIC_CMD 指定录音命令（当前：{}）。", self.recorder.join(" ")),
            );
            return;
        }
        if peak < SILENCE_PEAK {
            self.audit_mic(audio_ms, "本机（静音，已丢弃）");
            self.set_mic(MicState::Idle);
            self.notice("info", "没有听到声音，麦克风可能被静音了");
            return;
        }

        self.set_mic(MicState::Transcribing);
        self.audit_mic(audio_ms, &stt.destination());
        let wav = wav_from_pcm(&pcm, SAMPLE_RATE, 1);
        let me = self.clone();
        // Transcribe off the control path so the shell stays responsive.
        tokio::spawn(async move {
            let result = stt.transcribe(wav).await;
            me.set_mic(MicState::Idle);
            match result {
                Ok(text) if !text.trim().is_empty() => {
                    let text = text.trim().to_string();
                    me.emit(json!({"type": "transcript", "text": text}));
                    let _ = me
                        .percepts
                        .send(Percept {
                            device: DeviceId::new(MIC_DEVICE),
                            at_ms: now_ms(),
                            kind: PerceptKind::Text {
                                text,
                                speaker: None,
                            },
                        })
                        .await;
                }
                Ok(_) => me.notice("info", "没听清，请再说一次"),
                Err(e) => me.notice("error", format!("语音识别失败：{e}")),
            }
        });
    }

    fn audit_mic(&self, audio_ms: u64, sent_to: &str) {
        let ev = AuditEvent::DeviceUsed {
            device: DeviceId::new(MIC_DEVICE),
            action: "record".into(),
            detail: json!({"duration_ms": audio_ms, "sent_to": sent_to}),
        };
        if let Err(e) = self.audit.append(ev) {
            eprintln!("audit write failed: {e}");
        }
    }

    /// Queue `text` to be read aloud.
    fn say(&self, text: String) {
        let generation = self.generation.load(Ordering::SeqCst);
        let _ = self.queue.send(Utterance { text, generation });
    }

    pub fn stop_speaking(&self) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.hush.notify_waiters();
    }

    fn set_speaking(&self, on: bool) {
        if self.speaking.swap(on, Ordering::SeqCst) != on {
            self.emit_state();
        }
    }

    async fn speech_worker(me: std::sync::Weak<Self>, mut rx: mpsc::UnboundedReceiver<Utterance>) {
        while let Some(u) = rx.recv().await {
            let Some(v) = me.upgrade() else { return };
            // Replies queued before a "stop speaking" are dropped.
            if u.generation == v.generation.load(Ordering::SeqCst) {
                v.set_speaking(true);
                if let Err(e) = v.speak_one(&u).await {
                    v.notice("error", format!("语音播报失败：{e}"));
                }
            }
            if rx.is_empty() {
                v.set_speaking(false);
            }
        }
    }

    async fn speak_one(&self, u: &Utterance) -> VoiceResult<()> {
        let tts = self.tts.clone().ok_or("没有配置语音合成")?;
        let stale = || u.generation != self.generation.load(Ordering::SeqCst);
        let hushed = self.hush.notified();
        tokio::pin!(hushed);
        let audio = tokio::select! {
            a = tts.synthesize(&u.text) => a?,
            _ = &mut hushed => return Ok(()),
        };
        if stale() {
            return Ok(());
        }
        let mut child = command(&self.player)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .map_err(|e| format!("无法运行播放程序 {}：{e}", self.player[0]))?;
        let mut stdin = child.stdin.take().ok_or("无法写入播放程序")?;
        let play = async {
            // The player may exit early (closed pipe); that is not an error here.
            let _ = stdin.write_all(&audio).await;
            drop(stdin);
            child.wait().await
        };
        tokio::select! {
            _ = play => {}
            _ = &mut hushed => {}
        }
        // Kills the player if it is still running (kill_on_drop).
        Ok(())
    }
}

/// The speaker, as an expression device.
pub struct SpeechOutput {
    id: DeviceId,
    voice: Arc<Voice>,
}

impl SpeechOutput {
    pub fn new(voice: Arc<Voice>) -> Self {
        Self {
            id: DeviceId::new("speaker"),
            voice,
        }
    }
}

#[async_trait]
impl Output for SpeechOutput {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            id: self.id.clone(),
            kind: "speaker".into(),
            location: None,
            privacy: PrivacyClass::Personal,
        }
    }

    fn modalities(&self) -> &[Modality] {
        &[Modality::Audio]
    }

    async fn express(&self, e: &Expression) -> Result<(), OutputError> {
        if !self.voice.output_enabled() {
            return Ok(());
        }
        let wanted = match self.voice.speak_mode() {
            SpeakMode::Off => false,
            SpeakMode::Always => true,
            SpeakMode::Auto => self.voice.is_voice_intent(e.intent),
        };
        if wanted {
            if let Some(text) = spoken_text(e) {
                self.voice.say(text);
            }
        }
        Ok(())
    }
}

/// The microphone's device description (for listings and policy).
pub fn mic_info() -> DeviceInfo {
    DeviceInfo {
        id: DeviceId::new(MIC_DEVICE),
        kind: "microphone".into(),
        location: None,
        privacy: PrivacyClass::Private,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::{one_shot_server, one_shot_server_bytes};
    use agentos_core::{AgentId, Urgency};

    fn expr(kind: ExpressionKind, text: &str) -> Expression {
        Expression {
            from: AgentId::new("butler"),
            intent: Some(IntentId(1)),
            kind,
            urgency: Urgency::Normal,
            text: text.into(),
        }
    }

    #[test]
    fn wav_header_is_valid() {
        let pcm = vec![0u8; 3200];
        let w = wav_from_pcm(&pcm, 16_000, 1);
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(u32::from_le_bytes(w[4..8].try_into().unwrap()), 36 + 3200);
        assert_eq!(&w[8..16], b"WAVEfmt ");
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 16_000);
        assert_eq!(u32::from_le_bytes(w[28..32].try_into().unwrap()), 32_000);
        assert_eq!(&w[36..40], b"data");
        assert_eq!(w.len(), 44 + 3200);
    }

    #[test]
    fn levels_of_silence_and_full_scale() {
        assert_eq!(levels(&[0, 0, 0, 0]), (0.0, 0.0));
        let loud: Vec<u8> = [i16::MAX, i16::MIN]
            .iter()
            .flat_map(|s| s.to_le_bytes())
            .collect();
        let (peak, rms) = levels(&loud);
        assert!(peak > 0.99 && rms > 0.99);
    }

    #[test]
    fn spoken_text_skips_plans_and_rewrites_confirmations() {
        assert_eq!(
            spoken_text(&expr(ExpressionKind::Plan, "1. notes.list")),
            None
        );
        assert_eq!(
            spoken_text(&expr(ExpressionKind::Report, " 已记下：买牛奶 ")).as_deref(),
            Some("已记下：买牛奶")
        );
        let ask = "notes.clear：清空全部笔记。原因：不可逆。回复 y 确认，其他任意内容取消。";
        assert_eq!(
            spoken_text(&expr(ExpressionKind::AskConfirm, ask)).as_deref(),
            Some("这一步需要你确认：清空全部笔记。请看屏幕。")
        );
        let long = "长".repeat(500);
        let s = spoken_text(&expr(ExpressionKind::Report, &long)).unwrap();
        assert!(s.ends_with("完整内容在屏幕上。"));
        assert!(s.chars().count() < 260);
    }

    #[test]
    fn multipart_has_fields_and_file() {
        let b = multipart_body(
            "XX",
            &[("model", "m1")],
            ("file", "a.wav", "audio/wav", b"RIFF"),
        );
        let s = String::from_utf8(b).unwrap();
        assert!(
            s.starts_with("--XX\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nm1\r\n")
        );
        assert!(s.contains("name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\r\n--XX--\r\n"));
    }

    fn local(url: &str) -> OpenAiVoiceConfig {
        OpenAiVoiceConfig {
            base_url: url.into(),
            api_key: Some("sk-test".into()),
        }
    }

    #[tokio::test]
    async fn openai_stt_uploads_wav_and_reads_text() {
        let (url, req) = one_shot_server("200 OK", json!({"text": " 记一下 买牛奶 "})).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let stt = OpenAiStt::with_client(local(&url), "whisper-1".into(), Some("zh".into()), http);
        let text = stt
            .transcribe(wav_from_pcm(&[1, 0], 16_000, 1))
            .await
            .unwrap();
        assert_eq!(text, "记一下 买牛奶");
        let req = req.await.unwrap();
        assert!(req.starts_with("POST /audio/transcriptions"));
        assert!(req.to_lowercase().contains("authorization: bearer sk-test"));
        assert!(req.contains("multipart/form-data; boundary="));
        assert!(req.contains("name=\"language\"\r\n\r\nzh"));
        assert!(req.contains("name=\"model\"\r\n\r\nwhisper-1"));
        assert!(req.contains("filename=\"speech.wav\""));
        assert_eq!(stt.destination(), url.trim_start_matches("http://"));
    }

    #[tokio::test]
    async fn openai_stt_reports_api_errors() {
        let (url, _req) =
            one_shot_server("401 Unauthorized", json!({"error": {"message": "bad key"}})).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let stt = OpenAiStt::with_client(local(&url), "m".into(), None, http);
        let err = stt.transcribe(vec![]).await.unwrap_err();
        assert!(err.contains("401") && err.contains("bad key"), "{err}");
    }

    #[tokio::test]
    async fn openai_tts_returns_audio_bytes() {
        let (url, req) =
            one_shot_server_bytes("200 OK", "audio/wav", b"RIFF....WAVE".to_vec()).await;
        let http = reqwest::Client::builder().no_proxy().build().unwrap();
        let tts = OpenAiTts::with_client(local(&url), "tts-1".into(), "alloy".into(), http);
        let audio = tts.synthesize("你好").await.unwrap();
        assert_eq!(audio, b"RIFF....WAVE");
        let req = req.await.unwrap();
        assert!(req.starts_with("POST /audio/speech"));
        assert!(req.contains("\"input\":\"你好\""));
        assert!(req.contains("\"response_format\":\"wav\""));
    }

    #[tokio::test]
    async fn command_backends_run_local_programs() {
        let stt = CommandStt::new(vec![
            "sh".into(),
            "-c".into(),
            "test -s \"$0\" && echo ' 看看 笔记 '".into(),
            "{wav}".into(),
        ]);
        assert_eq!(stt.transcribe(b"RIFF".to_vec()).await.unwrap(), "看看 笔记");
        assert_eq!(stt.destination(), "本机");

        let tts = CommandTts::new(vec!["sh".into(), "-c".into(), "printf RIFF; cat".into()]);
        assert_eq!(tts.synthesize("hi").await.unwrap(), b"RIFFhi\n");

        let bad = CommandTts::new(vec![
            "sh".into(),
            "-c".into(),
            "echo nope >&2; exit 3".into(),
        ]);
        assert!(bad.synthesize("hi").await.unwrap_err().contains("nope"));
    }

    #[test]
    fn speak_modes_round_trip() {
        for m in [SpeakMode::Auto, SpeakMode::Always, SpeakMode::Off] {
            assert_eq!(SpeakMode::parse(m.as_str()), Some(m));
        }
        assert_eq!(SpeakMode::parse("loud"), None);
    }
}
