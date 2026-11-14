use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use agentos_ipc::{
    current_uid, read_frame, write_frame, Command, Event, ProtocolError, Reply, Request, Response,
    ResponseBody, VERSION,
};
use tokio::io::BufReader;
use tokio::net::UnixStream;
use tokio::time::timeout;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug)]
pub enum ClientError {
    Transport { message: String, ambiguous: bool },
    Protocol { message: String, ambiguous: bool },
    Remote(ProtocolError),
}

impl ClientError {
    pub fn ambiguous(&self) -> bool {
        matches!(
            self,
            Self::Transport {
                ambiguous: true,
                ..
            } | Self::Protocol {
                ambiguous: true,
                ..
            }
        )
    }

    pub fn json(&self) -> serde_json::Value {
        let code = match self {
            Self::Remote(e) => e.code.as_str(),
            Self::Protocol { .. } => "protocol_error",
            Self::Transport { .. } => "connection_error",
        };
        serde_json::json!({"error":{"code":code,"message":self.to_string(),"outcome_unknown":self.ambiguous()}})
    }
}

impl fmt::Display for ClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Remote(error) => write!(f, "{}：{}", error.code, error.message)?,
            Self::Transport { message, .. } | Self::Protocol { message, .. } => {
                f.write_str(message)?
            }
        }
        if self.ambiguous() {
            f.write_str("；操作结果可能已经生效，请先查询任务，避免直接重发")?;
        }
        Ok(())
    }
}

impl std::error::Error for ClientError {}

fn mutating(command: &Command) -> bool {
    matches!(
        command,
        Command::Submit { .. }
            | Command::Cancel { .. }
            | Command::Approve { .. }
            | Command::Reply { .. }
            | Command::Preauthorize { .. }
            | Command::RevokePreauthorization { .. }
            | Command::RegisterCaller { .. }
            | Command::RevokeCaller { .. }
    )
}

fn transport(error: impl fmt::Display, ambiguous: bool) -> ClientError {
    ClientError::Transport {
        message: error.to_string(),
        ambiguous,
    }
}

/// Verify the protected local endpoint before sending any intent. Peer
/// credentials also close the path-replacement race for a different OS user.
async fn connect(path: &Path) -> Result<UnixStream, ClientError> {
    use std::os::unix::fs::{FileTypeExt, MetadataExt};
    let metadata = std::fs::symlink_metadata(path).map_err(|e| {
        transport(
            format!("无法连接 {}：{e}；请先启动 agentosd", path.display()),
            false,
        )
    })?;
    if !metadata.file_type().is_socket()
        || metadata.uid() != current_uid()
        || metadata.mode() & 0o077 != 0
    {
        return Err(transport(
            "拒绝连接：控制端点必须是当前系统用户拥有、仅本人可访问的 Unix socket",
            false,
        ));
    }
    let stream = timeout(REQUEST_TIMEOUT, UnixStream::connect(path))
        .await
        .map_err(|_| transport("连接后台服务超时", false))?
        .map_err(|e| {
            transport(
                format!("无法连接 {}：{e}；请检查后台服务", path.display()),
                false,
            )
        })?;
    let credentials = stream
        .peer_cred()
        .map_err(|e| transport(format!("无法验证后台服务身份：{e}"), false))?;
    if credentials.uid() != current_uid() {
        return Err(transport("拒绝连接：后台服务与当前系统用户不一致", false));
    }
    Ok(stream)
}

fn reply_matches(command: &Command, reply: &Reply) -> bool {
    matches!(
        (command, reply),
        (Command::Ping {}, Reply::Pong { .. })
            | (Command::Submit { .. }, Reply::Accepted { .. })
            | (Command::List { .. }, Reply::Tasks { .. })
            | (
                Command::Get { .. } | Command::Cancel { .. },
                Reply::Task { .. }
            )
            | (Command::Approve { .. }, Reply::ApprovalQueued { .. })
            | (Command::Reply { .. }, Reply::ReplyQueued { .. })
            | (Command::Preauthorize { .. }, Reply::Preauthorized { .. })
            | (
                Command::Preauthorizations {},
                Reply::Preauthorizations { .. }
            )
            | (
                Command::RevokePreauthorization { .. },
                Reply::PreauthorizationRevoked { .. }
            )
            | (
                Command::RegisterCaller { .. },
                Reply::CallerRegistered { .. }
            )
            | (Command::Callers {}, Reply::Callers { .. })
            | (Command::RevokeCaller { .. }, Reply::CallerRevoked { .. })
            | (Command::Capabilities {}, Reply::Capabilities { .. })
            | (Command::Read { .. }, Reply::Value { .. })
            | (Command::Watch { .. }, Reply::Watching { .. })
            | (
                Command::Voice {} | Command::Speech { .. },
                Reply::Voice { .. }
            )
            | (Command::Listen { .. }, Reply::Listening { .. })
    )
}

