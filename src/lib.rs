//! Stenosaurus — a self-hosted, single-user dictation site.
//!
//! Speech recognition runs entirely in the browser; the server stores text.

pub mod auth;
pub mod cli;
pub mod db;
pub mod error;
pub mod routes;
pub mod state;
pub mod storage;
