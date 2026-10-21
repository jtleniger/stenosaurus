//! Command-line interface.

use std::net::SocketAddr;
use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

#[derive(Debug, Parser)]
#[command(name = "stenosaurus", version, about = "Self-hosted browser dictation")]
pub struct Cli {
    /// Directory holding auth.toml and dictations/.
    #[arg(long, global = true, env = "STENO_DATA_DIR", default_value = "data")]
    pub data_dir: PathBuf,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Serve the dictation site.
    Serve(ServeArgs),
    /// Manage the single user account.
    User {
        #[command(subcommand)]
        command: UserCommand,
    },
}

#[derive(Debug, Args)]
pub struct ServeArgs {
    /// Address to listen on.
    #[arg(long, env = "STENO_BIND", default_value = "127.0.0.1:3000")]
    pub bind: SocketAddr,

    /// Path to the Whistle model served to browsers.
    #[arg(long, env = "STENO_MODEL", default_value = "vendor/whistle.cact")]
    pub model: PathBuf,

    /// Directory served at /static.
    #[arg(long, env = "STENO_STATIC_DIR", default_value = "static")]
    pub static_dir: PathBuf,

    /// Mark session cookies Secure; use when serving over HTTPS.
    #[arg(long, env = "STENO_SECURE_COOKIES", action = clap::ArgAction::SetTrue)]
    pub secure_cookies: bool,
}

#[derive(Debug, Subcommand)]
pub enum UserCommand {
    /// Set the username and password (prompts when stdin is a terminal).
    Set {
        #[arg(long, env = "STENO_USERNAME")]
        username: String,
    },
}