fn verify_response(response: Response, id: u64, ambiguous: bool) -> Result<Reply, ClientError> {
    if response.version != VERSION || response.id != id {
        return Err(ClientError::Protocol {
            message: "后台响应的协议版本或请求编号不匹配".into(),
            ambiguous,
        });
    }
    match response.body {
        ResponseBody::Ok { data } => Ok(data),
        ResponseBody::Error { error } => Err(ClientError::Remote(error)),
    }
}

#[derive(Clone)]
pub struct Client {
    path: PathBuf,
    next_id: Arc<AtomicU64>,
    /// Make every request as this registered caller instead of as the user.
    caller: Option<String>,
}

impl Client {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            next_id: Arc::new(AtomicU64::new(1)),
            caller: None,
        }
    }

    /// Narrow this client to what the caller `id` was given. There is no
    /// way back: a client made for a caller never speaks as the user.
    pub fn as_caller(mut self, id: impl Into<String>) -> Self {
        self.caller = Some(id.into());
        self
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Each request connects once and is sent once. In particular, a missing
    /// mutation response is never interpreted as permission to replay it.
    pub async fn request(&self, command: Command) -> Result<Reply, ClientError> {
        let mut stream = connect(&self.path).await?;
        let ambiguous = mutating(&command);
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            version: VERSION,
            id,
            command,
            caller: self.caller.clone(),
        };
        timeout(REQUEST_TIMEOUT, write_frame(&mut stream, &request))
            .await
            .map_err(|_| transport("发送请求超时", ambiguous))?
            .map_err(|e| {
                let possible = ambiguous && e.kind() != io::ErrorKind::InvalidData;
                transport(e, possible)
            })?;
        let mut reader = BufReader::new(stream);
        let frame = timeout(REQUEST_TIMEOUT, read_frame(&mut reader))
            .await
            .map_err(|_| transport("等待后台响应超时", ambiguous))?
            .map_err(|e| transport(e, ambiguous))?
            .ok_or_else(|| transport("后台连接在响应前关闭", ambiguous))?;
        let response = serde_json::from_slice(&frame).map_err(|e| ClientError::Protocol {
            message: format!("无法解析后台响应：{e}"),
            ambiguous,
        })?;
        let reply = verify_response(response, id, ambiguous)?;
        let identity_matches = match (&request.command, &reply) {
            (Command::Get { id } | Command::Cancel { id }, Reply::Task { task }) => *id == task.id,
            (
                Command::Approve { id, step, .. },
                Reply::ApprovalQueued {
                    id: actual,
                    step: actual_step,
                },
            ) => id == actual && step == actual_step,
            (
                Command::Reply { id, step, .. },
                Reply::ReplyQueued {
                    id: actual,
                    step: actual_step,
                },
            ) => id == actual && step == actual_step,
            (
                Command::RevokePreauthorization { id },
                Reply::PreauthorizationRevoked { id: actual },
            ) => id == actual,
            (Command::RevokeCaller { id }, Reply::CallerRevoked { id: actual }) => id == actual,
            (Command::RegisterCaller { id, .. }, Reply::CallerRegistered { caller }) => {
                id == &caller.id
            }
            (
                Command::Preauthorize { capability, .. },
                Reply::Preauthorized { preauthorization },
            ) => capability == &preauthorization.capability,
            (
                Command::Read { capability, .. },
                Reply::Value {
                    capability: actual, ..
                },
            ) => capability == actual,
            _ => true,
        };
        if !reply_matches(&request.command, &reply) || !identity_matches {
            return Err(ClientError::Protocol {
                message: "后台返回了不匹配请求的响应类型".into(),
                ambiguous,
            });
        }
        Ok(reply)
    }

    pub async fn watch(&self, after: Option<u64>) -> Result<WatchStream, ClientError> {
        self.watch_with(after, false).await
    }

    /// [`Self::watch`], also receiving voice events when `voice` is set.
    pub async fn watch_with(
        &self,
        after: Option<u64>,
        voice: bool,
    ) -> Result<WatchStream, ClientError> {
        let mut stream = connect(&self.path).await?;
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let request = Request {
            version: VERSION,
            id,
            command: Command::Watch { after, voice },
            caller: self.caller.clone(),
        };
        timeout(REQUEST_TIMEOUT, write_frame(&mut stream, &request))
            .await
            .map_err(|_| transport("发送订阅请求超时", false))?
            .map_err(|e| transport(e, false))?;
        let mut reader = BufReader::new(stream);
        let frame = timeout(REQUEST_TIMEOUT, read_frame(&mut reader))
            .await
            .map_err(|_| transport("建立订阅超时", false))?
            .map_err(|e| transport(e, false))?
            .ok_or_else(|| transport("后台连接在订阅响应前关闭", false))?;
        let response = serde_json::from_slice(&frame).map_err(|e| ClientError::Protocol {
            message: format!("无法解析订阅响应：{e}"),
            ambiguous: false,
        })?;
        let Reply::Watching { revision } = verify_response(response, id, false)? else {
            return Err(ClientError::Protocol {
                message: "后台未返回订阅确认".into(),
                ambiguous: false,
            });
        };
        Ok(WatchStream { reader, revision })
    }
}

