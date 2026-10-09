//! Claude access via rig. rig's HTTP stack needs a tokio runtime, while gpui
//! drives its own executor, so requests run on a dedicated tokio runtime and
//! the UI awaits the returned `JoinHandle`.

use std::sync::LazyLock;

use anyhow::Result;
use rig_core::{
    completion::CompletionRequest,
    providers::anthropic::{self, Anthropic},
};
use tokio::{runtime::Runtime, task::JoinHandle};

static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime")
});

pub fn spawn_hello() -> JoinHandle<Result<String>> {
    RUNTIME.spawn(async {
        let model = Anthropic::from_env()?.completion(anthropic::CLAUDE_SONNET_5_5);
        let request = CompletionRequest::new("Say hello to a new AI stock trading app called Warren in one sentence.")
            .preamble("You are the assistant inside Warren, a desktop stock trading app.")
            .max_tokens(256);
        let response = model.call(request).await?;
        Ok(response.text())
    })
}

#[cfg(test)]
mod tests {
    /// Hits the live API; run with `cargo test -- --ignored`.
    #[test]
    #[ignore]
    fn hello_claude() {
        dotenvy::dotenv().ok();
        let reply = super::RUNTIME.block_on(super::spawn_hello()).unwrap().unwrap();
        println!("{reply}");
        assert!(!reply.is_empty());
    }
}
