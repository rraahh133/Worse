//! Discovery: the 401 challenge, protected-resource metadata (RFC 9728), authorization-server metadata
//! (RFC 8414 / `OpenID`), with the fallbacks older MCP servers need.

use serde::Deserialize;
use serde_json::json;

use super::challenge_params;
use crate::{ConnectorError, Result};

/// Endpoints of the authorization server.
#[derive(Debug, Clone, Deserialize)]
pub struct ServerMeta {
    pub authorization_endpoint: String,
    pub token_endpoint: String,
    #[serde(default)]
    pub registration_endpoint: Option<String>,
    #[serde(default)]
    pub scopes_supported: Vec<String>,
}

/// What a login needs.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub meta: ServerMeta,
    /// RFC 8707 resource: the protected-resource metadata's `resource`, else the server URL.
    pub resource: String,
    /// Scopes to ask for: the challenge's `scope`, else the resource's `scopes_supported`.
    pub scopes: Vec<String>,
}

#[derive(Deserialize, Default)]
struct ResourceMeta {
    #[serde(default)]
    resource: Option<String>,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

async fn get_json<T: for<'de> Deserialize<'de>>(http: &reqwest::Client, url: &str) -> Option<T> {
    let response = http
        .get(url)
        .header("accept", "application/json")
        .send()
        .await
        .ok()?;
    if !response.status().is_success() {
        return None;
    }
    response.json().await.ok()
}

/// `<origin>/.well-known/<name><path>` (path insertion), then `<origin>/.well-known/<name>`.
fn well_known(url: &reqwest::Url, name: &str) -> Vec<String> {
    let origin = url.origin().ascii_serialization();
    let path = url.path().trim_end_matches('/');
    let mut out = Vec::new();
    if !path.is_empty() {
        out.push(format!("{origin}/.well-known/{name}{path}"));
    }
    out.push(format!("{origin}/.well-known/{name}"));
    out
}

/// Finds everything a login needs for the MCP server at `url`.
///
/// # Errors
///
/// No authorization server could be found, or the network failed.
pub async fn discover(http: &reqwest::Client, url: &str) -> Result<Discovered> {
    let server = reqwest::Url::parse(url)
        .map_err(|_| ConnectorError::Invalid(format!("{url} is not a valid URL")))?;
    let response = http
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({
            "jsonrpc": "2.0", "id": 0, "method": "initialize",
            "params": {"protocolVersion": "2025-06-18", "capabilities": {},
                       "clientInfo": {"name": "Harness", "version": env!("CARGO_PKG_VERSION")}}
        }))
        .send()
        .await?;
    let header = response
        .headers()
        .get("www-authenticate")
        .and_then(|h| h.to_str().ok())
        .map(str::to_owned);
    let (metadata_url, scope) = challenge_params(header.as_deref());
    let mut resource = None;
    for candidate in metadata_url
        .into_iter()
        .chain(well_known(&server, "oauth-protected-resource"))
    {
        if let Some(found) = get_json::<ResourceMeta>(http, &candidate).await {
            if !found.authorization_servers.is_empty() {
                resource = Some(found);
                break;
            }
        }
    }
    let resource = resource.unwrap_or_default();
    let issuer = resource
        .authorization_servers
        .first()
        .cloned()
        .unwrap_or_else(|| server.origin().ascii_serialization());
    let issuer_url = reqwest::Url::parse(&issuer)
        .map_err(|_| ConnectorError::Remote(format!("bad authorization server {issuer}")))?;
    let mut candidates = well_known(&issuer_url, "oauth-authorization-server");
    candidates.extend(well_known(&issuer_url, "openid-configuration"));
    candidates.push(format!(
        "{}/.well-known/openid-configuration",
        issuer.trim_end_matches('/')
    ));
    let mut meta = None;
    for candidate in candidates {
        if let Some(found) = get_json::<ServerMeta>(http, &candidate).await {
            meta = Some(found);
            break;
        }
    }
    let meta = meta.ok_or_else(|| {
        ConnectorError::Remote(format!(
            "{} has no sign-in information Harness can use",
            server.host_str().unwrap_or(url)
        ))
    })?;
    let scopes: Vec<String> = match scope {
        Some(s) if !s.trim().is_empty() => s.split_whitespace().map(str::to_owned).collect(),
        _ => resource.scopes_supported,
    };
    Ok(Discovered {
        meta,
        resource: resource.resource.unwrap_or_else(|| url.to_owned()),
        scopes,
    })
}

/// True when the server at `url` answers `initialize` without a sign-in.
///
/// # Errors
///
/// The network failed.
pub async fn is_public(http: &reqwest::Client, url: &str) -> Result<bool> {
    let response = http
        .post(url)
        .header("content-type", "application/json")
        .header("accept", "application/json, text/event-stream")
        .json(&json!({"jsonrpc": "2.0", "id": 0, "method": "ping"}))
        .send()
        .await?;
    Ok(response.status() != reqwest::StatusCode::UNAUTHORIZED)
}
