use harness_agent_config::ConfigError;

/// Errors of the plugin store.
#[derive(Debug, thiserror::Error)]
pub enum PluginError {
    /// Bad marketplace or plugin data (also unsafe paths and names).
    #[error("{0}")]
    Invalid(String),
    /// A download failed (`git` missing, network, unknown revision).
    #[error("download failed: {0}")]
    Fetch(String),
    /// Reading a plugin or planning an agent configuration change failed.
    #[error(transparent)]
    Config(#[from] ConfigError),
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

impl PluginError {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }
}

pub type Result<T, E = PluginError> = std::result::Result<T, E>;
