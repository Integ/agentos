//! Plain text completion, for components that need a model but not tool use
//! (for example the plugin generator).

use async_trait::async_trait;

#[async_trait]
pub trait TextModel: Send + Sync {
    /// One system prompt + one user message in, the model's text out.
    async fn complete(&self, system: &str, user: &str) -> Result<String, String>;
}

/// How a model client retries transient failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Retry {
    /// Total tries, including the first.
    pub attempts: u32,
    /// Wait before the first retry; doubled for each further one.
    pub base_delay: std::time::Duration,
    /// Longest wait honoured from a `Retry-After` header.
    pub max_delay: std::time::Duration,
}

impl Default for Retry {
    fn default() -> Self {
        Self {
            attempts: 3,
            base_delay: std::time::Duration::from_secs(1),
            max_delay: std::time::Duration::from_secs(20),
        }
    }
}

impl Retry {
    /// Never retry.
    pub fn none() -> Self {
        Self {
            attempts: 1,
            ..Self::default()
        }
    }
}

/// Rate limits (429), overload (529, Anthropic) and gateway hiccups (5xx,
/// 408) are worth another try; anything else (bad key, bad request, unknown
/// model) will fail the same way again.
pub fn is_transient(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 408 | 429 | 500 | 502 | 503 | 504 | 529)
}

fn retry_after(resp: &reqwest::Response) -> Option<std::time::Duration> {
    resp.headers()
        .get(reqwest::header::RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(std::time::Duration::from_secs)
}

/// Send the request `make` builds, retrying transient HTTP statuses and
/// failed connections. Timeouts are not retried: the client timeout is long
/// and a retry would multiply it. Returns the last response or error.
pub async fn send_with_retry(
    retry: Retry,
    make: impl Fn() -> reqwest::RequestBuilder,
) -> Result<reqwest::Response, reqwest::Error> {
    let mut delay = retry.base_delay;
    let mut attempt = 1;
    loop {
        let last = attempt >= retry.attempts.max(1);
        let wait = match make().send().await {
            Ok(resp) if !last && is_transient(resp.status()) => {
                retry_after(&resp).map_or(delay, |d| d.min(retry.max_delay))
            }
            Err(e) if !last && e.is_connect() => delay,
            other => return other,
        };
        tokio::time::sleep(wait).await;
        delay = delay.saturating_mul(2);
        attempt += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_http::sequence_server;
    use serde_json::json;
    use std::time::Duration;

    fn fast(attempts: u32) -> Retry {
        Retry {
            attempts,
            base_delay: Duration::from_millis(1),
            max_delay: Duration::from_millis(5),
        }
    }

    fn client() -> reqwest::Client {
        reqwest::Client::builder().no_proxy().build().unwrap()
    }

    #[tokio::test]
    async fn transient_errors_are_retried_until_success() {
        let (url, server) = sequence_server(vec![
            ("429 Too Many Requests", json!({"error": "slow down"})),
            ("529 Overloaded", json!({"error": "overloaded"})),
            ("200 OK", json!({"ok": true})),
        ])
        .await;
        let http = client();
        let resp = send_with_retry(fast(3), || http.post(&url).json(&json!({"q": 1})))
            .await
            .unwrap();
        assert_eq!(resp.status(), 200);
        assert_eq!(server.await.unwrap().len(), 3);
    }

    #[tokio::test]
    async fn gives_up_after_the_last_attempt() {
        let (url, server) = sequence_server(vec![
            ("503 Service Unavailable", json!({})),
            ("503 Service Unavailable", json!({})),
        ])
        .await;
        let http = client();
        let resp = send_with_retry(fast(2), || http.get(&url)).await.unwrap();
        assert_eq!(resp.status(), 503);
        assert_eq!(server.await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn permanent_errors_are_not_retried() {
        let (url, server) =
            sequence_server(vec![("401 Unauthorized", json!({"error": "bad key"}))]).await;
        let http = client();
        let resp = send_with_retry(fast(3), || http.get(&url)).await.unwrap();
        assert_eq!(resp.status(), 401);
        assert_eq!(server.await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn refused_connections_are_retried() {
        // Nothing listens on this port once the listener is dropped.
        let addr = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap();
        let http = client();
        let tries = std::sync::atomic::AtomicU32::new(0);
        let r = send_with_retry(fast(3), || {
            tries.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            http.get(format!("http://{addr}"))
        })
        .await;
        assert!(r.is_err());
        assert_eq!(tries.into_inner(), 3);
    }
}
