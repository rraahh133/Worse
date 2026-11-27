//! How Harness starts its own helper processes (phase-9-parity.md §9.9).
//!
//! Owner: [`HelperLaunch`] (held in `HubConfig.helpers`) is the only code that turns a helper into
//! `program + args`. The product ships one executable (`harness-slint`) whose helper modes are selected by a leading
//! flag; development builds and older gates use the standalone binaries `harness-memory-mcp` and `harness-api-agent`.
//! The helper bodies live here ([`run_memory_mcp`], [`run_delegate_mcp`]) and in `harness_api_agent::run`, so the
//! standalone binaries and the app modes share one implementation.

use std::io::{self, BufReader};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use crate::delegate_mcp::{self, DelegateEnv};
use crate::error::HubError;
use crate::memory::MemoryFiles;

/// Mode flag of the memory MCP server (`<exe> --memory-mcp [--project <dir>] [--session <id>]`).
pub const FLAG_MEMORY_MCP: &str = "--memory-mcp";
/// Mode flag of the `harness-delegate` MCP server (`<exe> --delegate`).
pub const FLAG_DELEGATE: &str = "--delegate";
/// Mode flag of the API provider ACP agent (`<exe> --api-agent`).
pub const FLAG_API_AGENT: &str = "--api-agent";
pub const FLAG_COMPUTER_MCP: &str = "--computer-mcp";
pub const FLAG_WORK_MCP: &str = "--work-mcp";
/// Mode flag of the `harness-tools` gateway in front of the overlay MCP servers (`<exe> --tool-gateway`).
pub const FLAG_TOOL_GATEWAY: &str = "--tool-gateway";
/// Mode flag of a connector bridge (`<exe> --connector-mcp <id>`, `docs/plans/connectors.md`).
pub const FLAG_CONNECTOR_MCP: &str = harness_connectors::FLAG_CONNECTOR_MCP;

/// A helper process Harness starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Helper {
    /// Memory MCP server (`memory_read` / `memory_write`).
    MemoryMcp,
    /// `harness-delegate` MCP server (§9.8).
    Delegate,
    /// ACP agent of one OpenAI-compatible provider (§9.7).
    ApiAgent,
    Computer,
    /// Work edition MCP server (`work_*` tools).
    Work,
    /// `harness-tools`: `find_tools` / `call_tool` over the overlay servers.
    ToolGateway,
    /// Bridge to one connected remote MCP server.
    Connector,
}

/// How helpers are launched.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HelperLaunch {
    /// One multi-mode executable: `program <mode flag> [args]` (the shipped app).
    SelfExe { program: PathBuf },
    /// Standalone binaries: `harness-memory-mcp [--delegate]` and `harness-api-agent` (development, gates).
    Separate {
        memory_mcp: PathBuf,
        api_agent: PathBuf,
    },
}

fn exe_name(stem: &str) -> String {
    if cfg!(windows) {
        format!("{stem}.exe")
    } else {
        stem.to_owned()
    }
}

fn env_path(key: &str) -> Option<PathBuf> {
    std::env::var_os(key)
        .filter(|v| !v.is_empty())
        .map(PathBuf::from)
}

impl HelperLaunch {
    /// Standalone binaries: `HARNESS_MEMORY_MCP` and `HARNESS_API_AGENT`, each else next to the current executable
    /// (the API agent: next to the memory server when only `HARNESS_MEMORY_MCP` is set).
    ///
    /// # Errors
    ///
    /// The current executable cannot be resolved and no override is set.
    pub fn from_env() -> Result<Self, HubError> {
        let memory_mcp = if let Some(path) = env_path("HARNESS_MEMORY_MCP") {
            path
        } else {
            let exe = std::env::current_exe()?;
            let dir = exe
                .parent()
                .ok_or_else(|| HubError::Config("current executable has no parent".into()))?;
            dir.join(exe_name("harness-memory-mcp"))
        };
        let api_agent = env_path("HARNESS_API_AGENT")
            .unwrap_or_else(|| memory_mcp.with_file_name(exe_name("harness-api-agent")));
        Ok(Self::Separate {
            memory_mcp,
            api_agent,
        })
    }

    /// The app's choice: its own executable `exe` with mode flags, unless `HARNESS_MEMORY_MCP` selects the
    /// standalone binaries (older UI gates).
    ///
    /// # Errors
    ///
    /// Only from [`HelperLaunch::from_env`] when the override is set.
    pub fn for_app(exe: PathBuf) -> Result<Self, HubError> {
        if env_path("HARNESS_MEMORY_MCP").is_some() {
            return Self::from_env();
        }
        Ok(Self::SelfExe { program: exe })
    }

    /// Program of `helper`.
    #[must_use]
    pub fn program(&self, helper: Helper) -> &Path {
        match (self, helper) {
            (Self::SelfExe { program }, _) => program,
            (
                Self::Separate { memory_mcp, .. },
                Helper::MemoryMcp
                | Helper::Delegate
                | Helper::Computer
                | Helper::Work
                | Helper::ToolGateway
                | Helper::Connector,
            ) => memory_mcp,
            (Self::Separate { api_agent, .. }, Helper::ApiAgent) => api_agent,
        }
    }

