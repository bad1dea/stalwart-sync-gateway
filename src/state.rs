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

/// Per-item state for collections that need real add/change/delete
/// diffing (Notes) rather than the simpler "have I ever sent this id"
/// dedup `SyncRecord.seen_ids` provides for mail. `hash` is an opaque,
/// caller-defined change-detection token (see jmap::notes::note_hash) --
/// this store just persists and compares it, it doesn't interpret it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ItemState {
    pub item_id: String,
    pub hash: String,
}

#[async_trait]
pub trait StateStore: Send + Sync {
    async fn contact_extensions(
        &self,
        account_id: &str,
        contact_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>>;
    async fn put_contact_extensions(
        &self,
        account_id: &str,
        contact_id: &str,
        value: Option<serde_json::Value>,
    ) -> anyhow::Result<()>;
    async fn get(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Option<SyncRecord>>;
    async fn put(&self, record: SyncRecord) -> anyhow::Result<()>;
    async fn put_with_items(
        &self,
        record: SyncRecord,
        items: Option<Vec<ItemState>>,
    ) -> anyhow::Result<()> {
        self.put_with_items_and_clear(record, items, None).await
    }

    async fn put_with_items_and_clear(
        &self,
        record: SyncRecord,
        items: Option<Vec<ItemState>>,
        completed: Option<(String, Vec<ItemState>)>,
    ) -> anyhow::Result<()>;

    // EAS IDs stay stable when JMAP replaces an immutable draft.
    async fn draft_mappings(&self, account: &str) -> anyhow::Result<Vec<ItemState>>;
    async fn swap_draft_mapping(
        &self,
        account: &str,
        stable_id: &str,
        expected_backend: &str,
        new_backend: &str,
    ) -> anyhow::Result<bool>;

    async fn reserve_item(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        sync_key: &str,
        journal_id: &str,
        item: ItemState,
    ) -> anyhow::Result<Option<ItemState>>;

    /// Last-known per-item state for a collection, previous call to
    /// `put_item_states` (empty if never synced before).
    async fn item_states(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Vec<ItemState>>;

    /// Replaces the ENTIRE per-item state set for a collection. Callers
    /// re-list the complete current set from JMAP each sync (no
    /// incremental JMAP queryChanges/changes yet -- same simplification
    /// the mail path's `seen_ids` makes), so a full replace is the correct
    /// operation here, not a merge.
    async fn put_item_states(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        items: Vec<ItemState>,
    ) -> anyhow::Result<()>;
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
    item_states: RwLock<BTreeMap<String, Vec<ItemState>>>,
    draft_mappings: RwLock<BTreeMap<(String, String), String>>,
    contact_extensions: RwLock<BTreeMap<(String, String), serde_json::Value>>,
}

#[async_trait]
impl StateStore for MemoryStateStore {
    async fn contact_extensions(
        &self,
        account_id: &str,
        contact_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        Ok(self
            .contact_extensions
            .read()
            .await
            .get(&(account_id.to_owned(), contact_id.to_owned()))
            .cloned())
    }

    async fn put_contact_extensions(
        &self,
        account_id: &str,
        contact_id: &str,
        value: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let key = (account_id.to_owned(), contact_id.to_owned());
        let mut values = self.contact_extensions.write().await;
        if let Some(value) = value {
            values.insert(key, value);
        } else {
            values.remove(&key);
        }
        Ok(())
    }

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

    async fn put_with_items_and_clear(
        &self,
        record: SyncRecord,
        items: Option<Vec<ItemState>>,
        completed: Option<(String, Vec<ItemState>)>,
    ) -> anyhow::Result<()> {
        let record_key = key(&record.user, &record.device_id, &record.collection_id);
        let journal_key = completed
            .as_ref()
            .map(|(journal, _)| key(&record.user, &record.device_id, journal));
        let mut records = self.records.write().await;
        let mut item_states = self.item_states.write().await;
        records.insert(record_key.clone(), record);
        if let Some(items) = items {
            item_states.insert(record_key, items);
        }
        if let (Some(journal_key), Some((_, completed))) = (journal_key, completed) {
            if let Some(entries) = item_states.get_mut(&journal_key) {
                entries.retain(|entry| !completed.contains(entry));
            }
        }
        Ok(())
    }

    async fn draft_mappings(&self, account: &str) -> anyhow::Result<Vec<ItemState>> {
        let mappings = self.draft_mappings.read().await;
        Ok(mappings
            .iter()
            .filter(|((owner, _), _)| owner == account)
            .map(|((_, stable), backend)| ItemState {
                item_id: stable.clone(),
                hash: backend.clone(),
            })
            .collect())
    }

    async fn swap_draft_mapping(
        &self,
        account: &str,
        stable_id: &str,
        expected_backend: &str,
        new_backend: &str,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !account.is_empty()
                && !stable_id.is_empty()
                && !expected_backend.is_empty()
                && !new_backend.is_empty(),
            "invalid draft identity"
        );
        let mut mappings = self.draft_mappings.write().await;
        let entry = (account.to_owned(), stable_id.to_owned());
        let current = mappings
            .get(&entry)
            .map(String::as_str)
            .unwrap_or(stable_id);
        if current != expected_backend {
            return Ok(false);
        }
        if mappings.iter().any(|((owner, stable), backend)| {
            owner == account
                && stable != stable_id
                && (backend == new_backend || stable == new_backend || backend == stable_id)
        }) {
            anyhow::bail!("conflicting draft identity");
        }
        if stable_id == new_backend {
            mappings.remove(&entry);
        } else {
            mappings.insert(entry, new_backend.to_owned());
        }
        Ok(true)
    }

    async fn reserve_item(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        sync_key: &str,
        journal_id: &str,
        item: ItemState,
    ) -> anyhow::Result<Option<ItemState>> {
        let records = self.records.read().await;
        if !records
            .get(&key(user, device_id, collection_id))
            .is_some_and(|record| record.sync_key == sync_key)
        {
            return Ok(None);
        }
        let mut states = self.item_states.write().await;
        let entries = states.entry(key(user, device_id, journal_id)).or_default();
        if let Some(existing) = entries
            .iter()
            .find(|existing| existing.item_id == item.item_id)
        {
            return Ok(Some(existing.clone()));
        }
        entries.push(item.clone());
        Ok(Some(item))
    }

    async fn item_states(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Vec<ItemState>> {
        Ok(self
            .item_states
            .read()
            .await
            .get(&key(user, device_id, collection_id))
            .cloned()
            .unwrap_or_default())
    }

    async fn put_item_states(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        items: Vec<ItemState>,
    ) -> anyhow::Result<()> {
        self.item_states
            .write()
            .await
            .insert(key(user, device_id, collection_id), items);
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
    async fn contact_extensions(
        &self,
        account_id: &str,
        contact_id: &str,
    ) -> anyhow::Result<Option<serde_json::Value>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        let encoded: Option<String> = connection
            .query_row(
                "SELECT fields_json FROM contact_extensions WHERE account_id = ?1 AND contact_id = ?2",
                params![account_id, contact_id],
                |row| row.get(0),
            )
            .optional()
            .context("failed to read contact extension fields")?;
        encoded
            .map(|value| {
                serde_json::from_str(&value).context("invalid stored contact extension fields")
            })
            .transpose()
    }

    async fn put_contact_extensions(
        &self,
        account_id: &str,
        contact_id: &str,
        value: Option<serde_json::Value>,
    ) -> anyhow::Result<()> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        if let Some(value) = value {
            let encoded = serde_json::to_string(&value)
                .context("failed to encode contact extension fields")?;
            connection.execute(
                "INSERT INTO contact_extensions (account_id, contact_id, fields_json, updated_at)
                 VALUES (?1, ?2, ?3, unixepoch())
                 ON CONFLICT(account_id, contact_id) DO UPDATE SET
                    fields_json = excluded.fields_json, updated_at = excluded.updated_at",
                params![account_id, contact_id, encoded],
            ).context("failed to store contact extension fields")?;
        } else {
            connection
                .execute(
                    "DELETE FROM contact_extensions WHERE account_id = ?1 AND contact_id = ?2",
                    params![account_id, contact_id],
                )
                .context("failed to delete contact extension fields")?;
        }
        Ok(())
    }

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

    async fn put_with_items_and_clear(
        &self,
        record: SyncRecord,
        items: Option<Vec<ItemState>>,
        completed: Option<(String, Vec<ItemState>)>,
    ) -> anyhow::Result<()> {
        let seen_ids =
            serde_json::to_string(&record.seen_ids).context("failed to encode seen IDs")?;
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        let tx = connection
            .transaction()
            .context("failed to start Sync state transaction")?;
        tx.execute(
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
                seen_ids
            ],
        )
        .context("failed to write Sync state")?;
        if let Some(items) = items {
            tx.execute(
                "DELETE FROM item_states WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
                params![record.user, record.device_id, record.collection_id],
            )
            .context("failed to clear previous item state")?;
            for item in &items {
                tx.execute(
                    "INSERT INTO item_states (user, device_id, collection_id, item_id, item_hash, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())",
                    params![record.user, record.device_id, record.collection_id, item.item_id, item.hash],
                ).context("failed to write item state")?;
            }
        }
        if let Some((journal, completed)) = completed {
            for item in completed {
                tx.execute(
                    "DELETE FROM item_states WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3 AND item_id = ?4 AND item_hash = ?5",
                    params![record.user, record.device_id, journal, item.item_id, item.hash],
                ).context("failed to clear completed operation")?;
            }
        }
        tx.commit()
            .context("failed to commit Sync state transaction")?;
        Ok(())
    }

    async fn draft_mappings(&self, account: &str) -> anyhow::Result<Vec<ItemState>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        let mut query = connection
            .prepare("SELECT stable_id, backend_id FROM draft_identities WHERE account_id = ?1")?;
        let rows = query.query_map(params![account], |row| {
            Ok(ItemState {
                item_id: row.get(0)?,
                hash: row.get(1)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("failed to read draft identities")
    }

    async fn swap_draft_mapping(
        &self,
        account: &str,
        stable_id: &str,
        expected_backend: &str,
        new_backend: &str,
    ) -> anyhow::Result<bool> {
        anyhow::ensure!(
            !account.is_empty()
                && !stable_id.is_empty()
                && !expected_backend.is_empty()
                && !new_backend.is_empty(),
            "invalid draft identity"
        );
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        let tx = connection.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let current: Option<String> = tx
            .query_row(
                "SELECT backend_id FROM draft_identities WHERE account_id = ?1 AND stable_id = ?2",
                params![account, stable_id],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref().unwrap_or(stable_id) != expected_backend {
            return Ok(false);
        }
        let conflict: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM draft_identities WHERE account_id = ?1 AND stable_id != ?2 AND (backend_id = ?3 OR stable_id = ?3 OR backend_id = ?2))",
            params![account,stable_id,new_backend], |row| row.get(0))?;
        if conflict {
            anyhow::bail!("conflicting draft identity");
        }
        if stable_id == new_backend {
            tx.execute(
                "DELETE FROM draft_identities WHERE account_id = ?1 AND stable_id = ?2",
                params![account, stable_id],
            )?;
        } else {
            tx.execute(
                "INSERT INTO draft_identities (account_id,stable_id,backend_id) VALUES (?1,?2,?3)
                ON CONFLICT(account_id,stable_id) DO UPDATE SET backend_id = excluded.backend_id",
                params![account, stable_id, new_backend],
            )?;
        }
        tx.commit()?;
        Ok(true)
    }

    async fn reserve_item(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        sync_key: &str,
        journal_id: &str,
        item: ItemState,
    ) -> anyhow::Result<Option<ItemState>> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        let tx = connection
            .transaction()
            .context("failed to start operation reservation")?;
        let current: Option<String> = tx.query_row(
            "SELECT sync_key FROM sync_records WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
            params![user, device_id, collection_id], |row| row.get(0)
        ).optional().context("failed to read operation Sync key")?;
        if current.as_deref() != Some(sync_key) {
            return Ok(None);
        }
        tx.execute(
            "INSERT INTO item_states (user, device_id, collection_id, item_id, item_hash, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, unixepoch()) ON CONFLICT DO NOTHING",
            params![user, device_id, journal_id, item.item_id, item.hash]
        ).context("failed to reserve operation")?;
        let hash: String = tx.query_row(
            "SELECT item_hash FROM item_states WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3 AND item_id = ?4",
            params![user, device_id, journal_id, item.item_id], |row| row.get(0)
        ).context("failed to read reserved operation")?;
        tx.commit()
            .context("failed to commit operation reservation")?;
        Ok(Some(ItemState {
            item_id: item.item_id,
            hash,
        }))
    }

    async fn item_states(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
    ) -> anyhow::Result<Vec<ItemState>> {
        let connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        let mut statement = connection.prepare(
            "SELECT item_id, item_hash FROM item_states
             WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
        )?;
        let rows = statement
            .query_map(params![user, device_id, collection_id], |row| {
                Ok(ItemState {
                    item_id: row.get(0)?,
                    hash: row.get(1)?,
                })
            })
            .context("failed to read item state")?;
        rows.collect::<Result<Vec<_>, _>>()
            .context("failed to read item state rows")
    }

    async fn put_item_states(
        &self,
        user: &str,
        device_id: &str,
        collection_id: &str,
        items: Vec<ItemState>,
    ) -> anyhow::Result<()> {
        let mut connection = self
            .connection
            .lock()
            .map_err(|_| anyhow::anyhow!("SQLite state lock poisoned"))?;
        let tx = connection
            .transaction()
            .context("failed to start item state transaction")?;
        tx.execute(
            "DELETE FROM item_states WHERE user = ?1 AND device_id = ?2 AND collection_id = ?3",
            params![user, device_id, collection_id],
        )
        .context("failed to clear previous item state")?;
        for item in &items {
            tx.execute(
                "INSERT INTO item_states (user, device_id, collection_id, item_id, item_hash, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, unixepoch())",
                params![user, device_id, collection_id, item.item_id, item.hash],
            )
            .context("failed to write item state")?;
        }
        tx.commit().context("failed to commit item state")?;
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

        CREATE TABLE IF NOT EXISTS item_states (
            user TEXT NOT NULL,
            device_id TEXT NOT NULL,
            collection_id TEXT NOT NULL,
            item_id TEXT NOT NULL,
            item_hash TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (user, device_id, collection_id, item_id)
        );

        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (2, unixepoch());

        CREATE TABLE IF NOT EXISTS draft_identities (
            account_id TEXT NOT NULL,
            stable_id TEXT NOT NULL,
            backend_id TEXT NOT NULL,
            PRIMARY KEY (account_id, stable_id),
            UNIQUE (account_id, backend_id)
        );

        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (3, unixepoch());

        CREATE TABLE IF NOT EXISTS contact_extensions (
            account_id TEXT NOT NULL,
            contact_id TEXT NOT NULL,
            fields_json TEXT NOT NULL,
            updated_at INTEGER NOT NULL,
            PRIMARY KEY (account_id, contact_id)
        );

        INSERT OR IGNORE INTO schema_migrations(version, applied_at)
        VALUES (4, unixepoch());
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

    #[tokio::test]
    async fn sqlite_contact_extensions_survive_reopen_and_clear() {
        let path = std::env::temp_dir().join(format!(
            "stalwart-sync-gateway-contact-state-{}.db",
            uuid::Uuid::new_v4()
        ));
        let store = std::sync::Arc::new(SqliteStateStore::open(path.to_str().unwrap()).unwrap());
        let fields = serde_json::json!({"assistantName":"Grace Hopper"});
        store
            .put_contact_extensions("account-a", "contact-1", Some(fields.clone()))
            .await
            .unwrap();
        drop(store);
        let reopened = std::sync::Arc::new(SqliteStateStore::open(path.to_str().unwrap()).unwrap());
        assert_eq!(
            reopened
                .contact_extensions("account-a", "contact-1")
                .await
                .unwrap(),
            Some(fields)
        );
        exercise_contact_extensions(reopened).await;
        let _ = std::fs::remove_file(&path);
    }

    async fn exercise_contact_extensions(store: std::sync::Arc<dyn StateStore>) {
        let fields = serde_json::json!({
            "assistantName":"Grace Hopper",
            "spouse":"Alex Lovelace",
            "children":["Byron Ada", "Byron Ralph"],
            "customerId":"customer-314",
            "governmentId":"id-2718",
            "managerName":"CN=Grace Hopper,OU=People"
        });
        store
            .put_contact_extensions("account-a", "contact-1", Some(fields.clone()))
            .await
            .unwrap();
        assert_eq!(
            store
                .contact_extensions("account-a", "contact-1")
                .await
                .unwrap(),
            Some(fields)
        );
        assert!(store
            .contact_extensions("account-b", "contact-1")
            .await
            .unwrap()
            .is_none());
        store
            .put_contact_extensions("account-a", "contact-1", None)
            .await
            .unwrap();
        assert!(store
            .contact_extensions("account-a", "contact-1")
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn memory_contact_extensions_are_scoped_and_clearable() {
        exercise_contact_extensions(std::sync::Arc::new(MemoryStateStore::default())).await;
    }

    async fn exercise_operation_reservation(store: std::sync::Arc<dyn StateStore>) {
        store.put(record("1")).await.unwrap();
        let mut jobs = Vec::new();
        for number in 0..16 {
            let store = store.clone();
            jobs.push(tokio::spawn(async move {
                store
                    .reserve_item(
                        "user@example.com",
                        "device",
                        "inbox",
                        "1",
                        "__journal__",
                        ItemState {
                            item_id: "op".to_owned(),
                            hash: number.to_string(),
                        },
                    )
                    .await
                    .unwrap()
                    .unwrap()
            }));
        }
        let mut values = Vec::new();
        for job in jobs {
            values.push(job.await.unwrap().hash);
        }
        assert!(values.iter().all(|value| value == &values[0]));
        let saved = ItemState {
            item_id: "op".to_owned(),
            hash: values[0].clone(),
        };
        store
            .put_with_items_and_clear(
                record("2"),
                None,
                Some(("__journal__".to_owned(), vec![saved])),
            )
            .await
            .unwrap();
        assert!(store
            .item_states("user@example.com", "device", "__journal__")
            .await
            .unwrap()
            .is_empty());
        let fresh = store
            .reserve_item(
                "user@example.com",
                "device",
                "inbox",
                "2",
                "__journal__",
                ItemState {
                    item_id: "op".to_owned(),
                    hash: "fresh".to_owned(),
                },
            )
            .await
            .unwrap()
            .unwrap();
        assert_eq!(fresh.hash, "fresh");
        assert!(store
            .reserve_item(
                "user@example.com",
                "device",
                "inbox",
                "1",
                "__journal__",
                ItemState {
                    item_id: "new".to_owned(),
                    hash: "bad".to_owned()
                }
            )
            .await
            .unwrap()
            .is_none());
    }

    #[tokio::test]
    async fn operation_reservation_is_atomic_in_memory_and_sqlite() {
        exercise_operation_reservation(std::sync::Arc::new(MemoryStateStore::default())).await;
        let path =
            std::env::temp_dir().join(format!("stalwart-reservation-{}.db", uuid::Uuid::new_v4()));
        let store = std::sync::Arc::new(SqliteStateStore::open(path.to_str().unwrap()).unwrap());
        exercise_operation_reservation(store.clone()).await;
        drop(store);
        let _ = std::fs::remove_file(path);
    }

    async fn exercise_draft_mappings(store: std::sync::Arc<dyn StateStore>) {
        assert!(store.draft_mappings("a").await.unwrap().is_empty());
        assert!(store
            .swap_draft_mapping("a", "stable", "stable", "one")
            .await
            .unwrap());
        assert!(!store
            .swap_draft_mapping("a", "stable", "stable", "stale")
            .await
            .unwrap());
        assert!(store
            .swap_draft_mapping("b", "stable", "stable", "one")
            .await
            .unwrap());
        assert!(store
            .swap_draft_mapping("a", "other", "other", "one")
            .await
            .is_err());
        let mut jobs = Vec::new();
        for next in ["two", "three"] {
            let store = store.clone();
            jobs.push(tokio::spawn(async move {
                store
                    .swap_draft_mapping("a", "stable", "one", next)
                    .await
                    .unwrap()
            }));
        }
        let mut wins = 0;
        for job in jobs {
            wins += usize::from(job.await.unwrap());
        }
        assert_eq!(wins, 1);
        let mappings = store.draft_mappings("a").await.unwrap();
        assert_eq!(mappings.len(), 1);
        assert_eq!(mappings[0].item_id, "stable");
        assert!(["two", "three"].contains(&mappings[0].hash.as_str()));
        assert!(store
            .swap_draft_mapping("a", "stable", &mappings[0].hash, "stable")
            .await
            .unwrap());
        assert!(store.draft_mappings("a").await.unwrap().is_empty());
        assert_eq!(store.draft_mappings("b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn draft_mappings_are_atomic_and_account_scoped() {
        exercise_draft_mappings(std::sync::Arc::new(MemoryStateStore::default())).await;
        let path =
            std::env::temp_dir().join(format!("stalwart-draft-map-{}.db", uuid::Uuid::new_v4()));
        let store = std::sync::Arc::new(SqliteStateStore::open(path.to_str().unwrap()).unwrap());
        exercise_draft_mappings(store.clone()).await;
        drop(store);
        let reopened = SqliteStateStore::open(path.to_str().unwrap()).unwrap();
        assert_eq!(reopened.draft_mappings("b").await.unwrap()[0].hash, "one");
        drop(reopened);
        let _ = std::fs::remove_file(path);
    }

    #[tokio::test]
    async fn sqlite_sync_and_item_state_commit_together() {
        let path =
            std::env::temp_dir().join(format!("stalwart-atomic-state-{}.db", uuid::Uuid::new_v4()));
        let store = SqliteStateStore::open(path.to_str().unwrap()).unwrap();
        let item = ItemState {
            item_id: "a".to_owned(),
            hash: "old".to_owned(),
        };
        store
            .put_with_items(record("1"), Some(vec![item.clone()]))
            .await
            .unwrap();
        let reserved = store
            .reserve_item(
                "user@example.com",
                "device",
                "inbox",
                "1",
                "__journal__",
                ItemState {
                    item_id: "operation".to_owned(),
                    hash: "pending".to_owned(),
                },
            )
            .await
            .unwrap()
            .unwrap();
        let duplicate = ItemState {
            item_id: "a".to_owned(),
            hash: "new".to_owned(),
        };
        assert!(store
            .put_with_items_and_clear(
                record("2"),
                Some(vec![duplicate.clone(), duplicate]),
                Some(("__journal__".to_owned(), vec![reserved.clone()]))
            )
            .await
            .is_err());
        assert_eq!(
            store
                .item_states("user@example.com", "device", "__journal__")
                .await
                .unwrap(),
            vec![reserved]
        );
        assert_eq!(
            store
                .get("user@example.com", "device", "inbox")
                .await
                .unwrap(),
            Some(record("1"))
        );
        assert_eq!(
            store
                .item_states("user@example.com", "device", "inbox")
                .await
                .unwrap(),
            vec![item]
        );
        drop(store);
        let _ = std::fs::remove_file(path);
    }
}
