//! Connectors half of the Plugins page (`docs/plans/connectors.md`): the catalog, what is connected, and the
//! sign-in flow. Sign-in runs on a worker thread (browser + loopback redirect); `connected.json` and
//! `tokens.json` are written only here and by `harness-connectors`.

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use harness_connectors::oauth::{self, LoginOptions};
use harness_connectors::state::OwnClient;
use harness_connectors::{
    catalog, Auth, Client, Connected, Connection, ConnectorError, Entry, Tokens,
};

use super::load::{block_on, run};
use super::{rows, with, PAGE};

/// Connector part of the page state.
#[derive(Default)]
pub(super) struct Connectors {
    pub(super) catalog: Vec<Entry>,
    pub(super) connected: Connected,
    /// Ids with a stored sign-in.
    pub(super) signed_in: BTreeSet<String>,
    pub(super) busy: Vec<String>,
    /// Last result per id: (text, is error).
    pub(super) notes: HashMap<String, (String, bool)>,
    pub(super) limit: usize,
    /// The own-client dialog: (connector id, authorization server).
    pub(super) client_for: Option<(String, String)>,
}

impl Connectors {
    pub(super) fn new() -> Self {
        Self {
            catalog: catalog::builtin(),
            limit: PAGE,
            ..Self::default()
        }
    }

    /// The entry of `id`: catalog first, else the user's custom one.
    pub(super) fn entry(&self, id: &str) -> Option<Entry> {
        self.catalog
            .iter()
            .find(|e| e.id == id)
            .cloned()
            .or_else(|| {
                self.connected
                    .connectors
                    .get(id)
                    .and_then(|c| c.custom.clone())
            })
    }
}

/// Reloads `connected.json` and which connectors are signed in.
pub(super) fn spawn_state() {
    let Some(home) = with(|s| s.data_dir.clone()) else {
        return;
    };
    run(
        move || {
            block_on(async {
                let connected = Connected::load(&home).await;
                let signed_in: BTreeSet<String> =
                    Tokens::load(&home).await.tokens.into_keys().collect();
                (connected, signed_in)
            })
            .unwrap_or_default()
        },
        |(connected, signed_in)| {
            with(|s| {
                s.connectors.connected = connected;
                s.connectors.signed_in = signed_in;
            });
            rows::push();
        },
    );
}

fn note(id: &str, text: &str, error: bool) {
    with(|s| {
        s.connectors.busy.retain(|b| b != id);
        if text.is_empty() {
            s.connectors.notes.remove(id);
        } else {
            s.connectors
                .notes
                .insert(id.to_owned(), (text.to_owned(), error));
        }
    });
}

async fn add(home: &Path, entry: &Entry, custom: bool) -> harness_connectors::Result<()> {
    let mut connected = Connected::load(home).await;
    connected.connectors.insert(
        entry.id.clone(),
        Connection {
            title: entry.title.clone(),
            url: entry.url.clone(),
            auth: entry.auth,
            enabled: true,
            custom: custom.then(|| entry.clone()),
        },
    );
    connected.save(home).await
}

/// Connect: public servers are added at once; OAuth ones sign in through the browser first.
pub(super) fn connect(id: &str, own: Option<OwnClient>) {
    let Some((home, entry, custom)) = with(|s| {
        let entry = s.connectors.entry(id)?;
        let custom = !s.connectors.catalog.iter().any(|e| e.id == id);
        if s.connectors.busy.iter().any(|b| b == id) {
            return None;
        }
        s.connectors.busy.push(id.to_owned());
        s.connectors.notes.remove(id);
        Some((s.data_dir.clone(), entry, custom))
    })
    .flatten() else {
        return;
    };
    rows::push();
    let id = id.to_owned();
    run(
        move || {
            block_on(async {
                if entry.auth == Auth::OAuth {
                    let options = LoginOptions { own_client: own };
                    oauth::login(
                        &home,
                        &entry.id,
                        &entry.url,
                        options,
                        crate::remote::open_url,
                    )
                    .await?;
                }
                add(&home, &entry, custom).await
            })
            .unwrap_or_else(|| Err(ConnectorError::Invalid("runtime failed".to_owned())))
        },
        move |result| {
            match result {
                Ok(()) => note(&id, "", false),
                Err(ConnectorError::NeedsClient(server)) => {
                    note(&id, "", false);
                    with(|s| s.connectors.client_for = Some((id.clone(), server)));
                }
                Err(error) => note(&id, &error.to_string(), true),
            }
            spawn_state();
        },
    );
}

/// Signs out and removes the connector (custom ones are gone afterwards).
pub(super) fn disconnect(id: &str) {
    let Some(home) = with(|s| s.data_dir.clone()) else {
        return;
    };
    let id = id.to_owned();
    run(
        move || {
            block_on(async {
                oauth::forget(&home, &id).await?;
                let mut connected = Connected::load(&home).await;
                connected.connectors.remove(&id);
                connected.save(&home).await
            })
        },
        |result| {
            if let Some(Err(error)) = result {
                super::fail(error.to_string());
            }
            spawn_state();
        },
    );
}

