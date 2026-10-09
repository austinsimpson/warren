//! A dedicated tokio runtime for network I/O. reqwest (used directly and by
//! rig) needs tokio, while gpui drives its own executor, so async work that
//! touches the network is handed to this runtime and awaited from gpui.

use std::sync::LazyLock;

use anyhow::Result;
use tokio::runtime::Runtime;

pub static RUNTIME: LazyLock<Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("failed to start tokio runtime")
});

/// Run `fut` on the tokio runtime and await its result from any executor.
pub async fn run<T: Send + 'static>(
    fut: impl Future<Output = Result<T>> + Send + 'static,
) -> Result<T> {
    RUNTIME.spawn(fut).await?
}
