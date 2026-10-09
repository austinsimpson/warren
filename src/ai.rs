//! Claude access via rig. Must run on the tokio runtime; see [`crate::runtime`].

use anyhow::Result;
use rig_core::{
    completion::CompletionRequest,
    providers::anthropic::{self, Anthropic},
};

pub async fn hello() -> Result<String> {
    let model = Anthropic::from_env()?.completion(anthropic::CLAUDE_SONNET_5_5);
    let request = CompletionRequest::new(
        "Say hello to a new AI stock trading app called Warren in one sentence.",
    )
    .preamble("You are the assistant inside Warren, a desktop stock trading app.")
    .max_tokens(256);
    let response = model.call(request).await?;
    Ok(response.text())
}

#[cfg(test)]
mod tests {
    /// Hits the live API; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn hello_claude() {
        dotenvy::dotenv().ok();
        let reply = crate::runtime::RUNTIME.block_on(super::hello()).unwrap();
        println!("{reply}");
        assert!(!reply.is_empty());
    }
}