pub(super) fn set_enabled(id: &str, on: bool) {
    let Some(home) = with(|s| s.data_dir.clone()) else {
        return;
    };
    let id = id.to_owned();
    run(
        move || {
            block_on(async {
                let mut connected = Connected::load(&home).await;
                if let Some(c) = connected.connectors.get_mut(&id) {
                    c.enabled = on;
                }
                connected.save(&home).await
            })
        },
        |result| {
            if let Some(Err(error)) = result {
                super::fail(error.to_string());
            } else {
                super::status("Saved. New agent sessions pick up the change.");
            }
            spawn_state();
        },
    );
}

/// The own-client dialog was submitted.
pub(super) fn submit_client(id: &str, client_id: &str, secret: &str) {
    with(|s| s.connectors.client_for = None);
    let own = OwnClient {
        client_id: client_id.trim().to_owned(),
        client_secret: Some(secret.trim().to_owned()).filter(|s| !s.is_empty()),
    };
    connect(id, Some(own));
}

pub(super) fn cancel_client() {
    with(|s| s.connectors.client_for = None);
    rows::push();
}

/// "Add custom connector": checks the URL, finds out whether it needs a sign-in, then connects.
pub(super) fn add_custom(name: &str, url: &str) {
    let parsed = match catalog::check_url(url) {
        Ok(u) => u,
        Err(error) => {
            super::fail(error.to_string());
            return;
        }
    };
    let Some(taken) = with(|s| {
        s.connectors
            .connected
            .connectors
            .keys()
            .cloned()
            .collect::<Vec<_>>()
    }) else {
        return;
    };
    let id = catalog::custom_id(&parsed, &taken);
    let title = match name.trim() {
        "" => parsed.host_str().unwrap_or("Custom").to_owned(),
        t => t.to_owned(),
    };
    let mut entry = Entry {
        id: id.clone(),
        title,
        publisher: parsed.host_str().unwrap_or_default().to_owned(),
        description: "Custom connector".to_owned(),
        category: "custom".to_owned(),
        url: parsed.to_string(),
        homepage: parsed.origin().ascii_serialization(),
        auth: Auth::OAuth,
        client: Client::Dcr,
    };
    let Some(home) = with(|s| s.data_dir.clone()) else {
        return;
    };
    super::status("Checking the server…");
    run(
        move || {
            let public = block_on(oauth::check_public(&entry.url));
            match public {
                Some(Ok(true)) => entry.auth = Auth::None,
                Some(Ok(false)) => {}
                Some(Err(e)) => return Err(e.to_string()),
                None => return Err("runtime failed".to_owned()),
            }
            // 先登记为已添加但未登录的自定义条目，登录流程再按目录条目一样处理。
            block_on(async {
                let mut connected = Connected::load(&home).await;
                connected.connectors.insert(
                    entry.id.clone(),
                    Connection {
                        title: entry.title.clone(),
                        url: entry.url.clone(),
                        auth: entry.auth,
                        enabled: entry.auth == Auth::None,
                        custom: Some(entry.clone()),
                    },
                );
                connected.save(&home).await
            })
            .ok_or_else(|| "runtime failed".to_owned())?
            .map_err(|e| e.to_string())?;
            Ok(entry)
        },
        move |result| match result {
            Ok(entry) => {
                super::status("");
                with(|s| {
                    s.connectors
                        .connected
                        .connectors
                        .entry(entry.id.clone())
                        .or_insert_with(|| Connection {
                            title: entry.title.clone(),
                            url: entry.url.clone(),
                            auth: entry.auth,
                            enabled: false,
                            custom: Some(entry.clone()),
                        });
                });
                if entry.auth == Auth::OAuth {
                    connect(&id, None);
                } else {
                    spawn_state();
                }
            }
            Err(error) => super::fail(error),
        },
    );
}

/// Help for the own-client dialog: (text, redirect, docs URL).
pub(super) fn client_help(server: &str) -> (String, String, String) {
    let redirect = "http://127.0.0.1 (any port, loopback)".to_owned();
    match server {
        "accounts.google.com" => (
            "Google does not let apps register themselves. Create an OAuth client once and every Google connector uses it:\n1. Google Cloud Console > APIs & Services > Credentials > Create credentials > OAuth client ID.\n2. Application type: Desktop app.\n3. Enable the APIs you need (Drive, Gmail, Calendar, Docs, Sheets) and add yourself as a test user on the OAuth consent screen.\n4. Paste the client ID and secret here.".to_owned(),
            redirect,
            "https://console.cloud.google.com/apis/credentials".to_owned(),
        ),
        "github.com" => (
            "GitHub needs an OAuth app of your own:\n1. GitHub > Settings > Developer settings > OAuth Apps > New OAuth App.\n2. Authorization callback URL: http://127.0.0.1/callback\n3. Paste the client ID and a client secret here.".to_owned(),
            "http://127.0.0.1/callback".to_owned(),
            "https://github.com/settings/developers".to_owned(),
        ),
        "slack.com" => (
            "Slack needs an app of your own:\n1. api.slack.com/apps > Create New App.\n2. OAuth & Permissions: add the redirect URL below and the user scopes you need.\n3. Paste the client ID and client secret here.".to_owned(),
            "http://127.0.0.1/callback".to_owned(),
            "https://api.slack.com/apps".to_owned(),
        ),
        other => (
            format!("{other} does not let apps register themselves. Create an OAuth app in the service's developer settings with the redirect URI below, then paste its client ID (and secret, if it has one)."),
            redirect,
            String::new(),
        ),
    }
}
