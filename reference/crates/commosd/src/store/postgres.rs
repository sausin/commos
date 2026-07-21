//! PostgreSQL binding of [`Store`] — the durable system of record (CMOS-14-DEP-020;
//! CMOS-00-ENG-007). The transactional-outbox guarantee is a real `BEGIN … COMMIT`:
//! entity upserts and their events land in one database transaction, so a crash can never
//! leave a state change without its event (CMOS-03-ARCH-030 / CMOS-05-EVT-010).
//!
//! Entities are stored as their contract JSON in a `jsonb` column, with the identity /
//! version / timestamp fields promoted to typed columns for querying and optimistic
//! concurrency. This keeps the row a faithful, lossless image of the frozen entity.

use axum::async_trait;
use sqlx::postgres::PgPoolOptions;
use sqlx::{PgPool, Row};

use commos_core::common::{EntityBase, Uuid};
use commos_core::entities::call::Call;
use commos_core::entities::cdr::Cdr;
use commos_core::entities::channel::Channel;
use commos_core::entities::device::Device;
use commos_core::entities::extension::Extension;
use commos_core::entities::message::Message;
use commos_core::entities::object::Object;
use commos_core::entities::presence_state::PresenceState;
use commos_core::entities::queue::Queue;
use commos_core::entities::recording::Recording;
use commos_core::entities::route::Route;
use commos_core::entities::thread::Thread;
use commos_core::entities::user::User;
use commos_core::entities::video_room::VideoRoom;
use commos_core::entities::webhook::Webhook;

use super::{OutboxRecord, Page, Store, StoreError, Tx};

/// Deserialise an entity from its `data` jsonb column (the lossless contract image).
fn entity_from_row<T: serde::de::DeserializeOwned>(
    row: &sqlx::postgres::PgRow,
) -> Result<T, StoreError> {
    let data: serde_json::Value = row.try_get("data").map_err(be)?;
    serde_json::from_value(data).map_err(be)
}

