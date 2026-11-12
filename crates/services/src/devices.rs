//! P0 devices: a line-based text sensor and a console output.

use agentos_core::{
    now_ms, DeviceId, DeviceInfo, Expression, ExpressionKind, Modality, Output, OutputError,
    Percept, PerceptKind, PrivacyClass, Sensor,
};
use async_trait::async_trait;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, BufReader, Lines, Stdin};

/// Reads one percept per line. Stands in for keyboard input or a speech
/// transcriber until those devices exist.
pub struct LineSensor<R> {
    id: DeviceId,
    lines: Lines<R>,
}

impl LineSensor<BufReader<Stdin>> {
    pub fn stdin() -> Self {
        Self::new(DeviceId::new("stdin"), BufReader::new(tokio::io::stdin()))
    }
}

impl<R: AsyncBufRead + Unpin + Send> LineSensor<R> {
    pub fn new(id: DeviceId, reader: R) -> Self {
        Self {
            id,
            lines: reader.lines(),
        }
    }
}

#[async_trait]
impl<R: AsyncBufRead + Unpin + Send> Sensor for LineSensor<R> {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            id: self.id.clone(),
            kind: "text".into(),
            location: None,
            privacy: PrivacyClass::Personal,
        }
    }

    async fn next(&mut self) -> Option<Percept> {
        let line = self.lines.next_line().await.ok().flatten()?;
        Some(Percept {
            device: self.id.clone(),
            at_ms: now_ms(),
            kind: PerceptKind::Text {
                text: line,
                speaker: None,
            },
        })
    }
}

pub struct ConsoleOutput {
    id: DeviceId,
}

impl Default for ConsoleOutput {
    fn default() -> Self {
        Self {
            id: DeviceId::new("console"),
        }
    }
}

#[async_trait]
impl Output for ConsoleOutput {
    fn info(&self) -> DeviceInfo {
        DeviceInfo {
            id: self.id.clone(),
            kind: "console".into(),
            location: None,
            privacy: PrivacyClass::Personal,
        }
    }

    fn modalities(&self) -> &[Modality] {
        &[Modality::Visual]
    }

    async fn express(&self, e: &Expression) -> Result<(), OutputError> {
        let tag = match e.kind {
            ExpressionKind::Plan => "计划",
            ExpressionKind::Report => "结果",
            ExpressionKind::AskConfirm => "确认",
            ExpressionKind::Error => "错误",
        };
        println!("[{}·{tag}] {}", e.from, e.text);
        Ok(())
    }
}
