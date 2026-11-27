//! The HTTP client and the small parsers shared by login and the bridge.

use std::time::Duration;

use crate::Result;

/// A client with the ring TLS provider installed, no total timeout (tool calls and streams may run long).
pub(crate) fn client() -> Result<reqwest::Client> {
    let _ = rustls::crypto::ring::default_provider().install_default();
    Ok(reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(30))
        .user_agent(concat!("Harness/", env!("CARGO_PKG_VERSION")))
        .build()?)
}

/// `key="value"` (or `key=value`) of a `WWW-Authenticate` header.
pub(crate) fn auth_param(header: &str, key: &str) -> Option<String> {
    let lower = header.to_ascii_lowercase();
    let needle = format!("{}=", key.to_ascii_lowercase());
    let mut from = 0;
    while let Some(pos) = lower[from..].find(&needle) {
        let at = from + pos;
        let boundary = at == 0 || matches!(lower.as_bytes()[at - 1], b' ' | b',' | b'\t');
        let rest = &header[at + needle.len()..];
        if boundary {
            return Some(if let Some(quoted) = rest.strip_prefix('"') {
                quoted.split('"').next().unwrap_or_default().to_owned()
            } else {
                rest.split([',', ' ']).next().unwrap_or_default().to_owned()
            });
        }
        from = at + needle.len();
    }
    None
}

/// Server-sent events: feed bytes, take out the `data` of each finished event.
#[derive(Default)]
pub(crate) struct Sse {
    buffer: String,
    data: Vec<String>,
}

impl Sse {
    pub(crate) fn push(&mut self, chunk: &[u8]) -> Vec<String> {
        self.buffer.push_str(&String::from_utf8_lossy(chunk));
        let mut events = Vec::new();
        while let Some(end) = self.buffer.find('\n') {
            let line: String = self.buffer.drain(..=end).collect();
            let line = line.trim_end_matches(['\n', '\r']);
            if line.is_empty() {
                if !self.data.is_empty() {
                    events.push(self.data.join("\n"));
                    self.data.clear();
                }
            } else if let Some(value) = line.strip_prefix("data:") {
                self.data
                    .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
            }
        }
        events
    }

    /// The last event when the stream ended without a blank line.
    pub(crate) fn finish(&mut self) -> Option<String> {
        if let Some(value) = self.buffer.trim_end().strip_prefix("data:") {
            self.data
                .push(value.strip_prefix(' ').unwrap_or(value).to_owned());
        }
        self.buffer.clear();
        (!self.data.is_empty()).then(|| std::mem::take(&mut self.data).join("\n"))
    }
}

/// Random bytes as base64url without padding.
pub(crate) fn random(len: usize) -> String {
    use base64::Engine as _;
    let mut bytes = vec![0u8; len];
    if getrandom::fill(&mut bytes).is_err() {
        // 系统随机源不可用时退回到时间与进程号混合（仅用于 state/verifier，不会静默复用）。
        let seed = crate::state::now() ^ u64::from(std::process::id()).rotate_left(32);
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (seed.rotate_left(u32::try_from(i % 64).unwrap_or(0)) & 0xff) as u8;
        }
    }
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes)
}

/// PKCE S256 challenge of `verifier`.
pub(crate) fn challenge(verifier: &str) -> String {
    use base64::Engine as _;
    use sha2::Digest as _;
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()))
}