pub struct WatchStream {
    reader: BufReader<UnixStream>,
    pub revision: u64,
}

impl WatchStream {
    pub async fn next(&mut self) -> Result<Option<Event>, ClientError> {
        let Some(frame) = read_frame(&mut self.reader)
            .await
            .map_err(|e| transport(e, false))?
        else {
            return Ok(None);
        };
        let event: Event = serde_json::from_slice(&frame).map_err(|e| ClientError::Protocol {
            message: format!("无法解析变更事件：{e}"),
            ambiguous: false,
        })?;
        if event.version != VERSION {
            return Err(ClientError::Protocol {
                message: "变更事件协议版本不匹配".into(),
                ambiguous: false,
            });
        }
        self.revision = event.revision;
        Ok(Some(event))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentos_ipc::Response;
    use std::os::unix::fs::PermissionsExt;
    use tokio::net::UnixListener;

    #[test]
    fn mismatched_responses_cannot_be_mistaken_for_a_mutation_acknowledgement() {
        let response = Response::ok(
            999,
            Reply::Pong {
                server_version: "test".into(),
                revision: 0,
            },
        );
        let error = verify_response(response, 1, true).unwrap_err();
        assert!(error.ambiguous());
        assert!(error.to_string().contains("避免直接重发"));
        let remote = verify_response(
            Response::error(1, "invalid_confirmation", "expired"),
            1,
            true,
        )
        .unwrap_err();
        assert!(!remote.ambiguous());
        assert_eq!(remote.json()["error"]["code"], "invalid_confirmation");
    }

    #[tokio::test]
    async fn disconnect_after_submission_is_reported_without_a_second_attempt() {
        let path = std::env::temp_dir().join(format!(
            "agentos-cli-drop-{}-{}.sock",
            std::process::id(),
            agentos_core::now_ms()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let service = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            let mut reader = BufReader::new(stream);
            let frame = read_frame(&mut reader).await.unwrap().unwrap();
            let request: Request = serde_json::from_slice(&frame).unwrap();
            assert!(matches!(request.command, Command::Submit { .. }));
            drop(reader);
            assert!(timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err());
        });
        let error = Client::new(&path)
            .request(Command::Submit {
                text: "记一下 买牛奶".into(),
                thread: None,
                spoken: false,
            })
            .await
            .unwrap_err();
        assert!(error.ambiguous());
        service.await.unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[tokio::test]
    async fn an_unprotected_socket_is_rejected_before_connecting() {
        let path = std::env::temp_dir().join(format!(
            "agentos-cli-open-{}-{}.sock",
            std::process::id(),
            agentos_core::now_ms()
        ));
        let listener = UnixListener::bind(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o666)).unwrap();
        let error = Client::new(&path)
            .request(Command::Ping {})
            .await
            .unwrap_err();
        assert!(!error.ambiguous());
        assert!(timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err());
        std::fs::remove_file(path).unwrap();
    }
}
