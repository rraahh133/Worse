# Connectors

Remote MCP servers (Notion, Linear, Google Drive, …) that the user signs in to once in Harness. After that,
every agent gets their tools: Claude Code, Codex, agy and API / custom provider agents.

## Rules

1. **One sign-in, every agent.** Harness owns the OAuth login and the tokens. Agents never see a token: each
   connector runs as a stdio helper, `<harness exe> --connector-mcp <id>`, which forwards to the remote server
   and adds the token.
2. **Official servers only in the catalog.** `crates/harness-connectors/assets/catalog.json` lists remote
   servers run by the service itself. Each URL was probed: a 401 answer that names its protected-resource
   metadata, or a public 200. Users can add any https server by URL ("Add custom connector").
3. **No secrets in logs or UI.** `tokens.json` is read and written only by `harness-connectors`. Errors carry the
   host, never a URL that has a query string.

## Files (`<HARNESS_HOME>/connectors/`)

| File | Writer | Content |
| --- | --- | --- |
| `connected.json` | app (Plugins page) | id → title, url, auth, enabled, custom entry |
| `tokens.json` | app at sign-in; bridges on refresh | id → client id/secret, token endpoint, resource, access/refresh token, expiry; plus own OAuth clients keyed by authorization-server host |
| `tokens.lock` | bridges / app | cross-process lock around refresh (refresh tokens may be single-use) |

## Sign-in (MCP authorization 2025-06-18)

1. POST `initialize` without a token. From the 401, read `resource_metadata` and `scope` in `WWW-Authenticate`.
2. Protected-resource metadata (RFC 9728): try the `resource_metadata` URL first, then
   `/.well-known/oauth-protected-resource{path}`, then the root. It gives `authorization_servers`, `resource`
   and `scopes_supported`.
3. Authorization-server metadata (RFC 8414 / OpenID). Try the path-inserted and root
   `oauth-authorization-server`, then `openid-configuration`.
4. Client:
   - If the server has a `registration_endpoint`, use dynamic registration (RFC 7591): a public client
     (`token_endpoint_auth_method: none`) with a loopback redirect.
   - Otherwise use the user's own client for that authorization-server host. If there is none, the page opens
     the "Use your own OAuth app" dialog. This applies to Google, Slack, GitHub, HubSpot, Asana and Box.
5. Browser login with PKCE S256, `state`, RFC 8707 `resource` and the scopes. The redirect goes to
   `http://127.0.0.1:<port>/callback`. For Google the request also sends `access_type=offline&prompt=consent`.
6. Exchange the code for tokens and store them.

## Bridge

- Stdio JSON-RPC in, streamable HTTP out. Requests are forwarded concurrently. `initialize` and notifications
  are forwarded in order.
- The bridge keeps `Mcp-Session-Id` and the negotiated `MCP-Protocol-Version`, and relays both JSON and SSE
  answers line by line.
- Token handling:
  - The token is refreshed when it is less than 60 s from expiry.
  - On a 401, the bridge refreshes once and retries.
  - If that fails, the agent gets a JSON-RPC error: "<title> is signed out. Sign in again in Harness…".
- On stdin EOF the bridge sends `DELETE` to end the session.

## Where agents get connectors

- `hub/ctx.rs` `mcp_servers` merges `harness_connectors::servers()` into the overlay list before
  `overlay_entries`. Claude Code and Codex get the stdio entries directly. agy and API providers reach them
  through the `harness-tools` gateway, which `helpers::run_tool_gateway` also extends with connectors.
- A connector with the same name as a user overlay entry replaces that entry.

## UI

Settings › Plugins has two sections, Connectors and Plugins, laid out like Claude's Customize screen:
- "Yours" and "More you can add" lists, with search, category chips and "Show all".
- Icon tiles: the site icon, or the GitHub owner's avatar, cached in `<HARNESS_HOME>/cache/icons`. A letter
  tile is shown when there is no icon.
- Contract: `crates/harness-ui/ui/settings/plugins_ui.slint`.
- Rust: `apps/harness-slint/src/plugins/{connectors,connector_rows,icons}.rs`.

## Not yet

- Legacy HTTP+SSE transport servers (only streamable HTTP is supported).
- Server-initiated requests over the GET stream (sampling, elicitation).
- Client ID metadata documents (CIMD).
