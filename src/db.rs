//! Shared SQLite database: schema, pragmas and the async connection handle.
//!
//! One `stenosaurus.db` file holds both the user account and the notes. The
//! handle is cheap to clone; all calls run on a dedicated database thread.

use std::io;
use std::path::{Path, PathBuf};

use tokio_rusqlite::Connection;

/// Database filename inside the data directory.
pub const DB_FILE: &str = "stenosaurus.db";

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY CHECK (id = 1),
    username      TEXT NOT NULL,
    password_hash TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS notes (
    id          TEXT PRIMARY KEY,
    recorded_at INTEGER NOT NULL,
    sequence    INTEGER NOT NULL,
    text        TEXT NOT NULL,
    text_lower  TEXT NOT NULL
);

CREATE INDEX IF NOT EXISTS notes_newest
    ON notes (recorded_at DESC, sequence DESC);

CREATE TABLE IF NOT EXISTS drafts (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    name       TEXT NOT NULL DEFAULT '',
    created_at INTEGER NOT NULL,
    updated_at INTEGER NOT NULL,
    text       TEXT NOT NULL DEFAULT ''
);

CREATE INDEX IF NOT EXISTS drafts_recent
    ON drafts (updated_at DESC, id DESC);

PRAGMA user_version = 2;
"#;

#[derive(Debug, thiserror::Error)]
pub enum DbError {
    #[error("database error: {0}")]
    Sqlite(#[from] tokio_rusqlite::Error),
    #[error("could not prepare {}: {source}", DB_FILE)]
    Io {
        #[source]
        source: io::Error,
    },
    #[error("could not open {}: {source}", DB_FILE)]
    Open {
        #[source]
        source: rusqlite::Error,
    },
}

/// A cloneable handle to the application database.
#[derive(Clone)]
pub struct Database {
    conn: Connection,
    path: PathBuf,
}

impl Database {
    /// Open (or create) the database at `<data_dir>/stenosaurus.db`.
    pub async fn open(data_dir: &Path) -> Result<Self, DbError> {
        tokio::fs::create_dir_all(data_dir)
            .await
            .map_err(|source| DbError::Io { source })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700))
                .await
                .map_err(|source| DbError::Io { source })?;
        }

        let path = data_dir.join(DB_FILE);
        let conn = Connection::open(&path)
            .await
            .map_err(|source| DbError::Open { source })?;
        conn.call(|conn| {
            conn.busy_timeout(std::time::Duration::from_secs(5))?;
            conn.execute_batch(
                "PRAGMA journal_mode = WAL;\n\
                 PRAGMA synchronous = NORMAL;\n\
                 PRAGMA foreign_keys = ON;",
            )?;
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            tokio::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .await
                .map_err(|source| DbError::Io { source })?;
        }

        Ok(Self { conn, path })
    }

    /// Run a closure on the database thread.
    pub async fn call<F, R, E>(&self, function: F) -> Result<R, tokio_rusqlite::Error<E>>
    where
        F: FnOnce(&mut rusqlite::Connection) -> Result<R, E> + Send + 'static,
        R: Send + 'static,
        E: Send + 'static,
    {
        self.conn.call(function).await
    }

    /// Path of the database file.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn creates_schema_and_private_file() {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).await.unwrap();
        assert_eq!(db.path(), dir.path().join(DB_FILE));
        let version: i64 = db
            .call(|conn| conn.query_row("PRAGMA user_version", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(version, 2);
        let tables: Vec<String> = db
            .call(|conn| -> rusqlite::Result<Vec<String>> {
                let mut stmt = conn
                    .prepare("SELECT name FROM sqlite_master WHERE type = 'table' ORDER BY name")?;
                let rows = stmt
                    .query_map([], |row| row.get(0))?
                    .collect::<std::result::Result<Vec<String>, _>>()?;
                Ok(rows)
            })
            .await
            .unwrap();
        assert!(tables.contains(&"notes".to_string()));
        assert!(tables.contains(&"users".to_string()));
        assert!(tables.contains(&"drafts".to_string()));

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let meta = std::fs::metadata(db.path()).unwrap();
            assert_eq!(meta.permissions().mode() & 0o777, 0o600);
            let dir_meta = std::fs::metadata(dir.path()).unwrap();
            assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);
        }
    }

    #[tokio::test]
    async fn reopens_existing_database() {
        let dir = tempfile::tempdir().unwrap();
        {
            let db = Database::open(dir.path()).await.unwrap();
            db.call(|conn| -> rusqlite::Result<()> {
                conn.execute(
                    "INSERT INTO users (id, username, password_hash) VALUES (1, 'alice', 'hash')",
                    [],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        }
        let db = Database::open(dir.path()).await.unwrap();
        let username: String = db
            .call(|conn| {
                conn.query_row("SELECT username FROM users WHERE id = 1", [], |row| {
                    row.get(0)
                })
            })
            .await
            .unwrap();
        assert_eq!(username, "alice");
    }

    #[tokio::test]
    async fn migrates_a_version_one_database() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join(DB_FILE);
        {
            let conn = rusqlite::Connection::open(&path).unwrap();
            conn.execute_batch(
                "CREATE TABLE users (
                     id INTEGER PRIMARY KEY CHECK (id = 1),
                     username TEXT NOT NULL,
                     password_hash TEXT NOT NULL
                 );
                 CREATE TABLE notes (
                     id TEXT PRIMARY KEY,
                     recorded_at INTEGER NOT NULL,
                     sequence INTEGER NOT NULL,
                     text TEXT NOT NULL,
                     text_lower TEXT NOT NULL
                 );
                 INSERT INTO notes VALUES ('old', 1, 0, 'kept', 'kept');
                 PRAGMA user_version = 1;",
            )
            .unwrap();
        }

        let db = Database::open(dir.path()).await.unwrap();
        let (version, note, has_drafts): (i64, String, bool) = db
            .call(|conn| -> rusqlite::Result<(i64, String, bool)> {
                let version = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
                let note =
                    conn.query_row("SELECT text FROM notes WHERE id = 'old'", [], |row| {
                        row.get(0)
                    })?;
                let has_drafts = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master
                                   WHERE type = 'table' AND name = 'drafts')",
                    [],
                    |row| row.get(0),
                )?;
                Ok((version, note, has_drafts))
            })
            .await
            .unwrap();
        assert_eq!(version, 2);
        assert_eq!(note, "kept");
        assert!(has_drafts);
    }
}
