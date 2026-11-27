# Plugins

A plugin store inside Harness: one place to find, install and switch off packaged extensions
(connectors such as Google Drive or Notion, Office documents, skills, commands, subagents) and have
them reach every agent Harness drives (Claude Code, Codex, agy, the API agent).

Supersedes the plugin part of `docs/specs/harness/phase-9-parity.md` §9.3 for the first phases;
the MCP-Apps WebView part of §9.3 stays the target of phase 3.

## 1. Product rules

1. **Claude Code plugin format, no new format.** A plugin is a directory with
   `.claude-plugin/plugin.json` and any of `skills/`, `commands/`, `agents/`, `.mcp.json`,
   `hooks/hooks.json`. A marketplace is a git repository (or local directory) with
   `.claude-plugin/marketplace.json`. Existing marketplaces work unchanged.
2. **One install, every agent.** Components are written through `harness-agent-config` for each
   agent that supports them: skills → Claude Code, Codex, Harness overlay; commands and subagents →
   Claude Code (others where the CLI has an equivalent); MCP servers → the Harness overlay
   `~/.harness/mcp.json`, which every agent receives through ACP `session/new`.
3. **Nothing is written without a diff.** Install, update, disable and uninstall produce a normal
   `WritePlan` that the user confirms in the existing diff dialog. The settings worker stays the only
   writer of agent configuration.
3a. **No agent is left out.** Not every agent reads skills or commands natively (checked
   2026-11-25: skills reach Claude Code and Codex only, commands Claude Code and Codex, subagents
   Claude Code; agy and the API agent / custom providers get MCP only). Harness closes the gap itself:
   - skills: a built-in `harness-skills` MCP server (`list_skills`, `read_skill`) added to every
     ACP `session/new` like `harness-memory`, serving the plugin skills (and `~/.harness/skills`);
   - commands: the composer expands `/<command>` into its prompt text before sending, so every agent
     receives plain text;
   - subagents: Claude Code only; other agents use Orchestra delegation.
   Agents with native support also get the native copy.
4. **Hooks from plugins are off.** Plugin hooks run shell commands with the user's rights and the
   Hooks page is hidden; phase 1 lists them in the plugin detail as "not installed" and never writes
   them.
5. **Credentials never touch plugin files.** Connector sign-in tokens live in the OS credential store
   (Windows Credential Manager), never in `mcp.json`, never in logs.
6. **Works offline for what is installed.** The Installed view reads disk and the installed record;
   only Discover needs the network.

## 2. Store layout (UI)

Settings › **Plugins** (new page, agent-config group, replaces the hidden Hooks slot):

- **Discover**: featured row (connectors + built-ins) and the marketplace list, search, filter by
  category (Connectors, Documents, Dev tools, Skills).
- **Installed**: each plugin with an on/off switch, the agents it reached, Update, Uninstall.
- **Plugin detail**: description, author, source repository, version, component list (skills,
  commands, subagents, MCP servers, hooks marked "not installed"), permissions the MCP servers ask
  for, Install button → diff dialog.
- **Marketplaces**: the default marketplace plus user-added ones (`owner/repo`, git URL or local
  folder), refresh, remove.

UI work is split: backend (crate, worker, Rust models) by the main agent, Slint page by the agy
subagent.

## 3. Built-in plugins

Shown in Discover as plugins but shipped inside the exe, so installing only flips them on:

| Plugin | What it is | State today |
| --- | --- | --- |
| Office documents (Word, Excel, PowerPoint) | `harness-work` MCP server | exists |
| Computer use | `harness-computer` MCP server | exists |
| Memory | `harness-memory` MCP server | exists |

## 4. Connectors

A connector is a plugin whose main component is an MCP server that needs sign-in. Featured set for
phase 2: Notion, Slack, GitHub, Linear, Asana, Atlassian, Figma, Canva, Box (all in the default
marketplace) plus Google Drive, Gmail and Google Calendar, which the default marketplace does not
carry (checked 2026-11-25) and need a second source chosen in phase 2. Remote (HTTP) MCP
servers with the MCP authorization flow (OAuth 2.1 + PKCE, browser sign-in, loopback redirect) are
preferred; Harness runs the flow, stores the token per connector in Credential Manager and injects it
as the `Authorization` header when it builds `session/new`. Servers without remote hosting run as
local `npx`/`uvx` commands with their own sign-in.

## 5. Data

- Download cache: `<HARNESS_HOME>/plugins/cache/<marketplace>/<plugin>/<version>/`.
- Installed record: `<HARNESS_HOME>/plugins/installed.json` — per plugin: marketplace, version,
  enabled, and the exact paths it wrote per agent (needed for a clean uninstall; disk is still the
  truth for "present").
- Marketplaces: `<HARNESS_HOME>/plugins/marketplaces.json`.

## 6. Phases

1. **Core store.** `crates/harness-plugins` (marketplace fetch + parse, plugin download, install /
   disable / uninstall plans, installed record), settings worker requests, Plugins page with
   Discover / Installed / detail / Marketplaces, built-in plugins as toggles, the `harness-skills`
   MCP server and composer command expansion (rule 3a). Default marketplace:
   `anthropics/claude-plugins-official` (317 plugins on 2026-11-25; entry `source` is a relative
   path, `{source: "url", url, sha}` or `git-subdir`, all three must be supported). Connectors
   install in phase 1 but show "sign-in comes later" until phase 2.
2. **Connectors.** Harness-side MCP OAuth, credential store, featured connectors, sign-in state in
   the Installed view.
3. **Plugin UI and upkeep.** MCP Apps in the WebView helper (§9.3), update notices, project-scope
   installs.

## 7. Out of scope

Running plugin hooks, a Harness-published marketplace, paid plugins, plugins that change the Harness
UI itself.
