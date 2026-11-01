//! Test helper: a one-request HTTP server on localhost.

use serde_json::Value;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// Serve one canned HTTP response and capture the request.
pub async fn one_shot_server(
    status: &str,
    body: Value,
) -> (String, tokio::task::JoinHandle<String>) {
    one_shot_server_bytes(status, "application/json", body.to_string().into_bytes()).await
}

/// Like [`one_shot_server`], with any content type and body.
pub async fn one_shot_server_bytes(
    status: &str,
    content_type: &str,
    payload: Vec<u8>,
) -> (String, tokio::task::JoinHandle<String>) {
    let content_type = content_type.to_string();
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let status = status.to_string();
    let handle = tokio::spawn(async move {
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = vec![0u8; 65536];
        let mut req = Vec::new();
        loop {
            let n = sock.read(&mut buf).await.unwrap();
            req.extend_from_slice(&buf[..n]);
            let s = String::from_utf8_lossy(&req);
            if let Some(idx) = s.find("\r\n\r\n") {
                let len = s[..idx]
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|v| v.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                if req.len() >= idx + 4 + len {
                    break;
                }
            }
            if n == 0 {
                break;
            }
        }
        let head = format!(
            "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
            payload.len()
        );
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(&payload).await.unwrap();
        String::from_utf8_lossy(&req).to_string()
    });
    (format!("http://{addr}"), handle)
}
