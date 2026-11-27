//! Page actions: detail, install / update, switch on/off, uninstall, marketplaces.

use std::path::PathBuf;

use harness_plugins::store::{self, MarketplaceSource, DEFAULT_MARKETPLACE};
use harness_plugins::{Listing, Plugin};

use super::load::{block_on, read_plugin, run, spawn_markets};
use super::{fail, plan, rows, with, PlanOp, Req, BUILTIN, BUILTINS};

/// `(marketplace, name)` of a key `"<marketplace>/<name>"`.
fn split(key: &str) -> Option<(&str, &str)> {
    key.split_once('/')
}

type Located = (
    PathBuf,
    Option<(PathBuf, String, Listing)>,
    Option<(PathBuf, String, Option<String>)>,
);

/// Everything a background read of `key` needs, taken from the state.
fn locate(key: &str) -> Option<Located> {
    let (market, name) = split(key)?;
    with(|s| {
        let listing = s.markets.iter().find_map(|m| {
            let data = m.data.as_ref().filter(|d| d.name == market)?;
            data.get(name)
                .map(|l| (m.dir.clone(), data.name.clone(), l.clone()))
        });
        let installed = s
            .installed
            .plugins
            .get(name)
            .filter(|r| r.marketplace == market)
            .map(|r| (r.root.clone(), r.marketplace.clone(), r.rev.clone()));
        (s.data_dir.clone(), listing, installed)
    })
}

pub(super) fn open_detail(key: &str) {
    let key = key.to_owned();
    with(|s| {
        s.detail_key = Some(key.clone());
        s.detail = None;
        s.detail_loading = !key.starts_with(BUILTIN);
    });
    rows::push();
    if key.starts_with(BUILTIN) {
        return;
    }
    let Some((data_dir, listing, installed)) = locate(&key) else {
        return;
    };
    run(
        move || read_plugin(&data_dir, listing, installed),
        move |result| {
            with(|s| {
                if s.detail_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                s.detail_loading = false;
                match result {
                    Ok((plugin, _, _)) => s.detail = Some(plugin),
                    Err(e) => {
                        s.status = e;
                        s.status_error = true;
                    }
                }
            });
            rows::push();
        },
    );
}

fn builtin(key: &str) -> Option<&'static str> {
    let (market, name) = split(key)?;
    (market == BUILTIN)
        .then(|| BUILTINS.iter().find(|b| b.0 == name).map(|b| b.0))
        .flatten()
}

fn start(key: &str) -> bool {
    with(|s| {
        if s.busy.is_some() {
            return false;
        }
        s.busy = Some(key.to_owned());
        s.status.clear();
        s.status_error = false;
        true
    })
    .unwrap_or(false)
}

/// Reads the plugin in the background, then asks the worker for the install plan.
fn plan_install(key: &str, title: &'static str) {
    let Some((data_dir, listing, installed)) = locate(key) else {
        fail(format!("unknown plugin {key}"));
        return;
    };
    run(
        move || read_plugin(&data_dir, listing, installed),
        move |result: Result<(Plugin, String, Option<String>), String>| match result {
            Ok((plugin, marketplace, rev)) => plan(
                title,
                PlanOp::InstallPlugin {
                    plugin: Box::new(plugin),
                    marketplace,
                    rev,
                },
            ),
            Err(e) => fail(e),
        },
    );
    rows::push();
}

pub(super) fn install(key: &str) {
    if !start(key) {
        return;
    }
    if let Some(name) = builtin(key) {
        send_builtin(name, true);
        return;
    }
    plan_install(key, "install plugin");
}

pub(super) fn uninstall(key: &str) {
    let Some((_, name)) = split(key) else { return };
    if builtin(key).is_some() || !start(key) {
        return;
    }
    plan(
        "uninstall plugin",
        PlanOp::RemovePlugin {
            name: name.to_owned(),
            keep: false,
        },
    );
    rows::push();
}

pub(super) fn set_enabled(key: &str, on: bool) {
    if !start(key) {
        return;
    }
    if let Some(name) = builtin(key) {
        send_builtin(name, on);
        return;
    }
    let Some((_, name)) = split(key) else { return };
    if on {
        plan_install(key, "switch plugin on");
    } else {
        plan(
            "switch plugin off",
            PlanOp::RemovePlugin {
                name: name.to_owned(),
                keep: true,
            },
        );
        rows::push();
    }
}

fn send_builtin(name: &str, on: bool) {
    let sent = with(|s| {
        s.req
            .send(Req::PluginBuiltin {
                name: name.to_owned(),
                on,
            })
            .is_ok()
    })
    .unwrap_or(false);
    if !sent {
        fail("the settings worker is not running".to_owned());
    }
    rows::push();
}

fn edit_markets(
    change: impl FnOnce(&mut Vec<MarketplaceSource>) -> Result<(), String> + Send + 'static,
) {
    let Some(data_dir) = with(|s| s.data_dir.clone()) else {
        return;
    };
    run(
        move || -> Result<(), String> {
            block_on(async {
                let mut list = store::load(&data_dir).await;
                change(&mut list)?;
                store::save(&data_dir, &list)
                    .await
                    .map_err(|e| e.to_string())
            })
            .unwrap_or_else(|| Err("runtime failed".to_owned()))
        },
        |result| match result {
            Ok(()) => {
                with(|s| s.last_fetch = None);
                spawn_markets(false);
            }
            Err(e) => fail(e),
        },
    );
}

pub(super) fn add_market(source: &str) {
    let source = MarketplaceSource::new(source.trim());
    if source.local_dir().is_none() {
        if let Err(e) = source.git_url() {
            fail(e.to_string());
            return;
        }
    }
    edit_markets(move |list| {
        if list.iter().any(|m| m.source == source.source) {
            return Err(format!("{} is already in the list", source.source));
        }
        list.push(source);
        Ok(())
    });
}

pub(super) fn remove_market(source: &str) {
    if source == DEFAULT_MARKETPLACE {
        return;
    }
    let source = source.to_owned();
    edit_markets(move |list| {
        list.retain(|m| m.source != source);
        Ok(())
    });
}
