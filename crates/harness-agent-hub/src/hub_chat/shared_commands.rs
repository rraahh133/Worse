//! Slash commands for agents without command files of their own (agy, API agents / custom providers):
//! Claude Code's user commands, where plugins install theirs, are offered to them too and expanded into the
//! prompt text before sending (`docs/plans/plugins.md` §1 rule 3a).

use harness_agent_config::{
    AgentConfigService, AgentKind, Command, Concept, ConfigTarget, EntryBody,
};
use harness_core_api::AgentId;

use super::ConfigScope;
use crate::hub::HubCtx;

/// Agents that read command files natively (Claude Code accounts, Codex) do not get the shared ones.
pub(crate) fn needs_shared(ctx: &HubCtx, agent: &AgentId) -> bool {
    !crate::accounts::is_claude(&ctx.config, agent) && !crate::codex_delegate::is_codex(ctx, agent)
}

/// Claude Code user-scope commands with their file path.
pub(crate) async fn shared_commands(ctx: &HubCtx) -> Vec<(Command, std::path::PathBuf)> {
    let Some(paths) = &ctx.config.config_paths else {
        return Vec::new();
    };
    let service = AgentConfigService::new(paths.clone(), ctx.config.auth_probe.0.clone());
    let target = ConfigTarget {
        agent: AgentKind::ClaudeCode,
        scope: ConfigScope::User,
    };
    service
        .list(Concept::Command, target)
        .await
        .unwrap_or_default()
        .into_iter()
        .filter_map(|entry| match entry.body {
            EntryBody::Command(c) => Some((c, entry.source_path)),
            _ => None,
        })
        .collect()
}

/// The command body with `$ARGUMENTS` and `$1`…`$9` filled from `input` (Claude Code's placeholders).
pub(crate) fn command_prompt(command: &Command, input: &str) -> String {
    let input = input.trim();
    let words: Vec<&str> = input.split_whitespace().collect();
    let mut body = command.body.trim().replace("$ARGUMENTS", input);
    for i in (1..=9).rev() {
        let word = words.get(i - 1).copied().unwrap_or_default();
        body = body.replace(&format!("${i}"), word);
    }
    if !command.body.contains('$') && !input.is_empty() {
        body.push_str("\n\n");
        body.push_str(input);
    }
    body
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled_and_plain_bodies_get_the_input_appended() {
        let mut c = Command {
            name: "ship".into(),
            body: "Ship $1 to $2 ($ARGUMENTS)".into(),
            ..Command::default()
        };
        assert_eq!(
            command_prompt(&c, "app prod"),
            "Ship app to prod (app prod)"
        );
        c.body = "Review the diff.".into();
        assert_eq!(
            command_prompt(&c, "focus on tests"),
            "Review the diff.\n\nfocus on tests"
        );
        assert_eq!(command_prompt(&c, ""), "Review the diff.");
    }
}
