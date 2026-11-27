//! Harness agent hub (spec `docs/specs/harness/phase-8-core.md`).
//!
//! Implements [`harness_core_api::HarnessCore`]: one actor per session owns its transcript, queue and sticky
//! agent; [`pool::AgentPool`] owns agent CLI processes (spawned on demand, stopped when idle);
//! [`quota::QuotaRegistry`] owns exhaustion state; [`store::Store`] is the single SQLite writer.

pub mod about;
mod accounts;
pub mod acp;
mod adapter_update;
mod admin;
pub mod agents;
mod agy_accounts;
mod agy_agents;
pub mod agy_cred;
mod agy_delegate;
mod agy_ops;
mod agy_tasks;
mod board;
mod board_sync;
mod bus;
pub mod catchup;
mod codex_delegate;
pub mod config;
mod context;
pub mod decision;
mod delegate;
mod delegate_compact;
mod delegate_coord;
pub mod delegate_mcp;
mod delegate_server;
mod error;
mod factcheck;
mod failure;
mod files;
mod gh;
pub mod graphify;
pub mod helpers;
mod hub;
mod hub_chat;
mod import;
mod inbox;
mod insights;
mod journal;
pub mod machine;
pub mod mcp;
pub mod memory;
mod options;
mod orchestra;
pub mod pool;
mod prd;
mod process;
mod project_map;
mod providers;
pub mod quota;
pub mod recall;
mod research;
mod research_figures;
mod resume;
mod schedule;
pub mod selector;
mod session;
pub(crate) mod session_notes;
pub mod setup;
mod snapshot;
pub mod store;
mod store_chat;
pub mod symbol_claims;
mod tool_gateway;
mod tools;
mod util;
pub mod work;
mod worktree;

pub use agy_cred::{AgyCredStore, CredAttribute, CredBlob, SharedAgyCredStore};
pub use config::{
    AutoUpdatePolicy, DelegationPolicy, DisabledAgents, FactCheckPolicy, HubConfig,
    SharedAuthProbe, SharedFileSearch,
};
pub use error::HubError;
pub use hub::Hub;
