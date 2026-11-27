use crate::catalog::{self, Auth, Client};
use crate::http::{auth_param, challenge, Sse};
use crate::state::{Connected, Connection, Token, Tokens};

#[test]
fn the_catalog_parses_with_unique_ids_and_https_urls() {
    let entries = catalog::builtin();
    assert!(entries.len() > 30);
    let mut ids: Vec<&str> = entries.iter().map(|e| e.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), entries.len());
    for entry in &entries {
        assert!(catalog::check_url(&entry.url).is_ok(), "{}", entry.url);
        assert!(!entry.description.is_empty(), "{}", entry.id);
    }
    let drive = catalog::get("google-drive").unwrap();
    assert_eq!(drive.client, Client::Own);
    assert_eq!(catalog::get("context7").unwrap().auth, Auth::None);
    assert_eq!(catalog::get("notion").unwrap().auth, Auth::OAuth);
}

#[test]
fn custom_urls_are_checked_and_get_free_ids() {
    assert!(catalog::check_url("http://example.com/mcp").is_err());
    assert!(catalog::check_url("https://user:pw@example.com/mcp").is_err());
    assert!(catalog::check_url("http://127.0.0.1:9000/mcp").is_ok());
    let url = catalog::check_url("https://mcp.acme.io/mcp").unwrap();
    assert_eq!(catalog::custom_id(&url, &[]), "acme");
    assert_eq!(catalog::custom_id(&url, &["acme".to_owned()]), "acme-2");
    let taken = catalog::check_url("https://mcp.notion.com/mcp").unwrap();
    assert_eq!(catalog::custom_id(&taken, &[]), "notion-2");
}

#[test]
fn challenge_headers_and_event_streams_are_read() {
    let header = r#"Bearer realm="OAuth", resource_metadata="https://x.test/.well-known/oauth-protected-resource/mcp", scope="read write""#;
    assert_eq!(
        auth_param(header, "resource_metadata").as_deref(),
        Some("https://x.test/.well-known/oauth-protected-resource/mcp")
    );
    assert_eq!(auth_param(header, "scope").as_deref(), Some("read write"));
    assert_eq!(
        auth_param("Bearer error=invalid_token", "error").as_deref(),
        Some("invalid_token")
    );
    assert_eq!(auth_param(header, "missing"), None);

    let mut sse = Sse::default();
    assert_eq!(sse.push(b"event: message\ndata: {\"a\":").len(), 0);
    let events = sse.push(b"1}\n\ndata: {\"b\":2}\r\n\r\ndata: {\"c\"");
    assert_eq!(events, vec!["{\"a\":1}".to_owned(), "{\"b\":2}".to_owned()]);
    sse.push(b":3}");
    assert_eq!(sse.finish().as_deref(), Some("{\"c\":3}"));

    // base64url(sha256(verifier)) without padding, checked against openssl.
    assert_eq!(
        challenge("dBjftJeZ4CVP-mJ0kzWzRRaR6rG_mX1qZBQ7oSPzAc"),
        "2yThaIUGJki7hwAMNDdeWpbYymjocLJz-so6fKJMwmw"
    );
}

#[test]
fn tokens_go_stale_shortly_before_they_expire() {
    let mut token = Token {
        access_token: "t".to_owned(),
        expires_at: Some(1_000),
        ..Token::default()
    };
    assert!(!token.stale(900));
    assert!(token.stale(950));
    token.expires_at = None;
    assert!(!token.stale(5_000));
    token.access_token.clear();
    assert!(token.stale(0));
}

#[tokio::test]
async fn connected_connectors_become_stdio_servers_and_files_round_trip() {
    let home = tempfile::tempdir().unwrap();
    let mut connected = Connected::default();
    for (id, enabled) in [("notion", true), ("linear", false)] {
        connected.connectors.insert(
            id.to_owned(),
            Connection {
                title: id.to_owned(),
                url: format!("https://mcp.{id}.test/mcp"),
                auth: Auth::OAuth,
                enabled,
                custom: None,
            },
        );
    }
    connected.save(home.path()).await.unwrap();
    assert_eq!(Connected::load(home.path()).await, connected);

    let exe = std::path::Path::new("C:/h/harness.exe");
    let servers = crate::servers(home.path(), exe, &["--connector-mcp".to_owned()]).await;
    assert_eq!(servers.len(), 1);
    assert_eq!(servers[0].name, "notion");
    assert_eq!(servers[0].args, vec!["--connector-mcp", "notion"]);
    assert_eq!(servers[0].command.as_deref(), Some("C:/h/harness.exe"));
    assert!(servers[0].env.contains_key("HARNESS_HOME"));

    Tokens::update(home.path(), |t| {
        t.tokens.insert(
            "notion".to_owned(),
            Token {
                access_token: "secret".to_owned(),
                ..Token::default()
            },
        );
    })
    .await
    .unwrap();
    assert_eq!(
        crate::oauth::access_token(home.path(), "notion", false)
            .await
            .unwrap()
            .as_deref(),
        Some("secret")
    );
    assert!(crate::oauth::access_token(home.path(), "linear", false)
        .await
        .unwrap()
        .is_none());
    crate::oauth::forget(home.path(), "notion").await.unwrap();
    assert!(Tokens::load(home.path()).await.tokens.is_empty());
    assert!(!crate::dir(home.path()).join("tokens.lock").exists());
}

/// Real discovery against the catalog: `HARNESS_CONNECTORS_LIVE=1 cargo test -p harness-connectors -- --ignored`.
#[tokio::test]
#[ignore = "network"]
async fn real_servers_are_discovered() {
    if std::env::var_os("HARNESS_CONNECTORS_LIVE").is_none() {
        return;
    }
    let http = crate::http::client().unwrap();
    for entry in catalog::builtin().iter().filter(|e| e.auth == Auth::OAuth) {
        match crate::oauth::discover(&http, &entry.url).await {
            Ok(found) => {
                let dcr = found.meta.registration_endpoint.is_some();
                println!(
                    "{:<16} dcr={dcr:<5} key={} scopes={}",
                    entry.id,
                    crate::oauth::client_key(&found.meta),
                    found.scopes.len()
                );
            }
            Err(error) => println!("{:<16} ERROR {error}", entry.id),
        }
    }
}
