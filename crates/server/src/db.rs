//! SQLite storage: peers, API tokens, jobs and statistics.
//!
//! Statements are tiny and SQLite runs in WAL mode, so the connection is shared
//! behind a mutex and used directly from async code.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use uuid::Uuid;

/// Current UNIX time in seconds.
pub fn now() -> i64 {
    chrono::Utc::now().timestamp()
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS peers (
    id             TEXT PRIMARY KEY,
    name           TEXT NOT NULL,
    public_key     TEXT NOT NULL,
    token_hash     TEXT NOT NULL UNIQUE,
    created_at     INTEGER NOT NULL,
    last_seen      INTEGER NOT NULL,
    client_version TEXT NOT NULL DEFAULT '',
    device_json    TEXT,
    backend        TEXT,
    score          REAL,
    score_model    TEXT
);

CREATE TABLE IF NOT EXISTS api_tokens (
    id         TEXT PRIMARY KEY,
    name       TEXT NOT NULL,
    token_hash TEXT NOT NULL UNIQUE,
    created_at INTEGER NOT NULL,
    last_used  INTEGER,
    expires_at INTEGER,
    revoked    INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS jobs (
    id                TEXT PRIMARY KEY,
    api_token_id      TEXT,
    created_at        INTEGER NOT NULL,
    finished_at       INTEGER,
    status            TEXT NOT NULL,
    model             TEXT,
    language          TEXT,
    audio_ms          INTEGER NOT NULL,
    words             INTEGER,
    parts             INTEGER NOT NULL,
    queue_ms          INTEGER,
    total_ms          INTEGER,
    override_rejected INTEGER NOT NULL DEFAULT 0,
    error             TEXT
);
CREATE INDEX IF NOT EXISTS jobs_created ON jobs(created_at);

CREATE TABLE IF NOT EXISTS job_parts (
    id               TEXT PRIMARY KEY,
    job_id           TEXT NOT NULL,
    peer_id          TEXT,
    created_at       INTEGER NOT NULL,
    status           TEXT NOT NULL,
    audio_ms         INTEGER NOT NULL,
    offer_ms         INTEGER,
    transfer_ms      INTEGER,
    processing_ms    INTEGER,
    first_segment_ms INTEGER,
    error            TEXT
);
CREATE INDEX IF NOT EXISTS job_parts_created ON job_parts(created_at);
CREATE INDEX IF NOT EXISTS job_parts_peer ON job_parts(peer_id);

CREATE TABLE IF NOT EXISTS peer_sessions (
    id              INTEGER PRIMARY KEY AUTOINCREMENT,
    peer_id         TEXT NOT NULL,
    connected_at    INTEGER NOT NULL,
    disconnected_at INTEGER
);
CREATE INDEX IF NOT EXISTS peer_sessions_peer ON peer_sessions(peer_id);
"#;

/// A stored peer.
#[derive(Debug, Clone)]
pub struct PeerRow {
    pub id: Uuid,
    pub name: String,
    pub public_key: String,
    pub created_at: i64,
    pub last_seen: i64,
    pub client_version: String,
    pub device_json: Option<String>,
    pub backend: Option<String>,
    pub score: Option<f64>,
    pub score_model: Option<String>,
}

/// A stored API token (without the secret).
#[derive(Debug, Clone)]
pub struct ApiTokenRow {
    pub id: Uuid,
    pub name: String,
    pub created_at: i64,
    pub last_used: Option<i64>,
    pub expires_at: Option<i64>,
    pub revoked: bool,
}

/// Values recorded for a finished job.
#[derive(Debug, Clone)]
pub struct JobRecord {
    pub id: Uuid,
    pub api_token_id: Uuid,
    pub created_at: i64,
    pub status: &'static str,
    pub model: Option<String>,
    pub language: Option<String>,
    pub audio_ms: u64,
    pub words: Option<u64>,
    pub parts: usize,
    pub queue_ms: Option<u64>,
    pub total_ms: u64,
    pub override_rejected: bool,
    pub error: Option<String>,
}

/// Values recorded for one attempt of a job part on a peer.
#[derive(Debug, Clone)]
pub struct PartRecord {
    pub id: Uuid,
    pub job_id: Uuid,
    pub peer_id: Option<Uuid>,
    pub status: &'static str,
    pub audio_ms: u64,
    pub offer_ms: Option<u64>,
    pub transfer_ms: Option<u64>,
    pub processing_ms: Option<u64>,
    pub first_segment_ms: Option<u64>,
    pub error: Option<String>,
}

/// The database handle.
pub struct Db {
    conn: Mutex<Connection>,
}

fn parse_uuid(text: String) -> rusqlite::Result<Uuid> {
    Uuid::parse_str(&text)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(0, rusqlite::types::Type::Text, Box::new(e)))
}

fn to_i64(v: u64) -> i64 {
    i64::try_from(v).unwrap_or(i64::MAX)
}

impl Db {
    /// Opens (and creates) the database file.
    pub fn open(path: &Path) -> anyhow::Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening {}", path.display()))?;
        Self::init(conn)
    }

    /// An in-memory database for tests.
    #[cfg(test)]
    pub fn open_in_memory() -> anyhow::Result<Self> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> anyhow::Result<Self> {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        conn.execute_batch(SCHEMA)?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().expect("database lock poisoned")
    }

    // ----- peers -----

    pub fn insert_peer(
        &self,
        id: Uuid,
        name: &str,
        public_key: &str,
        token_hash: &str,
        client_version: &str,
    ) -> anyhow::Result<()> {
        let t = now();
        self.conn().execute(
            "INSERT INTO peers (id, name, public_key, token_hash, created_at, last_seen, client_version) VALUES (?1, ?2, ?3, ?4, ?5, ?5, ?6)",
            params![id.to_string(), name, public_key, token_hash, t, client_version],
        )?;
        Ok(())
    }

    const PEER_COLUMNS: &'static str =
        "id, name, public_key, created_at, last_seen, client_version, device_json, backend, score, score_model";

    fn peer_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PeerRow> {
        Ok(PeerRow {
            id: parse_uuid(row.get(0)?)?,
            name: row.get(1)?,
            public_key: row.get(2)?,
            created_at: row.get(3)?,
            last_seen: row.get(4)?,
            client_version: row.get(5)?,
            device_json: row.get(6)?,
            backend: row.get(7)?,
            score: row.get(8)?,
            score_model: row.get(9)?,
        })
    }

    pub fn peer_by_token_hash(&self, token_hash: &str) -> anyhow::Result<Option<PeerRow>> {
        let sql = format!("SELECT {} FROM peers WHERE token_hash = ?1", Self::PEER_COLUMNS);
        Ok(self
            .conn()
            .query_row(&sql, params![token_hash], Self::peer_from_row)
            .optional()?)
    }

    pub fn list_peers(&self) -> anyhow::Result<Vec<PeerRow>> {
        let sql = format!("SELECT {} FROM peers ORDER BY name", Self::PEER_COLUMNS);
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt
            .query_map([], Self::peer_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Updates `last_seen`. Returns false when the peer no longer exists (revoked or expired).
    pub fn touch_peer(&self, id: Uuid) -> anyhow::Result<bool> {
        Ok(self.conn().execute(
            "UPDATE peers SET last_seen = ?2 WHERE id = ?1",
            params![id.to_string(), now()],
        )? > 0)
    }

    pub fn update_peer_hello(
        &self,
        id: Uuid,
        client_version: &str,
        device_json: &str,
        backend: &str,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE peers SET client_version = ?2, device_json = ?3, last_seen = ?4,
                 score = CASE WHEN backend IS ?5 THEN score ELSE NULL END,
                 score_model = CASE WHEN backend IS ?5 THEN score_model ELSE NULL END,
                 backend = ?5
             WHERE id = ?1",
            params![id.to_string(), client_version, device_json, now(), backend],
        )?;
        Ok(())
    }

    pub fn update_peer_score(&self, id: Uuid, score: f64, model: &str) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE peers SET score = ?2, score_model = ?3 WHERE id = ?1",
            params![id.to_string(), score, model],
        )?;
        Ok(())
    }

    /// Removes a peer (revocation). Returns true if it existed.
    pub fn delete_peer(&self, id: Uuid) -> anyhow::Result<bool> {
        Ok(self
            .conn()
            .execute("DELETE FROM peers WHERE id = ?1", params![id.to_string()])?
            > 0)
    }

    /// Removes peers that have not been seen since `cutoff`. Returns their ids.
    pub fn expire_peers(&self, cutoff: i64) -> anyhow::Result<Vec<Uuid>> {
        let conn = self.conn();
        let mut stmt = conn.prepare("DELETE FROM peers WHERE last_seen < ?1 RETURNING id")?;
        let ids = stmt
            .query_map(params![cutoff], |r| parse_uuid(r.get(0)?))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(ids)
    }

    pub fn open_session(&self, peer_id: Uuid) -> anyhow::Result<i64> {
        let conn = self.conn();
        conn.execute(
            "INSERT INTO peer_sessions (peer_id, connected_at) VALUES (?1, ?2)",
            params![peer_id.to_string(), now()],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub fn close_session(&self, session_id: i64) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE peer_sessions SET disconnected_at = ?2 WHERE id = ?1",
            params![session_id, now()],
        )?;
        Ok(())
    }

    /// Closes sessions left open by a crash.
    pub fn close_dangling_sessions(&self) -> anyhow::Result<()> {
        self.conn().execute(
            "UPDATE peer_sessions SET disconnected_at = (SELECT MAX(last_seen) FROM peers WHERE peers.id = peer_sessions.peer_id)
             WHERE disconnected_at IS NULL",
            [],
        )?;
        self.conn().execute(
            "UPDATE peer_sessions SET disconnected_at = connected_at WHERE disconnected_at IS NULL OR disconnected_at < connected_at",
            [],
        )?;
        Ok(())
    }

    // ----- API tokens -----

    pub fn insert_api_token(
        &self,
        id: Uuid,
        name: &str,
        token_hash: &str,
        expires_at: Option<i64>,
    ) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO api_tokens (id, name, token_hash, created_at, expires_at) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id.to_string(), name, token_hash, now(), expires_at],
        )?;
        Ok(())
    }

    /// Returns the id of a valid (not revoked, not expired) API token and marks it as used.
    pub fn use_api_token(&self, token_hash: &str) -> anyhow::Result<Option<Uuid>> {
        let t = now();
        let conn = self.conn();
        let id: Option<String> = conn
            .query_row(
                "SELECT id FROM api_tokens WHERE token_hash = ?1 AND revoked = 0 AND (expires_at IS NULL OR expires_at > ?2)",
                params![token_hash, t],
                |r| r.get(0),
            )
            .optional()?;
        let Some(id) = id else { return Ok(None) };
        conn.execute("UPDATE api_tokens SET last_used = ?2 WHERE id = ?1", params![id, t])?;
        Ok(Some(parse_uuid(id)?))
    }

    pub fn list_api_tokens(&self) -> anyhow::Result<Vec<ApiTokenRow>> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT id, name, created_at, last_used, expires_at, revoked FROM api_tokens ORDER BY created_at",
        )?;
        let rows = stmt
            .query_map([], |r| {
                Ok(ApiTokenRow {
                    id: parse_uuid(r.get(0)?)?,
                    name: r.get(1)?,
                    created_at: r.get(2)?,
                    last_used: r.get(3)?,
                    expires_at: r.get(4)?,
                    revoked: r.get::<_, i64>(5)? != 0,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn revoke_api_token(&self, id: Uuid) -> anyhow::Result<bool> {
        Ok(self.conn().execute(
            "UPDATE api_tokens SET revoked = 1 WHERE id = ?1",
            params![id.to_string()],
        )? > 0)
    }

    // ----- jobs -----

    pub fn record_job(&self, job: &JobRecord) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT OR REPLACE INTO jobs (id, api_token_id, created_at, finished_at, status, model, language, audio_ms, words, parts,
                                          queue_ms, total_ms, override_rejected, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
            params![
                job.id.to_string(),
                job.api_token_id.to_string(),
                job.created_at,
                now(),
                job.status,
                job.model,
                job.language,
                to_i64(job.audio_ms),
                job.words.map(to_i64),
                job.parts as i64,
                job.queue_ms.map(to_i64),
                to_i64(job.total_ms),
                job.override_rejected,
                job.error,
            ],
        )?;
        Ok(())
    }

    pub fn record_part(&self, part: &PartRecord) -> anyhow::Result<()> {
        self.conn().execute(
            "INSERT INTO job_parts (id, job_id, peer_id, created_at, status, audio_ms, offer_ms, transfer_ms, processing_ms, first_segment_ms, error)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                part.id.to_string(),
                part.job_id.to_string(),
                part.peer_id.map(|p| p.to_string()),
                now(),
                part.status,
                to_i64(part.audio_ms),
                part.offer_ms.map(to_i64),
                part.transfer_ms.map(to_i64),
                part.processing_ms.map(to_i64),
                part.first_segment_ms.map(to_i64),
                part.error,
            ],
        )?;
        Ok(())
    }

    /// Runs a closure with the raw connection (used by the statistics queries).
    pub fn with_conn<T>(&self, f: impl FnOnce(&Connection) -> rusqlite::Result<T>) -> anyhow::Result<T> {
        Ok(f(&self.conn())?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peer_lifecycle() {
        let db = Db::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        db.insert_peer(id, "desk", "pk", "hash", "0.1.0").unwrap();
        assert_eq!(db.peer_by_token_hash("hash").unwrap().unwrap().id, id);
        assert!(db.peer_by_token_hash("other").unwrap().is_none());

        db.update_peer_hello(id, "0.1.1", "{}", "cuda").unwrap();
        db.update_peer_score(id, 25.0, "large-v3-turbo").unwrap();
        let row = db.peer_by_token_hash("hash").unwrap().unwrap();
        assert_eq!(row.score, Some(25.0));

        // Changing the backend invalidates the score.
        db.update_peer_hello(id, "0.1.1", "{}", "cpu").unwrap();
        assert_eq!(db.peer_by_token_hash("hash").unwrap().unwrap().score, None);

        assert!(db.expire_peers(now() - 10).unwrap().is_empty());
        assert_eq!(db.expire_peers(now() + 10).unwrap(), vec![id]);
        assert!(db.list_peers().unwrap().is_empty());
    }

    #[test]
    fn api_tokens() {
        let db = Db::open_in_memory().unwrap();
        let id = Uuid::new_v4();
        db.insert_api_token(id, "ci", "h1", None).unwrap();
        db.insert_api_token(Uuid::new_v4(), "old", "h2", Some(now() - 1))
            .unwrap();
        assert_eq!(db.use_api_token("h1").unwrap(), Some(id));
        assert_eq!(db.use_api_token("h2").unwrap(), None);
        assert!(db.revoke_api_token(id).unwrap());
        assert_eq!(db.use_api_token("h1").unwrap(), None);
    }
}
