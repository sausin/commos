//! SQLite binding of [`Store`] — the **embedded, zero-external-dependency durable default**
//! (CMOS-14-DEP-021, refined by ADR-0012). One file, no server process: the single binary
//! is durable *and* dependency-free out of the box — ideal for a Raspberry Pi or a
//! small-business box. PostgreSQL remains the opt-in multi-node / HA backend
//! (CMOS-14-DEP-011/020); the two bindings are drop-in behind this trait (CMOS-14-DEP-042).
//!
//! **Kind to SD cards:** opened in WAL mode with `synchronous = NORMAL`, which batches
//! fsyncs hard. Combined with keeping ephemeral state (registrations, presence) out of the
//! durable store, steady-state write volume stays low so cheap flash survives for years.
//!
//! SQLite has no native `uuid`/`timestamptz`/`jsonb`, so ids/timestamps are stored as TEXT
//! and each entity as its contract JSON in a TEXT `data` column — a faithful, lossless
//! image, and UUIDv7 hex still sorts by creation time for keyset pagination.

use std::str::FromStr;
use std::time::Duration;

use axum::async_trait;
use sqlx::sqlite::{
    SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous,
};
use sqlx::{Row, SqlitePool};

use commos_core::common::{EntityBase, Uuid};
use commos_core::entities::call::Call;
use commos_core::entities::channel::Channel;
use commos_core::entities::message::Message;
use commos_core::entities::presence_state::PresenceState;
use commos_core::entities::thread::Thread;
use commos_core::entities::video_room::VideoRoom;

use super::{OutboxRecord, Page, Store, StoreError, Tx};

