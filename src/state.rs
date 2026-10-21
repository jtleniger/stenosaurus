//! Shared application state.

use std::path::PathBuf;
use std::sync::Arc;

use crate::auth::AuthConfig;
use crate::storage::DictationStore;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<DictationStore>,
    pub auth: Arc<AuthConfig>,
    pub static_dir: PathBuf,
    pub model_path: PathBuf,
    pub secure_cookies: bool,
}
