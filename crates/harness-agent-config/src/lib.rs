//! Agent CLI configuration for the settings pages (spec `docs/specs/harness/phase-9-parity.md` §9.1.3–9.1.4).
//!
//! The CLIs own their configuration: Harness reads their files and writes them only after the user saw a diff
//! ([`WritePlan`]) and confirmed ([`AgentConfigService::apply`]). Writes are atomic, keep unknown keys (and TOML
//! comments), and are skipped when nothing changes. Credentials files are never opened ([`harness_settings::fsx`]);
//! login state is a presence check through an injected [`AuthProbe`].

#![allow(
    clippy::missing_errors_doc,
    clippy::many_single_char_names,
    clippy::match_same_arms,
    clippy::format_push_string
)]

pub mod auth;
pub mod clis;
mod error;
mod frontmatter;
mod hooks;
mod jsonfile;
pub mod launch;
mod mcp;
mod mcp_import;
mod md_store;
pub mod overlay;
mod paths;
pub mod plan;
pub mod plugin_read;
mod service;
pub mod skill_catalog;
pub mod skill_health;
mod types;

pub use auth::{AuthProbe, FileExistenceProbe, StaticProbe};
pub use error::{ConfigError, Result};
pub use mcp_import::McpImport;
pub use paths::ConfigPaths;
pub use plan::{apply, unified_diff, ApplyOutcome, FileChange, FileOp, WritePlan};
pub use service::AgentConfigService;
pub use types::{
    AgentKind, AuthState, Command, Concept, ConceptSupport, ConfigChanged, ConfigTarget, Entry,
    EntryBody, Hook, InstructionFile, McpServer, McpTransport, Scope, Skill, Subagent,
};
