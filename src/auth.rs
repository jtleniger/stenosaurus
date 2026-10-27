//! Single-user authentication: one username and an Argon2id password hash
//! stored in SQLite.

use std::io;

use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::Argon2;
use rusqlite::{params, OptionalExtension};

use crate::db::Database;

pub const SESSION_KEY: &str = "username";

#[derive(Debug, Clone)]
pub struct AuthConfig {
    pub username: String,
    pub password_hash: String,
}

#[derive(Debug, thiserror::Error)]
pub enum AuthError {
    #[error(
        "Authentication is not configured yet. Run `stenosaurus user set --username <name>` to create it."
    )]
    NotConfigured,
    #[error("Username must be 1-64 characters using letters, digits, '-' or '_'.")]
    InvalidUsername,
    #[error("Password must be at least 8 characters.")]
    InvalidPassword,
    #[error("Passwords do not match.")]
    PasswordMismatch,
    #[error("could not read password: {0}")]
    Io(#[from] io::Error),
    #[error("authentication database error: {0}")]
    Db(#[from] tokio_rusqlite::Error),
    #[error("could not hash password: {0}")]
    Hash(String),
}

/// Hash a password with Argon2id defaults and return a PHC string.
pub fn hash_password(password: &str) -> Result<String, AuthError> {
    let hash = Argon2::default()
        .hash_password(password.as_bytes())
        .map_err(|err| AuthError::Hash(err.to_string()))?;
    Ok(hash.to_string())
}

pub fn validate_username(username: &str) -> Result<(), AuthError> {
    let valid = !username.is_empty()
        && username.len() <= 64
        && username
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    if valid {
        Ok(())
    } else {
        Err(AuthError::InvalidUsername)
    }
}

pub fn validate_password(password: &str) -> Result<(), AuthError> {
    if password.len() >= 8 {
        Ok(())
    } else {
        Err(AuthError::InvalidPassword)
    }
}

/// Interactively (or from piped stdin) set the one user account.
pub async fn set_user(db: &Database, username: &str) -> Result<(), AuthError> {
    use std::io::{BufRead, IsTerminal};

    validate_username(username)?;
    let password = if io::stdin().is_terminal() {
        let first = rpassword::prompt_password("Password: ")?;
        let second = rpassword::prompt_password("Confirm password: ")?;
        if first != second {
            return Err(AuthError::PasswordMismatch);
        }
        first
    } else {
        let mut line = String::new();
        io::stdin().lock().read_line(&mut line)?;
        line.trim_end_matches(['\n', '\r']).to_string()
    };
    validate_password(&password)?;
    AuthConfig::from_password(username, &password)?
        .save(db)
        .await
}

impl AuthConfig {
    pub fn from_password(username: &str, password: &str) -> Result<Self, AuthError> {
        Ok(Self {
            username: username.to_string(),
            password_hash: hash_password(password)?,
        })
    }

    /// Store (or replace) the single account.
    pub async fn save(&self, db: &Database) -> Result<(), AuthError> {
        let username = self.username.clone();
        let password_hash = self.password_hash.clone();
        db.call(move |conn| {
            conn.execute(
                "INSERT INTO users (id, username, password_hash)
                 VALUES (1, ?1, ?2)
                 ON CONFLICT (id) DO UPDATE SET
                     username = excluded.username,
                     password_hash = excluded.password_hash",
                params![username, password_hash],
            )?;
            Ok(())
        })
        .await?;
        Ok(())
    }

    /// Load the single account from the database.
    pub async fn load(db: &Database) -> Result<Self, AuthError> {
        let account = db
            .call(|conn| {
                let row = conn
                    .query_row(
                        "SELECT username, password_hash FROM users WHERE id = 1",
                        [],
                        |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
                    )
                    .optional()?;
                Ok(row.map(|(username, password_hash)| Self {
                    username,
                    password_hash,
                }))
            })
            .await?;
        account.ok_or(AuthError::NotConfigured)
    }

    pub fn verify(&self, username: &str, password: &str) -> bool {
        if username != self.username {
            return false;
        }
        let Ok(parsed) = PasswordHash::new(&self.password_hash) else {
            return false;
        };
        Argon2::default()
            .verify_password(password.as_bytes(), &parsed)
            .is_ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Database;

    const PASSWORD: &str = "correcthorse";

    async fn database() -> (tempfile::TempDir, Database) {
        let dir = tempfile::tempdir().unwrap();
        let db = Database::open(dir.path()).await.unwrap();
        (dir, db)
    }

    #[test]
    fn hash_and_verify_round_trip() {
        let config = AuthConfig::from_password("alice", PASSWORD).unwrap();
        assert!(config.password_hash.starts_with("$argon2id$"));
        assert!(config.verify("alice", PASSWORD));
        assert!(!config.verify("alice", "wrongpassword"));
        assert!(!config.verify("bob", PASSWORD));
        assert!(!config.verify("Alice", PASSWORD));
    }

    #[tokio::test]
    async fn save_and_load_round_trip() {
        let (_guard, db) = database().await;
        let config = AuthConfig::from_password("alice", PASSWORD).unwrap();
        config.save(&db).await.unwrap();
        let loaded = AuthConfig::load(&db).await.unwrap();
        assert_eq!(loaded.username, "alice");
        assert_eq!(loaded.password_hash, config.password_hash);
        assert!(loaded.verify("alice", PASSWORD));
    }

    #[tokio::test]
    async fn save_replaces_the_single_account() {
        let (_guard, db) = database().await;
        AuthConfig::from_password("alice", PASSWORD)
            .unwrap()
            .save(&db)
            .await
            .unwrap();
        AuthConfig::from_password("bob", "different-password")
            .unwrap()
            .save(&db)
            .await
            .unwrap();
        let loaded = AuthConfig::load(&db).await.unwrap();
        assert_eq!(loaded.username, "bob");
        assert!(loaded.verify("bob", "different-password"));
        let rows: i64 = db
            .call(|conn| conn.query_row("SELECT COUNT(*) FROM users", [], |row| row.get(0)))
            .await
            .unwrap();
        assert_eq!(rows, 1);
    }

    #[tokio::test]
    async fn missing_configuration_is_reported() {
        let (_guard, db) = database().await;
        let missing = AuthConfig::load(&db).await;
        assert!(matches!(missing, Err(AuthError::NotConfigured)));
    }

    #[test]
    fn rejects_invalid_usernames() {
        for username in ["", " ", "a b", "a/b", "../etc", &"a".repeat(65)] {
            assert!(
                matches!(validate_username(username), Err(AuthError::InvalidUsername)),
                "{username:?} should be rejected"
            );
        }
        for username in ["alice", "a", "Alice-1", "a_b", &"a".repeat(64)] {
            assert!(
                validate_username(username).is_ok(),
                "{username:?} should be accepted"
            );
        }
    }

    #[test]
    fn rejects_short_passwords() {
        assert!(matches!(
            validate_password("1234567"),
            Err(AuthError::InvalidPassword)
        ));
        assert!(validate_password("12345678").is_ok());
    }
}
