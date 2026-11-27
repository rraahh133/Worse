//! MCP authorization (2025-06-18): find the authorization server of a remote MCP server, register Harness as a
//! public client, sign in through the browser with PKCE and a loopback redirect, refresh tokens.

use std::path::Path;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::http::{auth_param, challenge, client, random};
use crate::state::{now, OwnClient, Token, Tokens};
use crate::{ConnectorError, Result};

mod discover;

pub use discover::{discover, is_public, Discovered, ServerMeta};

/// [`is_public`] with Harness's HTTP client.
///
/// # Errors
///
/// The network failed.
pub async fn check_public(url: &str) -> Result<bool> {
    is_public(&client()?, url).await
}

/// How long the browser sign-in may take.
pub const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

/// What a login needs besides the connector: the user's own client for services without registration.
#[derive(Debug, Clone, Default)]
pub struct LoginOptions {
    pub own_client: Option<OwnClient>,
}

#[derive(Deserialize)]
struct Registered {
    client_id: String,
    #[serde(default)]
    client_secret: Option<String>,
}

#[derive(Deserialize)]
struct TokenAnswer {
    access_token: String,
    #[serde(default)]
    refresh_token: Option<String>,
    #[serde(default)]
    expires_in: Option<u64>,
}

/// The key own clients are stored under: the authorization server's host.
#[must_use]
pub fn client_key(meta: &ServerMeta) -> String {
    reqwest::Url::parse(&meta.authorization_endpoint)
        .ok()
        .and_then(|u| u.host_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Signs in to the connector `id` at `url`: discovery, client registration (or the user's own client), browser
/// login through `open`, token exchange; the token is stored in `tokens.json`.
///
/// # Errors
///
/// [`ConnectorError::NeedsClient`] when the service has no registration and no own client is known; otherwise
/// discovery, network, login or file errors.
pub async fn login(
    home: &Path,
    id: &str,
    url: &str,
    options: LoginOptions,
    open: impl FnOnce(&str),
) -> Result<()> {
    let http = client()?;
    let found = discover(&http, url).await?;
    let key = client_key(&found.meta);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", 0)).await?;
    let redirect = format!(
        "http://127.0.0.1:{}/callback",
        listener.local_addr()?.port()
    );
    let stored = Tokens::load(home).await;
    let mut shipped = false;
    let (client_id, client_secret) = if let Some(own) = options
        .own_client
        .or_else(|| stored.clients.get(&key).cloned())
    {
        (own.client_id, own.client_secret)
    } else if let Some(endpoint) = &found.meta.registration_endpoint {
        let registered = register(&http, endpoint, &redirect).await?;
        (registered.client_id, registered.client_secret)
    } else if let Some(builtin) = crate::builtin::client(&key) {
        // 修复：Google 等不支持动态注册的服务，原来必须让用户自建 OAuth client；现在先用 Harness 自带的。
        shipped = true;
        (builtin.client_id, builtin.client_secret)
    } else {
        return Err(ConnectorError::NeedsClient(key));
    };
    let verifier = random(48);
    let state = random(24);
    let mut authorize = reqwest::Url::parse(&found.meta.authorization_endpoint)
        .map_err(|_| ConnectorError::Remote("bad authorization endpoint".to_owned()))?;
    {
        let mut q = authorize.query_pairs_mut();
        q.append_pair("response_type", "code")
            .append_pair("client_id", &client_id)
            .append_pair("redirect_uri", &redirect)
            .append_pair("code_challenge", &challenge(&verifier))
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", &state)
            .append_pair("resource", &found.resource);
        if !found.scopes.is_empty() {
            q.append_pair("scope", &found.scopes.join(" "));
        }
        if key == "accounts.google.com" {
            // Google 只有 access_type=offline 且重新同意时才给 refresh_token。
            q.append_pair("access_type", "offline")
                .append_pair("prompt", "consent");
        }
    }
    open(authorize.as_str());
    let code = tokio::time::timeout(LOGIN_TIMEOUT, wait_for_code(&listener, &state))
        .await
        .map_err(|_| ConnectorError::Login("sign-in timed out".to_owned()))??;
    let mut form = vec![
        ("grant_type", "authorization_code".to_owned()),
        ("code", code),
        ("redirect_uri", redirect),
        ("client_id", client_id.clone()),
        ("code_verifier", verifier),
        ("resource", found.resource.clone()),
    ];
    if let Some(secret) = &client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let answer = token_request(&http, &found.meta.token_endpoint, &form).await?;
    let token = Token {
        client_id,
        client_secret,
        token_endpoint: found.meta.token_endpoint.clone(),
        resource: found.resource,
        access_token: answer.access_token,
        refresh_token: answer.refresh_token,
        expires_at: answer.expires_in.map(|s| now() + s),
    };
    // 自带的 client 不记成用户自己的，这样更新 Harness 换了 client 也能生效。
    let own = (!shipped)
        .then(|| options_client(&found.meta, &token))
        .flatten();
    Tokens::update(home, |t| {
        t.tokens.insert(id.to_owned(), token);
        if let Some(own) = own {
            t.clients.insert(key, own);
        }
    })
    .await
}

/// Remembers the client when it was the user's own (no registration endpoint), so the next service on the same
/// authorization server signs in without asking again.
fn options_client(meta: &ServerMeta, token: &Token) -> Option<OwnClient> {
    meta.registration_endpoint.is_none().then(|| OwnClient {
        client_id: token.client_id.clone(),
        client_secret: token.client_secret.clone(),
    })
}

async fn register(http: &reqwest::Client, endpoint: &str, redirect: &str) -> Result<Registered> {
    let response = http
        .post(endpoint)
        .json(&json!({
            "client_name": "Harness",
            "redirect_uris": [redirect],
            "grant_types": ["authorization_code", "refresh_token"],
            "response_types": ["code"],
            "token_endpoint_auth_method": "none",
        }))
        .send()
        .await?;
    if !response.status().is_success() {
        return Err(ConnectorError::Remote(format!(
            "client registration was refused ({})",
            response.status()
        )));
    }
    response
        .json()
        .await
        .map_err(|_| ConnectorError::Remote("unreadable client registration".to_owned()))
}

async fn token_request(
    http: &reqwest::Client,
    endpoint: &str,
    form: &[(&str, String)],
) -> Result<TokenAnswer> {
    let response = http
        .post(endpoint)
        .header("accept", "application/json")
        .form(form)
        .send()
        .await?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = body
            .get("error_description")
            .or_else(|| body.get("error"))
            .and_then(Value::as_str)
            .unwrap_or("no reason given");
        return Err(ConnectorError::Login(format!(
            "the service refused the token request ({status}: {reason})"
        )));
    }
    serde_json::from_value(body)
        .map_err(|_| ConnectorError::Remote("the token answer has no access token".to_owned()))
}

/// Accepts browser requests on the loopback listener until the callback with `state` arrives.
async fn wait_for_code(listener: &tokio::net::TcpListener, state: &str) -> Result<String> {
    loop {
        let (mut stream, _) = listener.accept().await?;
        let mut buf = vec![0u8; 8192];
        let n = stream.read(&mut buf).await.unwrap_or(0);
        let request = String::from_utf8_lossy(&buf[..n]);
        let target = request
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .unwrap_or("/");
        let Ok(url) = reqwest::Url::parse(&format!("http://127.0.0.1{target}")) else {
            continue;
        };
        if url.path() != "/callback" {
            let _ = stream
                .write_all(
                    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await;
            continue;
        }
        let get = |k: &str| {
            url.query_pairs()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.into_owned())
        };
        let (title, result) = if get("state").as_deref() != Some(state) {
            (
                "Sign-in failed",
                Err(ConnectorError::Login(
                    "the sign-in answer did not match".to_owned(),
                )),
            )
        } else if let Some(code) = get("code") {
            ("Connected", Ok(code))
        } else {
            let reason = get("error_description")
                .or_else(|| get("error"))
                .unwrap_or_else(|| "no code".to_owned());
            (
                "Sign-in failed",
                Err(ConnectorError::Login(format!("sign-in failed: {reason}"))),
            )
        };
        let page = format!(
            "<!doctype html><meta charset=utf-8><title>Harness</title><body style=\"font-family:system-ui;background:#111;color:#eee;display:grid;place-items:center;height:90vh\"><div><h2>{title}</h2><p>You can close this tab and go back to Harness.</p></div>"
        );
        let _ = stream
            .write_all(
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{page}",
                    page.len()
                )
                .as_bytes(),
            )
            .await;
        return result;
    }
}

/// A usable access token of connector `id`, refreshed when stale (or when `force`). `None`: not signed in.
///
/// # Errors
///
/// The refresh failed (the user must sign in again).
pub async fn access_token(home: &Path, id: &str, force: bool) -> Result<Option<String>> {
    let tokens = Tokens::load(home).await;
    let Some(token) = tokens.tokens.get(id) else {
        return Ok(None);
    };
    if !force && !token.stale(now()) {
        return Ok(Some(token.access_token.clone()));
    }
    let seen = token.access_token.clone();
    let _lock = crate::state::Lock::acquire(home).await;
    // 加锁后重读：别的桥接进程可能已经刷新过（refresh_token 可能只能用一次）。
    let mut tokens = Tokens::load(home).await;
    let Some(token) = tokens.tokens.get_mut(id) else {
        return Ok(None);
    };
    if token.access_token != seen && !token.stale(now()) {
        return Ok(Some(token.access_token.clone()));
    }
    let Some(refresh) = token.refresh_token.clone() else {
        return Err(ConnectorError::Login(
            "signed out: sign in again".to_owned(),
        ));
    };
    let mut form = vec![
        ("grant_type", "refresh_token".to_owned()),
        ("refresh_token", refresh),
        ("client_id", token.client_id.clone()),
    ];
    if !token.resource.is_empty() {
        form.push(("resource", token.resource.clone()));
    }
    if let Some(secret) = &token.client_secret {
        form.push(("client_secret", secret.clone()));
    }
    let answer = token_request(&client()?, &token.token_endpoint, &form).await?;
    token.access_token = answer.access_token;
    if answer.refresh_token.is_some() {
        token.refresh_token = answer.refresh_token;
    }
    token.expires_at = answer.expires_in.map(|s| now() + s);
    let fresh = token.access_token.clone();
    tokens.save(home).await?;
    Ok(Some(fresh))
}

/// Forgets the sign-in of connector `id` (own clients stay).
///
/// # Errors
///
/// Writing the file failed.
pub async fn forget(home: &Path, id: &str) -> Result<()> {
    Tokens::update(home, |t| {
        t.tokens.remove(id);
    })
    .await
}

/// `scope` and `resource_metadata` of a 401 answer's `WWW-Authenticate` header.
pub(crate) fn challenge_params(header: Option<&str>) -> (Option<String>, Option<String>) {
    header.map_or((None, None), |h| {
        (auth_param(h, "resource_metadata"), auth_param(h, "scope"))
    })
}
