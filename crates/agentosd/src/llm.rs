//! Plain text completion, for components that need a model but not tool use
//! (for example the plugin generator).

use async_trait::async_trait;

#[async_trait]
pub trait TextModel: Send + Sync {
    /// One system prompt + one user message in, the model's text out.
    async fn complete(&self, system: &str, user: &str) -> Result<String, String>;
}
