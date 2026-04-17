use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::RwLock;

use crate::accounts;

const REFRESH_INTERVAL: Duration = Duration::from_secs(60);

#[derive(Clone, Debug)]
pub(crate) struct DefaultPoolLabels {
    labels: Arc<RwLock<Vec<String>>>,
}

impl DefaultPoolLabels {
    pub(crate) fn new(initial_labels: Vec<String>) -> Self {
        Self {
            labels: Arc::new(RwLock::new(initial_labels)),
        }
    }

    pub(crate) async fn snapshot(&self) -> Vec<String> {
        self.labels.read().await.clone()
    }

    pub(crate) fn spawn_refresh_task(&self, accounts_root: PathBuf) {
        let labels = Arc::clone(&self.labels);

        tokio::spawn(async move {
            loop {
                tokio::time::sleep(REFRESH_INTERVAL).await;

                let accounts_root = accounts_root.clone();
                let refreshed =
                    tokio::task::spawn_blocking(move || accounts::list_labels(&accounts_root))
                        .await;

                match refreshed {
                    Ok(Ok(next_labels)) => {
                        *labels.write().await = next_labels;
                    }
                    Ok(Err(err)) => {
                        tracing::warn!(error = %err, "failed to refresh default pool labels");
                    }
                    Err(err) => {
                        tracing::warn!(error = %err, "default pool label refresh task failed");
                    }
                }
            }
        });
    }
}
