use std::{
    collections::BTreeMap,
    path::Path,
    sync::{Arc, Mutex},
};

use anyhow::Context;
use async_trait::async_trait;
use rusqlite::{params, Connection, OptionalExtension};
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
    pub seen_ids: Vec<String>,
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
        StateBackend::Memory => Ok(Arc::new(MemoryStateStore::default())),
        StateBackend::Sqlite => {
            let store = SqliteStateStore::open(&config.state_sqlite_path).with_context(|| {
                format!(
                    "failed to open SQLite state at {}",
                    config.state_sqlite_path
                )
            })?;
            Ok(Arc::new(store))
        }
        StateBackend::Redis => {
            tracing::warn!(
                backend = ?config.state_backend,
                "Redis state backend is not implemented; using in-memory storage"
            );
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

pub struct SqliteStateStore {
    connection: Mutex<Connection>,
}

impl SqliteStateStore {
    pub fn open(path: &str) -> anyhow::Result<Self> {
        if let Some(parent) = Path::new(path).parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!("failed to create state directory {}", parent.display())
            })?;
        }

        let connection =
            Connection::open(path).with_context(|| format!("failed to open SQLite file {path}"))?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.pragma_update(None, "foreign_keys", "ON")?;
        migrate(&connection)?;
        Ok(Self {
            connection: Mutex::new(connection),
        })
    }
}

#[async_trait]
impl StateStore for SqliteStateStore {
    async fn get(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Option<SyncRecord>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        connection
            .query_row(
                "SELECT user, device_id, collection_id, sync_key, jmap_state, seen_ids_json
                 FROM sync_records
                 WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
                params![user, device_id, collection_id],
                |row| {
                    let seen_ids_json: String = row.get(5)?;
                    let seen_ids = serde_json::from_str(&seen_ids_json).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            5,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok(SyncRecord {
                        user: row.get(0)?,
                        device_id: row.get(1)?,
                        collection_id: row.get(2)?,
                        sync_key: row.get(3)?,
                        jmap_state: row.get(4)?,
                        seen_ids,
                    })
                },
            )
            .optional()
            .context("failed to read Sync state")
    }

    async fn put(&self, record: SyncRecord) -> anyhow::Result<()> {
        let seen_ids_json =
            serde_json::to_string(&record.seen_ids).context("failed to encode seen IDs")?;
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        connection
            .execute(
                "INSERT INTO sync_records
                    (user, device_id, collection_id, sync_key, jmap_state, seen_ids_json, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, unixepoch())
                 ON CONFLICT(user, device_id, collection_id) DO UPDATE SET
                    sync_key = excluded.sync_key,
                    jmap_state = excluded.jmap_state,
                    seen_ids_json = excluded.seen_ids_json,
                    updated_at = excluded.updated_at",
                params![
                    record.user,
                    record.device_id,
                    record.collection_id,
                    record.sync_key,
                    record.jmap_state,
                    seen_ids_json
                ],
            )
            .context("failed to write Sync state")?;
        Ok(())
    }
}

fn migrate(connection: &Connection) -> anyhow::Result<()> {
    connection.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            applied_at INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS sync_records (
            user TEXT NOT NULL,
            device_id TEXT NOT NULL,
            collection_id TEXT NOT NULL,
            sync_key TEXT NOT NULL,
            jmap_state TEXT NOT NULL DEFAULT '',
            seen_ids_json TEXT NOT NULL DEFAULT '[]',
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (user, device_id, collection_id)
        );

        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (1, unixepoch());
        ",
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(sync_key: &str) -> SyncRecord {
        SyncRecord {
            user: "user@example.com".to_owned(),
            device_id: "device".to_owned(),
            collection_id: "inbox".to_owned(),
            sync_key: sync_key.to_owned(),
            jmap_state: "state".to_owned(),
            seen_ids: vec!["a".to_owned(), "b".to_owned()],
        }
    }

    #[tokio::test]
    async fn memory_store_round_trips_record() {
        let store = MemoryStateStore::default();

        store.put(record("1")).await.unwrap();

        assert_eq!(
            store
                .get("user@example.com", "device", "inbox")
                .await
                .unwrap(),
            Some(record("1"))
        );
    }

    #[tokio::test]
    async fn sqlite_store_round_trips_and_updates_record() {
        let path = std::env::temp_dir().join(format!(
            "stalwart-sync-gateway-state-{}.db",
            uuid::Uuid::new_v4()
        ));
        let store = SqliteStateStore::open(path.to_str().unwrap()).unwrap();

        store.put(record("1")).await.unwrap();
        let mut updated = record("2");
        updated.seen_ids.push("c".to_owned());
        store.put(updated.clone()).await.unwrap();

        assert_eq!(
            store
                .get("user@example.com", "device", "inbox")
                .await
                .unwrap(),
            Some(updated)
        );

        let _ = std::fs::remove_file(path);
    }
}
