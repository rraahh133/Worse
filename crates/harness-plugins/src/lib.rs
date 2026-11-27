//! Plugin store core (`docs/plans/plugins.md`).
//!
//! Plugins use the Claude Code plugin format unchanged: a folder with `.claude-plugin/plugin.json` and any
//! of `skills/`, `commands/`, `agents/`, `.mcp.json`, `hooks/`; a marketplace is a git repository or local
//! folder with `.claude-plugin/marketplace.json`.
//!
//! This crate never applies a write itself: [`install::plan_install`] and [`install::plan_uninstall`]
//! return a normal [`harness_agent_config::WritePlan`] the user confirms in the diff dialog, applied by the
//! settings worker. It downloads with the `git` CLI ([`fetch`]) and keeps its own state under
//! `<HARNESS_HOME>/plugins/` ([`record`], [`store`]). Plugin hooks are never installed.

#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used))]

mod error;
pub mod fetch;
pub mod install;
pub mod marketplace;
pub mod plugin;
pub mod record;
pub mod store;

pub use error::{PluginError, Result};
pub use marketplace::{Listing, Marketplace, Source};
pub use plugin::Plugin;
pub use record::{Installed, InstalledPlugin, Written};

#[cfg(test)]
mod tests;
