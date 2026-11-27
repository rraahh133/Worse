//! OAuth clients shipped with Harness (`assets/clients.json`) for authorization servers that do not register
//! clients on the fly (Google). With one, signing in is one click: no client of the user's own is needed.

use serde::Deserialize;
use serde_json::Value;

use crate::state::OwnClient;

const CLIENTS: &str = include_str!("../assets/clients.json");

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Shipped {
    #[serde(default)]
    client_id: String,
    #[serde(default)]
    client_secret: String,
}

/// The shipped client for the authorization server `key` (its host), if any.
#[must_use]
pub fn client(key: &str) -> Option<OwnClient> {
    let all: Value = serde_json::from_str(CLIENTS).ok()?;
    let shipped: Shipped = serde_json::from_value(all.get(key)?.clone()).ok()?;
    let client_id = shipped.client_id.trim().to_owned();
    if client_id.is_empty() {
        return None;
    }
    let secret = shipped.client_secret.trim().to_owned();
    Some(OwnClient {
        client_id,
        client_secret: Some(secret).filter(|s| !s.is_empty()),
    })
}

#[cfg(test)]
mod tests {
    use super::client;

    #[test]
    fn google_has_a_shipped_client() {
        let google = client("accounts.google.com").unwrap();
        assert!(google.client_id.ends_with(".apps.googleusercontent.com"));
        assert!(client("example.com").is_none());
        assert!(client("_comment").is_none());
    }
}