    /// Leading arguments of `helper` (callers append their own, e.g. `--project <dir>`).
    #[must_use]
    pub fn args(&self, helper: Helper) -> Vec<String> {
        let flag = match (self, helper) {
            (Self::SelfExe { .. }, Helper::MemoryMcp) => Some(FLAG_MEMORY_MCP),
            (_, Helper::Delegate) => Some(FLAG_DELEGATE),
            (Self::SelfExe { .. }, Helper::ApiAgent) => Some(FLAG_API_AGENT),
            (_, Helper::Computer) => Some(FLAG_COMPUTER_MCP),
            (_, Helper::ToolGateway) => Some(FLAG_TOOL_GATEWAY),
            (_, Helper::Connector) => Some(FLAG_CONNECTOR_MCP),
            (Self::SelfExe { .. }, Helper::Work) => Some(FLAG_WORK_MCP),
            (Self::Separate { .. }, Helper::MemoryMcp | Helper::ApiAgent | Helper::Work) => None,
        };
        flag.into_iter().map(str::to_owned).collect()
    }
}

/// Memory MCP server over stdio. `args` are the arguments after the mode flag (`--project <dir>`,
/// `--session <id>`). Data directory: `HARNESS_HOME`, else `~/.harness`. The Harness session id comes from
/// `--session`, else `HARNESS_SESSION_ID`; without one the `session_notes_*` tools answer with a clear error.
pub fn run_memory_mcp(args: impl IntoIterator<Item = String>) -> ExitCode {
    let mut project = None;
    let mut session = None;
    let mut agent = None;
    let mut args = args.into_iter();
    while let Some(arg) = args.next() {
        if arg == "--project" {
            project = args.next().map(PathBuf::from);
        } else if arg == "--session" {
            session = args.next();
        } else if arg == "--agent" {
            agent = args.next().filter(|name| !name.trim().is_empty());
        }
    }
    let home = match env_path("HARNESS_HOME") {
        Some(home) => home,
        None => match crate::config::HubConfig::from_env() {
            Ok(config) => config.home,
            Err(error) => {
                eprintln!("harness memory-mcp: {error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let memory = MemoryFiles::new(home.as_path());
    let stdin = BufReader::new(io::stdin().lock());
    let session = session.or_else(|| {
        std::env::var("HARNESS_SESSION_ID")
            .ok()
            .filter(|id| !id.trim().is_empty())
    });
    match crate::mcp::serve(
        &memory,
        &home,
        project.as_deref(),
        session.as_deref(),
        agent.as_deref(),
        stdin,
        io::stdout().lock(),
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness memory-mcp: {error}");
            ExitCode::FAILURE
        }
    }
}

/// `harness-delegate` MCP server over stdio, configured by the `HARNESS_DELEGATE_*` env vars (§9.8).
#[must_use = "return the exit code from main"]
pub fn run_delegate_mcp() -> ExitCode {
    let Some(env) = DelegateEnv::from_env() else {
        eprintln!("harness delegate: HARNESS_DELEGATE_ADDR/TOKEN/SESSION are not set");
        return ExitCode::FAILURE;
    };
    let stdin = BufReader::new(io::stdin());
    match delegate_mcp::serve(&env, stdin, Box::new(io::stdout())) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness delegate: {error}");
            ExitCode::FAILURE
        }
    }
}

/// Connector bridge over stdio; `args` = `[<connector id>]`.
#[must_use = "return the exit code from main"]
pub fn run_connector_mcp(args: impl IntoIterator<Item = String>) -> ExitCode {
    harness_connectors::bridge::run(args)
}

/// `harness-tools` gateway over stdio: fronts the stdio overlay servers of `HARNESS_GATEWAY_AGENT`, read from
/// `<HARNESS_HOME>/mcp.json` here so no server env passes through the agent.
#[must_use = "return the exit code from main"]
pub fn run_tool_gateway() -> ExitCode {
    let home = match env_path("HARNESS_HOME") {
        Some(home) => home,
        None => match crate::config::HubConfig::from_env() {
            Ok(config) => config.home,
            Err(error) => {
                eprintln!("harness tool-gateway: {error}");
                return ExitCode::FAILURE;
            }
        },
    };
    let agent = std::env::var("HARNESS_GATEWAY_AGENT").unwrap_or_default();
    let servers = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| e.to_string())
        .and_then(|rt| {
            rt.block_on(harness_agent_config::overlay::servers(&home, &agent))
                .map_err(|e| e.to_string())
        });
    let servers = match servers {
        Ok(mut servers) => {
            // 连接器也经网关提供给 agy/API 提供商；本进程就是 Harness 可执行文件。
            if let Ok(exe) = std::env::current_exe() {
                let launch = HelperLaunch::SelfExe { program: exe };
                let args = launch.args(Helper::Connector);
                let connectors = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .map(|rt| {
                        rt.block_on(harness_connectors::servers(
                            &home,
                            launch.program(Helper::Connector),
                            &args,
                        ))
                    })
                    .unwrap_or_default();
                servers.extend(connectors);
            }
            servers
        }
        Err(error) => {
            eprintln!("harness tool-gateway: {error}");
            return ExitCode::FAILURE;
        }
    };
    let mut gateway = crate::tool_gateway::Gateway::new(servers);
    match gateway.serve(BufReader::new(io::stdin().lock()), io::stdout().lock()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("harness tool-gateway: {error}");
            ExitCode::FAILURE
        }
    }
}
