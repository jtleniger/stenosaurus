//! Binary entry point: CLI dispatch and the HTTP server loop.

use std::sync::Arc;

use anyhow::{Context, Result};
use clap::Parser;
use tracing_subscriber::EnvFilter;

use stenosaurus::auth::AuthConfig;
use stenosaurus::cli::{Cli, Command, ServeArgs, UserCommand};
use stenosaurus::db::Database;
use stenosaurus::routes::app;
use stenosaurus::state::AppState;
use stenosaurus::storage::DictationStore;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("stenosaurus=info,tower_http=warn")),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::User {
            command: UserCommand::Set { username },
        } => {
            let db = Database::open(&cli.data_dir)
                .await
                .with_context(|| format!("could not open {}", cli.data_dir.display()))?;
            stenosaurus::auth::set_user(&db, &username).await?;
            println!("Wrote {}", db.path().display());
            println!("Restart the server to apply changes.");
        }
        Command::Serve(args) => serve(cli.data_dir, args).await?,
    }
    Ok(())
}

async fn serve(data_dir: std::path::PathBuf, args: ServeArgs) -> Result<()> {
    if !args.model.is_file() {
        anyhow::bail!(
            "model file {} was not found; run ./scripts/setup-whistle.sh first",
            args.model.display()
        );
    }
    if !args.bind.ip().is_loopback() && !args.secure_cookies {
        tracing::warn!(
            "listening on a non-loopback address without --secure-cookies: microphone capture and \
             WASM integrity require HTTPS; terminate TLS in a reverse proxy and pass \
             --secure-cookies"
        );
    }

    let db = Database::open(&data_dir)
        .await
        .with_context(|| format!("could not open {}", data_dir.display()))?;
    let auth = AuthConfig::load(&db).await?;
    let store = DictationStore::new(db);
    let state = AppState {
        store: Arc::new(store),
        auth: Arc::new(auth),
        static_dir: args.static_dir,
        model_path: args.model,
        secure_cookies: args.secure_cookies,
    };

    let listener = tokio::net::TcpListener::bind(args.bind)
        .await
        .with_context(|| format!("could not bind {}", args.bind))?;
    let local = listener.local_addr()?;
    tracing::info!("listening on http://{local}");

    axum::serve(listener, app(state))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
            tracing::info!("shutting down");
        })
        .await
        .context("server failed")
}