/// Schema, applied idempotently at boot. Backward-compatible `IF NOT EXISTS` DDL so old and
/// new binaries can run concurrently during a rolling upgrade (CMOS-14-DEP-052).
const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS calls (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS calls_tenant_id_idx ON calls (tenant_id, id);

CREATE TABLE IF NOT EXISTS channels (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS channels_tenant_id_idx ON channels (tenant_id, id);

CREATE TABLE IF NOT EXISTS threads (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS threads_tenant_id_idx ON threads (tenant_id, id);

CREATE TABLE IF NOT EXISTS messages (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS messages_tenant_id_idx ON messages (tenant_id, id);

CREATE TABLE IF NOT EXISTS video_rooms (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS video_rooms_tenant_id_idx ON video_rooms (tenant_id, id);

CREATE TABLE IF NOT EXISTS presence (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS presence_tenant_id_idx ON presence (tenant_id, id);

CREATE TABLE IF NOT EXISTS cdrs (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS cdrs_tenant_id_idx ON cdrs (tenant_id, id);

CREATE TABLE IF NOT EXISTS queues (
    id          uuid PRIMARY KEY,
    tenant_id   uuid NOT NULL,
    version     bigint NOT NULL,
    created_at  timestamptz NOT NULL,
    updated_at  timestamptz NOT NULL,
    data        jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS queues_tenant_id_idx ON queues (tenant_id, id);

CREATE TABLE IF NOT EXISTS users (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL, version bigint NOT NULL,
    created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, data jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS users_tenant_id_idx ON users (tenant_id, id);

CREATE TABLE IF NOT EXISTS extensions (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL, version bigint NOT NULL,
    created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, data jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS extensions_tenant_id_idx ON extensions (tenant_id, id);

CREATE TABLE IF NOT EXISTS devices (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL, version bigint NOT NULL,
    created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, data jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS devices_tenant_id_idx ON devices (tenant_id, id);

CREATE TABLE IF NOT EXISTS routes (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL, version bigint NOT NULL,
    created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, data jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS routes_tenant_id_idx ON routes (tenant_id, id);

CREATE TABLE IF NOT EXISTS webhooks (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL, version bigint NOT NULL,
    created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, data jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS webhooks_tenant_id_idx ON webhooks (tenant_id, id);

CREATE TABLE IF NOT EXISTS objects (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL, version bigint NOT NULL,
    created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, data jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS objects_tenant_id_idx ON objects (tenant_id, id);

CREATE TABLE IF NOT EXISTS recordings (
    id uuid PRIMARY KEY, tenant_id uuid NOT NULL, version bigint NOT NULL,
    created_at timestamptz NOT NULL, updated_at timestamptz NOT NULL, data jsonb NOT NULL
);
CREATE INDEX IF NOT EXISTS recordings_tenant_id_idx ON recordings (tenant_id, id);

CREATE TABLE IF NOT EXISTS sip_credentials (
    tenant_id  uuid NOT NULL,
    username   text NOT NULL,
    secret     text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, username)
);

CREATE TABLE IF NOT EXISTS idempotency_keys (
    tenant_id   uuid NOT NULL,
    key         text NOT NULL,
    call_id     uuid NOT NULL,
    PRIMARY KEY (tenant_id, key)
);

CREATE TABLE IF NOT EXISTS outbox (
    seq         bigserial PRIMARY KEY,
    event       jsonb NOT NULL,
    created_at  timestamptz NOT NULL DEFAULT now()
);
"#;

/// Turn any sqlx/serde error into a backend `StoreError`.
fn be<E: std::fmt::Display>(e: E) -> StoreError {
    StoreError::Backend(e.to_string())
}

/// Insert a v0 entity into `table`; an id collision is a version conflict.
async fn insert_v0(
    conn: &mut sqlx::PgConnection,
    table: &str,
    base: &EntityBase,
    data: &serde_json::Value,
    entity: &'static str,
) -> Result<(), StoreError> {
    let sql = format!(
        "INSERT INTO {table} (id, tenant_id, version, created_at, updated_at, data) \
         VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING"
    );
    let res = sqlx::query(&sql)
        .bind(base.id.as_uuid())
        .bind(base.tenant_id.as_uuid())
        .bind(base.version as i64)
        .bind(base.created_at.into_offset())
        .bind(base.updated_at.into_offset())
        .bind(data)
        .execute(conn)
        .await
        .map_err(be)?;
    if res.rows_affected() == 0 {
        return Err(StoreError::VersionConflict { entity, id: base.id.to_string(), expected: 0 });
    }
    Ok(())
}

/// Version-aware upsert for entities that support in-place update (config re-import
/// reconciles by natural key and bumps the version). A v0 create inserts; a v>0 update
/// rewrites the row only if the stored version is exactly one behind — the same
/// optimistic-concurrency guard `Call` uses.
async fn upsert(
    conn: &mut sqlx::PgConnection,
    table: &str,
    entity: &'static str,
    base: &EntityBase,
    data: &serde_json::Value,
) -> Result<(), StoreError> {
    if base.version == 0 {
        return insert_v0(conn, table, base, data, entity).await;
    }
    let sql = format!(
        "UPDATE {table} SET version = $1, updated_at = $2, data = $3 \
         WHERE id = $4 AND tenant_id = $5 AND version = $6"
    );
    let res = sqlx::query(&sql)
        .bind(base.version as i64)
        .bind(base.updated_at.into_offset())
        .bind(data)
        .bind(base.id.as_uuid())
        .bind(base.tenant_id.as_uuid())
        .bind((base.version - 1) as i64)
        .execute(conn)
        .await
        .map_err(be)?;
    if res.rows_affected() == 0 {
        return Err(StoreError::VersionConflict { entity, id: base.id.to_string(), expected: base.version });
    }
    Ok(())
}

pub struct PgStore {
    pool: PgPool,
}

impl PgStore {
    /// Connect and apply the schema. The pool is bounded so a small box (a Raspberry Pi)
    /// doesn't exhaust Postgres connections.
    pub async fn connect(dsn: &str) -> Result<Self, StoreError> {
        let pool = PgPoolOptions::new()
            .max_connections(10)
            .connect(dsn)
            .await
            .map_err(be)?;
        sqlx::raw_sql(SCHEMA).execute(&pool).await.map_err(be)?;
        Ok(PgStore { pool })
    }

    fn call_from_row(row: &sqlx::postgres::PgRow) -> Result<Call, StoreError> {
        let data: serde_json::Value = row.try_get("data").map_err(be)?;
        serde_json::from_value(data).map_err(be)
    }

    /// Keyset-paginated read of a messaging entity table (`table` is a fixed literal from
    /// this module, never caller input). UUIDv7 ids sort by creation time, so `id > cursor`
    /// resumes strictly after the last row — the same stable cursor as `list_calls`.
    async fn list_entities<T: serde::de::DeserializeOwned>(
        &self,
        table: &str,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Vec<T>, StoreError> {
        let limit_i = limit as i64;
        let rows = match cursor {
            Some(c) => {
                let cur = uuid::Uuid::parse_str(&c)
                    .map_err(|_| StoreError::Backend("invalid cursor".into()))?;
                let sql = format!(
                    "SELECT data FROM {table} WHERE tenant_id = $1 AND id > $2 \
                     ORDER BY id ASC LIMIT $3"
                );
                sqlx::query(&sql)
                    .bind(tenant.as_uuid())
                    .bind(cur)
                    .bind(limit_i)
                    .fetch_all(&self.pool)
                    .await
            }
            None => {
                let sql = format!(
                    "SELECT data FROM {table} WHERE tenant_id = $1 ORDER BY id ASC LIMIT $2"
                );
                sqlx::query(&sql)
                    .bind(tenant.as_uuid())
                    .bind(limit_i)
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(be)?;

        rows.iter().map(entity_from_row).collect()
    }
}

#[async_trait]
impl Store for PgStore {
    async fn commit(&self, tx: Tx) -> Result<(), StoreError> {
        // One database transaction. Dropping `dbtx` on any early return rolls it back, so
        // the whole batch is all-or-nothing (CMOS-03-ARCH-030).
        let mut dbtx = self.pool.begin().await.map_err(be)?;

        for call in &tx.calls {
            let data = serde_json::to_value(call).map_err(be)?;
            let id = call.base.id.as_uuid();
            let tenant = call.base.tenant_id.as_uuid();
            let version = call.base.version as i64;
            let created = call.base.created_at.into_offset();
            let updated = call.base.updated_at.into_offset();

            if call.base.version == 0 {
                // First write: a plain insert. A colliding id means someone else created it.
                let res = sqlx::query(
                    "INSERT INTO calls (id, tenant_id, version, created_at, updated_at, data) \
                     VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
                )
                .bind(id)
                .bind(tenant)
                .bind(version)
                .bind(created)
                .bind(updated)
                .bind(&data)
                .execute(&mut *dbtx)
                .await
                .map_err(be)?;
                if res.rows_affected() == 0 {
                    return Err(StoreError::VersionConflict {
                        entity: "Call",
                        id: call.base.id.to_string(),
                        expected: 0,
                    });
                }
            } else {
                // Update guarded by the prior version: optimistic concurrency (CMOS-02-DOM-005).
                let res = sqlx::query(
                    "UPDATE calls SET version = $1, updated_at = $2, data = $3 \
                     WHERE id = $4 AND tenant_id = $5 AND version = $6",
                )
                .bind(version)
                .bind(updated)
                .bind(&data)
                .bind(id)
                .bind(tenant)
                .bind(version - 1)
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

        // Messaging entities are created at version 0 → a plain insert. A colliding id
        // means someone else already created it (mirrors the Call v0 path).
        for ch in &tx.channels {
            let data = serde_json::to_value(ch).map_err(be)?;
            let res = sqlx::query(
                "INSERT INTO channels (id, tenant_id, version, created_at, updated_at, data) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
            )
            .bind(ch.base.id.as_uuid())
            .bind(ch.base.tenant_id.as_uuid())
            .bind(ch.base.version as i64)
            .bind(ch.base.created_at.into_offset())
            .bind(ch.base.updated_at.into_offset())
            .bind(&data)
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
            if res.rows_affected() == 0 {
                return Err(StoreError::VersionConflict {
                    entity: "Channel",
                    id: ch.base.id.to_string(),
                    expected: 0,
                });
            }
        }

        for th in &tx.threads {
            let data = serde_json::to_value(th).map_err(be)?;
            let res = sqlx::query(
                "INSERT INTO threads (id, tenant_id, version, created_at, updated_at, data) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
            )
            .bind(th.base.id.as_uuid())
            .bind(th.base.tenant_id.as_uuid())
            .bind(th.base.version as i64)
            .bind(th.base.created_at.into_offset())
            .bind(th.base.updated_at.into_offset())
            .bind(&data)
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
            if res.rows_affected() == 0 {
                return Err(StoreError::VersionConflict {
                    entity: "Thread",
                    id: th.base.id.to_string(),
                    expected: 0,
                });
            }
        }

        for m in &tx.messages {
            let data = serde_json::to_value(m).map_err(be)?;
            let res = sqlx::query(
                "INSERT INTO messages (id, tenant_id, version, created_at, updated_at, data) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
            )
            .bind(m.base.id.as_uuid())
            .bind(m.base.tenant_id.as_uuid())
            .bind(m.base.version as i64)
            .bind(m.base.created_at.into_offset())
            .bind(m.base.updated_at.into_offset())
            .bind(&data)
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
            if res.rows_affected() == 0 {
                return Err(StoreError::VersionConflict {
                    entity: "Message",
                    id: m.base.id.to_string(),
                    expected: 0,
                });
            }
        }

        // Real-time entities are created at version 0 → a plain insert. A colliding id
        // means someone else already created it (mirrors the Call v0 path).
        for vr in &tx.video_rooms {
            let data = serde_json::to_value(vr).map_err(be)?;
            let res = sqlx::query(
                "INSERT INTO video_rooms (id, tenant_id, version, created_at, updated_at, data) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
            )
            .bind(vr.base.id.as_uuid())
            .bind(vr.base.tenant_id.as_uuid())
            .bind(vr.base.version as i64)
            .bind(vr.base.created_at.into_offset())
            .bind(vr.base.updated_at.into_offset())
            .bind(&data)
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
            if res.rows_affected() == 0 {
                return Err(StoreError::VersionConflict {
                    entity: "VideoRoom",
                    id: vr.base.id.to_string(),
                    expected: 0,
                });
            }
        }

        // PresenceState is inserted at version 0 for the MVP (a fuller upsert-by-subject is
        // a later refinement); a colliding id is a conflict.
        for p in &tx.presence {
            let data = serde_json::to_value(p).map_err(be)?;
            let res = sqlx::query(
                "INSERT INTO presence (id, tenant_id, version, created_at, updated_at, data) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
            )
            .bind(p.base.id.as_uuid())
            .bind(p.base.tenant_id.as_uuid())
            .bind(p.base.version as i64)
            .bind(p.base.created_at.into_offset())
            .bind(p.base.updated_at.into_offset())
            .bind(&data)
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
            if res.rows_affected() == 0 {
                return Err(StoreError::VersionConflict {
                    entity: "PresenceState",
                    id: p.base.id.to_string(),
                    expected: 0,
                });
            }
        }

        for c in &tx.cdrs {
            let data = serde_json::to_value(c).map_err(be)?;
            let res = sqlx::query(
                "INSERT INTO cdrs (id, tenant_id, version, created_at, updated_at, data) \
                 VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT (id) DO NOTHING",
            )
            .bind(c.base.id.as_uuid())
            .bind(c.base.tenant_id.as_uuid())
            .bind(c.base.version as i64)
            .bind(c.base.created_at.into_offset())
            .bind(c.base.updated_at.into_offset())
            .bind(&data)
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
            if res.rows_affected() == 0 {
                return Err(StoreError::VersionConflict { entity: "CDR", id: c.base.id.to_string(), expected: 0 });
            }
        }

        // Provisioning entities support version-aware update (config re-import reconciles
        // by natural key and bumps the version).
        for q in &tx.queues {
            upsert(&mut dbtx, "queues", "Queue", &q.base, &serde_json::to_value(q).map_err(be)?).await?;
        }
        for u in &tx.users {
            upsert(&mut dbtx, "users", "User", &u.base, &serde_json::to_value(u).map_err(be)?).await?;
        }
        for e in &tx.extensions {
            upsert(&mut dbtx, "extensions", "Extension", &e.base, &serde_json::to_value(e).map_err(be)?).await?;
        }
        for d in &tx.devices {
            upsert(&mut dbtx, "devices", "Device", &d.base, &serde_json::to_value(d).map_err(be)?).await?;
        }
        for r in &tx.routes {
            upsert(&mut dbtx, "routes", "Route", &r.base, &serde_json::to_value(r).map_err(be)?).await?;
        }
        for w in &tx.webhooks {
            upsert(&mut dbtx, "webhooks", "Webhook", &w.base, &serde_json::to_value(w).map_err(be)?).await?;
        }
        for o in &tx.objects {
            upsert(&mut dbtx, "objects", "Object", &o.base, &serde_json::to_value(o).map_err(be)?).await?;
        }
        for r in &tx.recordings {
            insert_v0(&mut dbtx, "recordings", &r.base, &serde_json::to_value(r).map_err(be)?, "Recording").await?;
        }

        if let Some((tenant, key, call_id)) = &tx.idempotency {
            sqlx::query(
                "INSERT INTO idempotency_keys (tenant_id, key, call_id) \
                 VALUES ($1, $2, $3) ON CONFLICT DO NOTHING",
            )
            .bind(tenant.as_uuid())
            .bind(key)
            .bind(call_id.as_uuid())
            .execute(&mut *dbtx)
            .await
            .map_err(be)?;
        }

        for event in &tx.events {
            sqlx::query("INSERT INTO outbox (event) VALUES ($1)")
                .bind(event)
                .execute(&mut *dbtx)
                .await
                .map_err(be)?;
        }

        dbtx.commit().await.map_err(be)?;
        Ok(())
    }

    async fn get_call(&self, tenant: Uuid, id: Uuid) -> Result<Option<Call>, StoreError> {
        let row = sqlx::query("SELECT data FROM calls WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(Self::call_from_row).transpose()
    }

    async fn list_calls(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Call>, StoreError> {
        // UUIDv7 ids sort by creation time, so `id > cursor` resumes strictly after the last
        // row returned — a stable keyset cursor.
        let limit_i = limit as i64;
        let rows = match cursor {
            Some(c) => {
                let cur = uuid::Uuid::parse_str(&c)
                    .map_err(|_| StoreError::Backend("invalid cursor".into()))?;
                sqlx::query(
                    "SELECT data FROM calls WHERE tenant_id = $1 AND id > $2 \
                     ORDER BY id ASC LIMIT $3",
                )
                .bind(tenant.as_uuid())
                .bind(cur)
                .bind(limit_i)
                .fetch_all(&self.pool)
                .await
            }
            None => {
                sqlx::query("SELECT data FROM calls WHERE tenant_id = $1 ORDER BY id ASC LIMIT $2")
                    .bind(tenant.as_uuid())
                    .bind(limit_i)
                    .fetch_all(&self.pool)
                    .await
            }
        }
        .map_err(be)?;

        let items = rows
            .iter()
            .map(Self::call_from_row)
            .collect::<Result<Vec<_>, _>>()?;
        // Offer a cursor only when the page was full (more may remain).
        let next_cursor = if items.len() == limit {
            items.last().map(|c| c.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_channel(&self, tenant: Uuid, id: Uuid) -> Result<Option<Channel>, StoreError> {
        let row = sqlx::query("SELECT data FROM channels WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }

    async fn list_channels(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Channel>, StoreError> {
        let items = self
            .list_entities::<Channel>("channels", tenant, limit, cursor)
            .await?;
        let next_cursor = if items.len() == limit {
            items.last().map(|c| c.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_thread(&self, tenant: Uuid, id: Uuid) -> Result<Option<Thread>, StoreError> {
        let row = sqlx::query("SELECT data FROM threads WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }

    async fn list_threads(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Thread>, StoreError> {
        let items = self
            .list_entities::<Thread>("threads", tenant, limit, cursor)
            .await?;
        let next_cursor = if items.len() == limit {
            items.last().map(|t| t.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_message(&self, tenant: Uuid, id: Uuid) -> Result<Option<Message>, StoreError> {
        let row = sqlx::query("SELECT data FROM messages WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }

    async fn list_messages(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<Message>, StoreError> {
        let items = self
            .list_entities::<Message>("messages", tenant, limit, cursor)
            .await?;
        let next_cursor = if items.len() == limit {
            items.last().map(|m| m.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_video_room(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Option<VideoRoom>, StoreError> {
        let row = sqlx::query("SELECT data FROM video_rooms WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }

    async fn list_video_rooms(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<VideoRoom>, StoreError> {
        let items = self
            .list_entities::<VideoRoom>("video_rooms", tenant, limit, cursor)
            .await?;
        let next_cursor = if items.len() == limit {
            items.last().map(|v| v.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_presence(
        &self,
        tenant: Uuid,
        id: Uuid,
    ) -> Result<Option<PresenceState>, StoreError> {
        let row = sqlx::query("SELECT data FROM presence WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }

    async fn list_presence(
        &self,
        tenant: Uuid,
        limit: usize,
        cursor: Option<String>,
    ) -> Result<Page<PresenceState>, StoreError> {
        let items = self
            .list_entities::<PresenceState>("presence", tenant, limit, cursor)
            .await?;
        let next_cursor = if items.len() == limit {
            items.last().map(|p| p.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_cdr(&self, tenant: Uuid, id: Uuid) -> Result<Option<Cdr>, StoreError> {
        let row = sqlx::query("SELECT data FROM cdrs WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }

    async fn list_cdrs(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Cdr>, StoreError> {
        let items = self.list_entities::<Cdr>("cdrs", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit {
            items.last().map(|c| c.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_queue(&self, tenant: Uuid, id: Uuid) -> Result<Option<Queue>, StoreError> {
        let row = sqlx::query("SELECT data FROM queues WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid())
            .bind(id.as_uuid())
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }

    async fn list_queues(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Queue>, StoreError> {
        let items = self.list_entities::<Queue>("queues", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit {
            items.last().map(|q| q.base.id.to_string())
        } else {
            None
        };
        Ok(Page { items, next_cursor })
    }

    async fn get_user(&self, tenant: Uuid, id: Uuid) -> Result<Option<User>, StoreError> {
        let row = sqlx::query("SELECT data FROM users WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }
    async fn list_users(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<User>, StoreError> {
        let items = self.list_entities::<User>("users", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit { items.last().map(|u| u.base.id.to_string()) } else { None };
        Ok(Page { items, next_cursor })
    }

    async fn get_extension(&self, tenant: Uuid, id: Uuid) -> Result<Option<Extension>, StoreError> {
        let row = sqlx::query("SELECT data FROM extensions WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }
    async fn list_extensions(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Extension>, StoreError> {
        let items = self.list_entities::<Extension>("extensions", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit { items.last().map(|e| e.base.id.to_string()) } else { None };
        Ok(Page { items, next_cursor })
    }

    async fn get_device(&self, tenant: Uuid, id: Uuid) -> Result<Option<Device>, StoreError> {
        let row = sqlx::query("SELECT data FROM devices WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }
    async fn list_devices(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Device>, StoreError> {
        let items = self.list_entities::<Device>("devices", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit { items.last().map(|d| d.base.id.to_string()) } else { None };
        Ok(Page { items, next_cursor })
    }

    async fn get_route(&self, tenant: Uuid, id: Uuid) -> Result<Option<Route>, StoreError> {
        let row = sqlx::query("SELECT data FROM routes WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }
    async fn list_routes(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Route>, StoreError> {
        let items = self.list_entities::<Route>("routes", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit { items.last().map(|r| r.base.id.to_string()) } else { None };
        Ok(Page { items, next_cursor })
    }

    async fn delete_extension(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM extensions WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .execute(&self.pool).await.map_err(be)?;
        Ok(res.rows_affected() > 0)
    }
    async fn delete_route(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM routes WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .execute(&self.pool).await.map_err(be)?;
        Ok(res.rows_affected() > 0)
    }

    async fn get_webhook(&self, tenant: Uuid, id: Uuid) -> Result<Option<Webhook>, StoreError> {
        let row = sqlx::query("SELECT data FROM webhooks WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }
    async fn list_webhooks(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Webhook>, StoreError> {
        let items = self.list_entities::<Webhook>("webhooks", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit { items.last().map(|w| w.base.id.to_string()) } else { None };
        Ok(Page { items, next_cursor })
    }
    async fn delete_webhook(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM webhooks WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .execute(&self.pool).await.map_err(be)?;
        Ok(res.rows_affected() > 0)
    }

    async fn get_object(&self, tenant: Uuid, id: Uuid) -> Result<Option<Object>, StoreError> {
        let row = sqlx::query("SELECT data FROM objects WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }
    async fn list_objects(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Object>, StoreError> {
        let items = self.list_entities::<Object>("objects", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit { items.last().map(|o| o.base.id.to_string()) } else { None };
        Ok(Page { items, next_cursor })
    }
    async fn delete_object(&self, tenant: Uuid, id: Uuid) -> Result<bool, StoreError> {
        let res = sqlx::query("DELETE FROM objects WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .execute(&self.pool).await.map_err(be)?;
        Ok(res.rows_affected() > 0)
    }

    async fn get_recording(&self, tenant: Uuid, id: Uuid) -> Result<Option<Recording>, StoreError> {
        let row = sqlx::query("SELECT data FROM recordings WHERE tenant_id = $1 AND id = $2")
            .bind(tenant.as_uuid()).bind(id.as_uuid())
            .fetch_optional(&self.pool).await.map_err(be)?;
        row.as_ref().map(entity_from_row).transpose()
    }
    async fn list_recordings(&self, tenant: Uuid, limit: usize, cursor: Option<String>) -> Result<Page<Recording>, StoreError> {
        let items = self.list_entities::<Recording>("recordings", tenant, limit, cursor).await?;
        let next_cursor = if items.len() == limit { items.last().map(|r| r.base.id.to_string()) } else { None };
        Ok(Page { items, next_cursor })
    }

    async fn put_sip_credential(&self, tenant: Uuid, username: &str, secret: &str) -> Result<(), StoreError> {
        sqlx::query(
            "INSERT INTO sip_credentials (tenant_id, username, secret) VALUES ($1, $2, $3) \
             ON CONFLICT (tenant_id, username) DO UPDATE SET secret = EXCLUDED.secret",
        )
        .bind(tenant.as_uuid()).bind(username).bind(secret)
        .execute(&self.pool).await.map_err(be)?;
        Ok(())
    }
    async fn get_sip_credential(&self, tenant: Uuid, username: &str) -> Result<Option<String>, StoreError> {
        let row = sqlx::query("SELECT secret FROM sip_credentials WHERE tenant_id = $1 AND username = $2")
            .bind(tenant.as_uuid()).bind(username)
            .fetch_optional(&self.pool).await.map_err(be)?;
        match row {
            Some(r) => Ok(Some(r.try_get("secret").map_err(be)?)),
            None => Ok(None),
        }
    }

    async fn call_for_idempotency_key(
        &self,
        tenant: Uuid,
        key: &str,
    ) -> Result<Option<Uuid>, StoreError> {
        let row = sqlx::query("SELECT call_id FROM idempotency_keys WHERE tenant_id = $1 AND key = $2")
            .bind(tenant.as_uuid())
            .bind(key)
            .fetch_optional(&self.pool)
            .await
            .map_err(be)?;
        match row {
            Some(r) => {
                let id: uuid::Uuid = r.try_get("call_id").map_err(be)?;
                Ok(Some(Uuid::from_uuid(id)))
            }
            None => Ok(None),
        }
    }

    async fn peek_outbox(&self, max: usize) -> Result<Vec<OutboxRecord>, StoreError> {
        // Bound the fetch so a final-drain `usize::MAX` doesn't overflow the bind.
        let lim = max.min(10_000) as i64;
        let rows = sqlx::query("SELECT seq, event FROM outbox ORDER BY seq ASC LIMIT $1")
            .bind(lim)
            .fetch_all(&self.pool)
            .await
            .map_err(be)?;
        rows.iter()
            .map(|r| {
                let seq: i64 = r.try_get("seq").map_err(be)?;
                let event: serde_json::Value = r.try_get("event").map_err(be)?;
                Ok(OutboxRecord {
                    seq: seq as u64,
                    event,
                })
            })
            .collect()
    }

    async fn ack_outbox(&self, up_to_seq: u64) -> Result<(), StoreError> {
        // At-least-once: the relay publishes before it acks, so a crash between the two
        // re-delivers rather than dropping. Deleting relayed rows keeps the outbox bounded.
        sqlx::query("DELETE FROM outbox WHERE seq <= $1")
            .bind(up_to_seq as i64)
            .execute(&self.pool)
            .await
            .map_err(be)?;
        Ok(())
    }
}
