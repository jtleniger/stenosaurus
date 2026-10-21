//! Single-user authentication: one username and an Argon2id password hash.

use std::io;
use std::path::Path;

use argon2::password_hash::{phc::PasswordHash, PasswordHasher, PasswordVerifier};
use argon2::Argon2;

pub const SESSION_KEY: &str = "username";
const AUTH_FILE: &str = "auth.toml";

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    #[error("could not read {}: {message}", AUTH_FILE)]
    Read { message: String },
    #[error("could not write {}: {message}", AUTH_FILE)]
    Write { message: String },
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
pub fn set_user(data_dir: &Path, username: &str) -> Result<(), AuthError> {
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
    AuthConfig::from_password(username, &password)?.save(data_dir)
}

impl AuthConfig {
    pub fn from_password(username: &str, password: &str) -> Result<Self, AuthError> {
        Ok(Self {
            username: username.to_string(),
            password_hash: hash_password(password)?,
        })
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), AuthError> {
        std::fs::create_dir_all(data_dir).map_err(|err| AuthError::Write {
            message: err.to_string(),
        })?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(data_dir, std::fs::Permissions::from_mode(0o700)).map_err(
                |err| AuthError::Write {
                    message: err.to_string(),
                },
            )?;
        }
        let serialized = toml::to_string_pretty(self)
            .map_err(|err| AuthError::Write { message: err.to_string() })?;
        let target = data_dir.join(AUTH_FILE);
        let temp = data_dir.join("auth.toml.tmp");

        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let write = || -> io::Result<()> {
            use std::io::Write;
            let mut file = options.open(&temp)?;
            file.write_all(serialized.as_bytes())?;
            file.sync_all()?;
            std::fs::rename(&temp, &target)
        };
        write().map_err(|err| AuthError::Write { message: err.to_string() })
    }

    pub fn load(data_dir: &Path) -> Result<Self, AuthError> {
        let path = data_dir.join(AUTH_FILE);
        let contents = match std::fs::read_to_string(&path) {
            Ok(contents) => contents,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Err(AuthError::NotConfigured),
            Err(err) => {
                return Err(AuthError::Read {
                    message: err.to_string(),
                })
            }
        };
        toml::from_str(&contents).map_err(|err| AuthError::Read {
            message: err.to_string(),
        })
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

    const PASSWORD: &str = "correcthorse";

    #[test]
    fn hash_and_verify_round_trip() {
        let config = AuthConfig::from_password("alice", PASSWORD).unwrap();
        assert!(config.password_hash.starts_with("$argon2id$"));
        assert!(config.verify("alice", PASSWORD));
        assert!(!config.verify("alice", "wrongpassword"));
        assert!(!config.verify("bob", PASSWORD));
        assert!(!config.verify("Alice", PASSWORD));
    }

    #[test]
    fn save_and_load_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let config = AuthConfig::from_password("alice", PASSWORD).unwrap();
        config.save(dir.path()).unwrap();
        let loaded = AuthConfig::load(dir.path()).unwrap();
        assert_eq!(loaded.username, "alice");
        assert_eq!(loaded.password_hash, config.password_hash);
        assert!(loaded.verify("alice", PASSWORD));
    }

    #[cfg(unix)]
    #[test]
    fn auth_file_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        AuthConfig::from_password("alice", PASSWORD)
            .unwrap()
            .save(dir.path())
            .unwrap();
        let meta = std::fs::metadata(dir.path().join(AUTH_FILE)).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o600);
        let dir_meta = std::fs::metadata(dir.path()).unwrap();
        assert_eq!(dir_meta.permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn missing_configuration_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let missing = AuthConfig::load(&dir.path().join("nothing-here"));
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
            assert!(validate_username(username).is_ok(), "{username:?} should be accepted");
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
