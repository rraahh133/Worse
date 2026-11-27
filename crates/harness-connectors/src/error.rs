//! Errors of connector operations. Messages are shown to the user; they never contain tokens.

/// A connector operation failed.
#[derive(Debug, thiserror::Error)]
pub enum ConnectorError {
    /// Bad input (unknown connector, malformed URL, missing client id).
    #[error("{0}")]
    Invalid(String),
    /// The server or its authorization server answered with something unusable.
    #[error("{0}")]
    Remote(String),
    /// The service does not register clients on the fly: the user must bring an OAuth client id for this
    /// authorization server (its host is the payload).
    #[error("{0} needs an OAuth client id of your own")]
    NeedsClient(String),
    /// The user did not finish signing in in time, or the browser reported an error.
    #[error("{0}")]
    Login(String),
    /// Network failure.
    #[error("network error: {0}")]
    Http(String),
    /// Reading or writing the connector files failed.
    #[error("{0}")]
    Io(#[from] std::io::Error),
}

impl From<reqwest::Error> for ConnectorError {
    fn from(error: reqwest::Error) -> Self {
        // reqwest 的错误文本可能带完整 URL（含 code/token 查询参数），只保留种类与主机。
        let host = error
            .url()
            .and_then(|u| u.host_str().map(str::to_owned))
            .unwrap_or_default();
        let kind = if error.is_timeout() {
            "timed out"
        } else if error.is_connect() {
            "could not connect"
        } else if error.is_decode() {
            "unreadable answer"
        } else {
            "request failed"
        };
        Self::Http(if host.is_empty() {
            kind.to_owned()
        } else {
            format!("{kind} ({host})")
        })
    }
}

/// Result of connector operations.
pub type Result<T> = std::result::Result<T, ConnectorError>;
