//! Core of Lazurio Agent Desktop.
//!
//! The computer opens the only connection: it pairs with an Environment through
//! the Environment's Launchpad (contract v0, section 1) and then runs the
//! Agent Desktop MCP server over the stdio of an SSH session it starts itself
//! (section 2). The owner's switch decides whether that session exists at all.
//!
//! Nothing here depends on a user interface, so the same code can later run as a
//! helper process of Lazurio App.

pub mod app_tools;
pub mod channel;
pub mod error;
pub mod history;
pub mod keys;
pub mod keystore;
pub mod launchpad;
pub mod manager;
pub mod mcp_tap;
pub mod pairing;
pub mod platform;
pub mod store;
mod time;

pub use error::{Error, Result};
pub use manager::{EnvironmentView, Manager, ManagerConfig};

/// Bundle identifier shared by the app, the key store entries and the data directory.
pub const APP_ID: &str = "io.lazurio.agent-desktop";
