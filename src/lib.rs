//! nido: gestor de sesiones de terminal (sobre todo Claude Code), por web y por terminal.

pub mod accounts;
#[cfg(unix)]
pub mod attach;
pub mod auth;
pub mod browse;
pub mod claude;
pub mod claude_store;
pub mod client;
pub mod e2e;
pub mod emu;
pub mod group;
pub mod history;
pub mod holder;
pub mod hub;
pub mod i18n;
pub mod import;
pub mod ipc;
pub mod launch;
pub mod oauth;
pub mod prompt;
pub mod proto;
pub mod registry;
pub mod server;
pub mod sys;
pub mod transcript;
pub mod upload;
