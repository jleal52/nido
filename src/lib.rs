//! nido: gestor de sesiones de terminal (sobre todo Claude Code), por web y por terminal.

pub mod accounts;
pub mod claude_store;
#[cfg(unix)]
pub mod attach;
pub mod auth;
pub mod browse;
pub mod claude;
pub mod client;
pub mod e2e;
pub mod emu;
pub mod group;
pub mod history;
pub mod holder;
pub mod hub;
pub mod import;
pub mod oauth;
pub mod ipc;
pub mod launch;
pub mod prompt;
pub mod proto;
pub mod registry;
pub mod server;
pub mod sys;
pub mod transcript;
pub mod upload;
