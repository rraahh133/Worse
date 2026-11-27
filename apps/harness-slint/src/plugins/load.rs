//! Background reads: marketplace checkouts (git), the installed record, and single plugins. Each job runs on
//! its own thread with a current-thread runtime and posts its result back to the UI thread.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use harness_plugins::store::{self, MarketplaceSource};
use harness_plugins::{fetch, install, plugin, Installed, Listing, Plugin};

use super::{rows, with, Market};

/// Marketplaces are re-downloaded at most this often unless the user presses Refresh.
const REFRESH_AFTER: Duration = Duration::from_secs(24 * 60 * 60);

pub(super) fn run<T: Send + 'static>(
    job: impl FnOnce() -> T + Send + 'static,
    done: impl FnOnce(T) + Send + 'static,
) {
    std::thread::spawn(move || {
        let value = job();
        let _ = slint::invoke_from_event_loop(move || done(value));
    });
}

pub(super) fn block_on<F: std::future::Future>(future: F) -> Option<F::Output> {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .ok()
        .map(|rt| rt.block_on(future))
}

fn stale(dir: &Path) -> bool {
    let stamp = std::fs::metadata(dir.join(".git"))
        .and_then(|m| m.modified())
        .ok();
    stamp
        .and_then(|t| SystemTime::now().duration_since(t).ok())
        .map_or(true, |age| age >= REFRESH_AFTER)
}

/// Reads one marketplace, downloading it first when `force`, missing or stale.
fn read_market(data_dir: &Path, source: MarketplaceSource, force: bool) -> Market {
    let dir = store::checkout_dir(data_dir, &source);
    let mut error = String::new();
    if source.local_dir().is_none()
        && (force
            || !dir
                .join(harness_plugins::marketplace::MARKETPLACE_FILE)
                .is_file()
            || stale(&dir))
    {
        let fetched = source
            .git_url()
            .and_then(|url| fetch::checkout(&url, None, &dir));
        if let Err(e) = fetched {
            error = e.to_string();
        }
    }
    let data = match block_on(store::read_marketplace(&dir)) {
        Some(Ok(m)) => {
            // 下载失败但本地还有旧副本时照常显示旧列表，只提示错误。
            Some(m)
        }
        Some(Err(e)) => {
            if error.is_empty() {
                error = e.to_string();
            }
            None
        }
        None => None,
    };
    Market {
        source,
        dir,
        data,
        error,
    }
}

/// Reloads the marketplace list (downloading as needed), the installed record and the built-in switches.
pub(super) fn spawn_markets(force: bool) {
    let Some(data_dir) = with(|s| {
        let recent = s.last_fetch.is_some_and(|t| t.elapsed() < REFRESH_AFTER);
        if s.loading || (!force && recent) {
            return None;
        }
        s.loading = true;
        s.last_fetch = Some(std::time::Instant::now());
        Some(s.data_dir.clone())
    })
    .flatten() else {
        return;
    };
    rows::push();
    run(
        move || {
            let sources = block_on(store::load(&data_dir)).unwrap_or_default();
            let markets: Vec<Market> = sources
                .into_iter()
                .map(|source| read_market(&data_dir, source, force))
                .collect();
            let (installed, disabled) = read_state(&data_dir);
            (markets, installed, disabled)
        },
        |(markets, installed, disabled)| {
            with(|s| {
                s.loading = false;
                s.markets = markets;
                s.installed = installed;
                s.disabled_builtins = disabled;
            });
            rows::push();
        },
    );
}

fn read_state(data_dir: &Path) -> (Installed, Vec<String>) {
    block_on(async {
        (
            Installed::load(data_dir).await,
            harness_agent_config::overlay::disabled_builtins(data_dir).await,
        )
    })
    .unwrap_or_default()
}

/// Reloads only the installed record and built-in switches (after a write).
pub(super) fn spawn_state() {
    let Some(data_dir) = with(|s| s.data_dir.clone()) else {
        return;
    };
    run(
        move || read_state(&data_dir),
        |(installed, disabled)| {
            with(|s| {
                s.installed = installed;
                s.disabled_builtins = disabled;
            });
            rows::push();
        },
    );
}

/// A plugin ready to plan: read from its installed folder when that still exists, else located (and
/// downloaded) through its marketplace. Returns the plugin, its marketplace name and commit.
pub(super) fn read_plugin(
    data_dir: &Path,
    market: Option<(PathBuf, String, Listing)>,
    installed_root: Option<(PathBuf, String, Option<String>)>,
) -> Result<(Plugin, String, Option<String>), String> {
    if let Some((root, marketplace, rev)) = installed_root.filter(|r| r.0.is_dir()) {
        let listing = market.as_ref().map(|m| m.2.clone());
        let plugin = block_on(plugin::load(&root, listing.as_ref()))
            .ok_or("runtime failed")?
            .map_err(|e| e.to_string())?;
        return Ok((plugin, marketplace, rev));
    }
    let (dir, marketplace, listing) =
        market.ok_or("this plugin is no longer in any marketplace")?;
    let (root, rev) =
        install::locate(data_dir, &dir, &marketplace, &listing).map_err(|e| e.to_string())?;
    let plugin = block_on(plugin::load(&root, Some(&listing)))
        .ok_or("runtime failed")?
        .map_err(|e| e.to_string())?;
    Ok((plugin, marketplace, rev))
}
