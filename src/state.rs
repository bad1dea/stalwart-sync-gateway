use std::{collections::BTreeMap, sync::Arc};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::config::{Config, StateBackend};

pub type SharedStateStore = Arc<dyn StateStore>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncRecord {
    pub user: String,
    pub device_id: String,
    pub collection_id: String,
    pub sync_key: String,
    pub jmap_state: String,
}

#[async_trait]
pub trait StateStore: Send + Sync {
    async fn get(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Option<SyncRecord>>;
    async fn put(&self, record: SyncRecord) -> anyhow::Result<()>;
}

pub async fn new_store(config: &Config) -> anyhow::Result<SharedStateStore> {
    match config.state_backend {
        StateBackend::Memory | StateBackend::Sqlite | StateBackend::Redis => {
            if config.state_backend != StateBackend::Memory {
                tracing::warn!(
                    backend = ?config.state_backend,
                    "state backend scaffold is using in-memory storage until migrations are implemented"
                );
            }
            Ok(Arc::new(MemoryStateStore::default()))
        }
    }
}

#[derive(Default)]
pub struct MemoryStateStore {
    records: RwLock<BTreeMap<String, SyncRecord>>,
}

#[async_trait]
impl StateStore for MemoryStateStore {
    async fn get(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Option<SyncRecord>> {
        Ok(self
            .records
            .read()
            .await
            .get(&key(user, device_id, collection_id))
            .cloned())
    }

    async fn put(&self, record: SyncRecord) -> anyhow::Result<()> {
        self.records.write().await.insert(
            key(&record.user, &record.device_id, &record.collection_id),
            record,
        );
        Ok(())
    }
}

fn key(user: &str, device_id: &str, collection_id: &str) -> String {
    format!("{user}\u{1f}{device_id}\u{1f}{collection_id}")
}
