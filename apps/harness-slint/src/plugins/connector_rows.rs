//! Projects the connector state into `PluginsUi` (Yours / More you can add, the own-client dialog).

use harness_connectors::{Auth, Client, Entry};
use slint::{ModelRc, SharedString, VecModel};

use super::rows::logo;
use super::State;
use crate::ui::{ConnectorRow, PluginsUi};

fn sv(s: &str) -> SharedString {
    SharedString::from(s)
}

fn row(s: &State, entry: &Entry, custom: bool) -> ConnectorRow {
    let c = &s.connectors;
    let connection = c.connected.connectors.get(&entry.id);
    let public = entry.auth == Auth::None;
    // OAuth 连接器只有在已登录时才算已连接；公开服务器添加即可。
    let connected = connection.is_some() && (public || c.signed_in.contains(&entry.id));
    let (note, note_error) = c.notes.get(&entry.id).cloned().unwrap_or_else(|| {
        if connection.is_some() && !connected {
            ("Signed out: connect again".to_owned(), true)
        } else {
            (String::new(), false)
        }
    });
    let (icon, has_icon) = logo(s, Some(&entry.homepage));
    ConnectorRow {
        id: sv(&entry.id),
        title: sv(&entry.title),
        publisher: sv(&entry.publisher),
        description: sv(&entry.description),
        category: sv(&entry.category),
        url: sv(&entry.url),
        homepage: sv(&entry.homepage),
        icon,
        has_icon,
        verified: !custom,
        connected,
        enabled: connected && connection.is_some_and(|x| x.enabled),
        public,
        own_client: entry.client == Client::Own,
        custom,
        busy: c.busy.contains(&entry.id),
        note: sv(&note),
        note_error,
    }
}

fn matches(r: &ConnectorRow, query: &str, category: &str) -> bool {
    let q = query.trim().to_lowercase();
    (category.is_empty() || r.category.as_str() == category)
        && (q.is_empty()
            || [&r.title, &r.description, &r.publisher, &r.category, &r.id]
                .iter()
                .any(|f| f.to_lowercase().contains(&q)))
}

pub(super) fn push(s: &State, ui: &PluginsUi<'_>) {
    let c = &s.connectors;
    let mut all: Vec<ConnectorRow> = c.catalog.iter().map(|e| row(s, e, false)).collect();
    for connection in c.connected.connectors.values() {
        if let Some(entry) = &connection.custom {
            all.push(row(s, entry, true));
        }
    }
    let (yours, more): (Vec<ConnectorRow>, Vec<ConnectorRow>) = all
        .iter()
        .filter(|r| matches(r, &s.query, &s.category))
        .cloned()
        .partition(|r| r.connected || r.busy || c.connected.connectors.contains_key(r.id.as_str()));
    let yours = if s.tab == "discover" {
        Vec::new()
    } else {
        yours
    };
    let more = if s.tab == "yours" { Vec::new() } else { more };
    let hidden = more.len().saturating_sub(c.limit);
    let more: Vec<ConnectorRow> = more.into_iter().take(c.limit).collect();
    ui.set_connectors_yours(ModelRc::new(VecModel::from(yours)));
    ui.set_connectors(ModelRc::new(VecModel::from(more)));
    ui.set_connectors_hidden(i32::try_from(hidden).unwrap_or(i32::MAX));
    if s.section == "connectors" {
        let mut categories: Vec<String> = all
            .iter()
            .map(|r| r.category.to_string())
            .filter(|x| !x.is_empty())
            .collect();
        categories.sort();
        categories.dedup();
        ui.set_categories(ModelRc::new(VecModel::from(
            categories.iter().map(|x| sv(x)).collect::<Vec<_>>(),
        )));
    }
    match &c.client_for {
        Some((id, server)) => {
            let target = all
                .iter()
                .find(|r| r.id.as_str() == id)
                .cloned()
                .unwrap_or_default();
            let (help, redirect, docs) = super::connectors::client_help(server);
            ui.set_client_for(target);
            ui.set_client_server(sv(server));
            ui.set_client_help(sv(&help));
            ui.set_client_redirect(sv(&redirect));
            ui.set_client_docs_url(sv(&docs));
            ui.set_client_open(true);
        }
        None => ui.set_client_open(false),
    }
}