/// Schema, applied idempotently at boot. TEXT ids/timestamps/data; INTEGER versions.
/// Backward-compatible `IF NOT EXISTS` DDL (CMOS-14-DEP-052).
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS calls        (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, version INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS calls_tenant_id_idx ON calls (tenant_id, id);
CREATE TABLE IF NOT EXISTS channels     (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, version INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS channels_tenant_id_idx ON channels (tenant_id, id);
CREATE TABLE IF NOT EXISTS threads      (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, version INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS threads_tenant_id_idx ON threads (tenant_id, id);
CREATE TABLE IF NOT EXISTS messages     (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, version INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS messages_tenant_id_idx ON messages (tenant_id, id);
CREATE TABLE IF NOT EXISTS video_rooms  (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, version INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS video_rooms_tenant_id_idx ON video_rooms (tenant_id, id);
CREATE TABLE IF NOT EXISTS presence     (id TEXT PRIMARY KEY, tenant_id TEXT NOT NULL, version INTEGER NOT NULL, created_at TEXT NOT NULL, updated_at TEXT NOT NULL, data TEXT NOT NULL);
CREATE INDEX IF NOT EXISTS presence_tenant_id_idx ON presence (tenant_id, id);
CREATE TABLE IF NOT EXISTS idempotency_keys (tenant_id TEXT NOT NULL, key TEXT NOT NULL, call_id TEXT NOT NULL, PRIMARY KEY (tenant_id, key));
CREATE TABLE IF NOT EXISTS outbox        (seq INTEGER PRIMARY KEY AUTOINCREMENT, event TEXT NOT NULL, created_at TEXT NOT NULL DEFAULT (datetime('now')));
"#;

fn be<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

pub struct SqliteStore {
    pool: SqlitePool,
}

impl SqliteStore {
    /// Open (creating if needed) the database file at `path` and apply the schema.
    /// `path` is a filesystem path such as `commos.db` (an in-memory database is not used
    /// here — the ephemeral test binding is `MemStore`).
    pub async fn connect(path: &str) -> Result<Self, StoreError> {
        let opts = SqliteConnectOptions::from_str(path)
            .map_err(be)?
            .create_if_missing(true)
            .journal_mode(SqliteJournalMode::Wal)
            .synchronous(SqliteSynchronous::Normal)
            .busy_timeout(Duration::from_secs(5));
        // WAL allows concurrent readers with a single writer, which matches the
        // control-plane's serialized commit path.
        let pool = SqlitePoolOptions::new()
            .max_connections(5)
            .connect_with(opts)
            .await
            .map_err(be)?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await.map_err(be)?;
        Ok(SqliteStore { pool })
    }

    /// Insert a v0 entity; returns whether a row was inserted (0 ⇒ id already exists).
    async fn insert_v0(
        conn: &mut sqlx::SqliteConnection,
        table: &str,
        base: &EntityBase,
        data: &str,
    ) -> Result<u64, StoreError> {
        let sql = format!(
            "INSERT INTO {table} (id, tenant_id, version, created_at, updated_at, data) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6) ON CONFLICT(id) DO NOTHING"
        );
        let res = sqlx::query(&sql)
            .bind(base.id.to_string())
            .bind(base.tenant_id.to_string())
            .bind(base.version as i64)
            .bind(base.created_at.to_string())
            .bind(base.updated_at.to_string())
            .bind(data)
            .execute(conn)
            .await
            .map_err(be)?;
        Ok(res.rows_affected())
    }

    async fn get_one<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Option<T>, StoreError> {
        let sql = format!("SELECT data FROM {table} WHERE tenant_id = ?1 AND id = ?2");
        let row = sqlx::query(&sql)
            .bind(tenant.to_string())
            .bind(id.to_string())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        match row {
            Some(r) => {
                let data: String = r.try_get("data").map_err(be)?;
                Ok(Some(serde_json::from_str(&data).map_err(be)?))
            }
            None => Ok(None),
        }
    }

    async fn list<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<T>, StoreError> {
        // UUIDv7 ids are TEXT that sorts by creation time, so `id > cursor` is a stable keyset.
        let limit_i = limit as i64;
        let rows = match cursor {
            Some(c) => {
                let sql = format!(
                    "SELECT id, data FROM {table} WHERE tenant_id = ?1 AND id > ?2 \
                     ORDER BY id ASC LIMIT ?3"
                );
                sqlx::query(&sql)
                    .bind(tenant.to_string())
                    .bind(c)
                    .bind(limit_i)
                    .fetch_all(&self.pool)
                    .await
            }
            None => {
                let sql = format!(
                    "SELECT id, data FROM {table} WHERE tenant_id = ?1 ORDER BY id ASC LIMIT ?2"
                );
                sqlx::query(&sql)
                    .bind(tenant.to_string())
                    .bind(limit_i)
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(be)?;

        let mut items = Vec::with_capacity(rows.len());
        let mut last_id: Option<String> = None;
        for r in &rows {
            let data: String = r.try_get("data").map_err(be)?;
            items.push(serde_json::from_str(&data).map_err(be)?);
            last_id = Some(r.try_get("id").map_err(be)?);
        }
        let next_cursor = if items.len() == limit { last_id } else { None };
        Ok(Page { items, next_cursor })
    }
}

#[async_trait]
impl Store for SqliteStore {
    async fn commit(&self, tx: Tx) -> Result<(), StoreError> {
        let mut dbtx = self.pool.begin().await.map_err(be)?;

        for call in &tx.calls {
            let data = serde_json::to_string(call).map_err(be)?;
            if call.base.version == 0 {
                if Self::insert_v0(&mut dbtx, "calls", &call.base, &data).await? == 0 {
                    return Err(StoreError::VersionConflict {
                        entity: "Call",
                        id: call.base.id.to_string(),
                        expected: 0,
                    });
                }
            } else {
                let res = sqlx::query(
                    "UPDATE calls SET version = ?1, updated_at = ?2, data = ?3 \
                     WHERE id = ?4 AND tenant_id = ?5 AND version = ?6",
                )
                .bind(call.base.version as i64)
                .bind(call.base.updated_at.to_string())
                .bind(&data)
                .bind(call.base.id.to_string())
                .bind(call.base.tenant_id.to_string())
                .bind((call.base.version - 1) as i64)
                .execute(&mut *dbtx)
                .await
                .map_err(be)?;
                if res.rows_affected() == 0 {
                    return Err(StoreError::VersionConflict {
                        entity: "Call",
                        id: call.base.id.to_string(),
                        expected: call.base.version,
                    });
                }
            }
        }

        // Messaging + real-time entities are created at v0 (id collision ⇒ conflict).
        for c in &tx.channels {
            let data = serde_json::to_string(c).map_err(be)?;
            if Self::insert_v0(&mut dbtx, "channels", &c.base, &data).await? == 0 {
                return Err(StoreError::VersionConflict { entity: "Channel", id: c.base.id.to_string(), expected: 0 });
            }
        }
        for t in &tx.threads {
            let data = serde_json::to_string(t).map_err(be)?;
            if Self::insert_v0(&mut dbtx, "threads", &t.base, &data).await? == 0 {
                return Err(StoreError::VersionConflict { entity: "Thread", id: t.base.id.to_string(), expected: 0 });
            }
        }
        for m in &tx.messages {
            let data = serde_json::to_string(m).map_err(be)?;
            if Self::insert_v0(&mut dbtx, "messages", &m.base, &data).await? == 0 {
                return Err(StoreError::VersionConflict { entity: "Message", id: m.base.id.to_string(), expected: 0 });
            }
        }
        for v in &tx.video_rooms {
            let data = serde_json::to_string(v).map_err(be)?;
            if Self::insert_v0(&mut dbtx, "video_rooms", &v.base, &data).await? == 0 {
                return Err(StoreError::VersionConflict { entity: "VideoRoom", id: v.base.id.to_string(), expected: 0 });
            }
        }
        for p in &tx.presence {
            let data = serde_json::to_string(p).map_err(be)?;
            if Self::insert_v0(&mut dbtx, "presence", &p.base, &data).await? == 0 {
                return Err(StoreError::VersionConflict { entity: "PresenceState", id: p.base.id.to_string(), expected: 0 });
            }
        }

        if let Some((tenant, key, call_id)) = &tx.idempotency {
            sqlx::query(
                "INSERT INTO idempotency_keys (tenant_id, key, call_id) \
                 VALUES (?1, ?2, ?3) ON CONFLICT DO NOTHING",
            )
            .bind(tenant.to_string())
            .bind(key)
            .bind(call_id.to_string())
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
        }

        for event in &tx.events {
            let ev = serde_json::to_string(event).map_err(be)?;
            sqlx::query("INSERT INTO outbox (event) VALUES (?1)")
                .bind(ev)
                .execute(&mut *dbtx)
                .await
                .map_err(be)?;
        }

        dbtx.commit().await.map_err(be)?;
        Ok(())
    }

    async fn get_call(&self, tenant: Uuid, id: Uuid) -> Result<Option<Call>, StoreError> {
        self.get_one("calls", tenant, id).await
    }
    async fn list_calls(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Call>, StoreError> {
        self.list("calls", tenant, limit, cursor).await
    }

    async fn get_channel(&self, tenant: Uuid, id: Uuid) -> Result<Option<Channel>, StoreError> {
        self.get_one("channels", tenant, id).await
    }
    async fn list_channels(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Channel>, StoreError> {
        self.list("channels", tenant, limit, cursor).await
    }

    async fn get_thread(&self, tenant: Uuid, id: Uuid) -> Result<Option<Thread>, StoreError> {
        self.get_one("threads", tenant, id).await
    }
    async fn list_threads(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Thread>, StoreError> {
        self.list("threads", tenant, limit, cursor).await
    }

    async fn get_message(&self, tenant: Uuid, id: Uuid) -> Result<Option<Message>, StoreError> {
        self.get_one("messages", tenant, id).await
    }
    async fn list_messages(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Message>, StoreError> {
        self.list("messages", tenant, limit, cursor).await
    }

    async fn get_video_room(&self, tenant: Uuid, id: Uuid) -> Result<Option<VideoRoom>, StoreError> {
        self.get_one("video_rooms", tenant, id).await
    }
    async fn list_video_rooms(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<VideoRoom>, StoreError> {
        self.list("video_rooms", tenant, limit, cursor).await
    }

    async fn get_presence(&self, tenant: Uuid, id: Uuid) -> Result<Option<PresenceState>, StoreError> {
        self.get_one("presence", tenant, id).await
    }
    async fn list_presence(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<PresenceState>, StoreError> {
        self.list("presence", tenant, limit, cursor).await
    }

    async fn call_for_idempotency_key(&self, tenant: Uuid, key: &str) -> Result<Option<Uuid>, StoreError> {
        let row = sqlx::query("SELECT call_id FROM idempotency_keys WHERE tenant_id = ?1 AND key = ?2")
            .bind(tenant.to_string())
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        match row {
            Some(r) => {
                let s: String = r.try_get("call_id").map_err(be)?;
                Ok(Some(Uuid::parse(&s).map_err(be)?))
            }
            None => Ok(None),
        }
    }

    async fn peek_outbox(&self, max: usize) -> Result<Vec<OutboxRecord>, StoreError> {
        let lim = max.min(10_000) as i64;
        let rows = sqlx::query("SELECT seq, event FROM outbox ORDER BY seq ASC LIMIT ?1")
            .bind(lim)
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        rows.iter()
            .map(|r| {
                let seq: i64 = r.try_get("seq").map_err(be)?;
                let ev: String = r.try_get("event").map_err(be)?;
                Ok(OutboxRecord { seq: seq as u64, event: serde_json::from_str(&ev).map_err(be)? })
            })
            .collect()
    }

    async fn ack_outbox(&self, up_to_seq: u64) -> Result<(), StoreError> {
        sqlx::query("DELETE FROM outbox WHERE seq <= ?1")
            .bind(up_to_seq as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }
}
