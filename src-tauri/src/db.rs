use rusqlite::{params, Connection};
use std::path::Path;

pub struct SyncDatabase {
    conn: Connection,
}

impl SyncDatabase {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
        }
        let conn = Connection::open(path).map_err(|e| e.to_string())?;
        conn.execute_batch(
            r"
            CREATE TABLE IF NOT EXISTS synced_files (
                path TEXT PRIMARY KEY,
                sha1 TEXT NOT NULL,
                remote_id TEXT,
                is_duplicate INTEGER NOT NULL DEFAULT 0,
                updated_at TEXT NOT NULL
            );
            ",
        )
        .map_err(|e| e.to_string())?;
        Ok(Self { conn })
    }

    pub fn get_sha1(&self, path: &str) -> Result<Option<String>, String> {
        let mut stmt = self
            .conn
            .prepare("SELECT sha1 FROM synced_files WHERE path = ?1")
            .map_err(|e| e.to_string())?;
        let mut rows = stmt.query(params![path]).map_err(|e| e.to_string())?;
        if let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let sha1: String = row.get(0).map_err(|e| e.to_string())?;
            Ok(Some(sha1))
        } else {
            Ok(None)
        }
    }

    pub fn upsert(
        &self,
        path: &str,
        sha1: &str,
        remote_id: Option<&str>,
        is_duplicate: bool,
    ) -> Result<(), String> {
        let now = chrono::Utc::now().to_rfc3339();
        self.conn
            .execute(
                r"
                INSERT INTO synced_files (path, sha1, remote_id, is_duplicate, updated_at)
                VALUES (?1, ?2, ?3, ?4, ?5)
                ON CONFLICT(path) DO UPDATE SET
                    sha1 = excluded.sha1,
                    remote_id = excluded.remote_id,
                    is_duplicate = excluded.is_duplicate,
                    updated_at = excluded.updated_at
                ",
                params![
                    path,
                    sha1,
                    remote_id,
                    if is_duplicate { 1 } else { 0 },
                    now
                ],
            )
            .map_err(|e| e.to_string())?;
        Ok(())
    }
}
