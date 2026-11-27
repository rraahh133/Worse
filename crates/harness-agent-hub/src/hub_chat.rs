//! Hub side of API 0.5.0 (phase-9-parity.md §9.2.1): session organisation, groups, fork, turn actions, file
//! undo, find, context items and mentions, slash commands and the command-center search.

use std::collections::HashMap;
use std::sync::Mutex;

use harness_agent_config::{
    AgentConfigService, Concept, ConfigTarget, EntryBody, Scope as ConfigScope,
};
use harness_core_api::{
    ActionEntry, AgentId, ApiError, ApiResult, ApplyFileChanges, ChangeDirection, CommandList,
    CommandSearch, CommandSearchResult, CommandSource, ContextItem, EditResend, EventKind,
    FileChangePlan, FileRestoreReport, FindField, FindHit, FindInSession, ForkOrigin, ForkSession,
    GroupColor, MentionCandidate, MentionKind, MentionQuery, ResolvedItem, Retry, Rewind,
    RewindResult, ScoredId, SearchScope, SendAccepted, SessionGroup, SessionHit, SessionId,
    SessionSummary, SlashCommand, TurnId, UpdateGroup,
};

use harness_core_api::workspace::FileHit;

use crate::context::{self, Caps};
use crate::hub::{Hub, HubCtx};
use crate::session::{Resend, RewindCmd, SessionCmd};
use crate::store::{self, SessionRow};
use crate::store_chat;
use crate::util::{new_id, now_ms};

mod commands;
mod find;
mod organize;
pub(crate) mod shared_commands;
mod turn_actions;

const COMMANDS_PREFIX: &str = "agent_commands:";

pub(crate) struct HarnessSkill {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) body: String,
    pub(crate) path: std::path::PathBuf,
}

pub(crate) async fn harness_skills(ctx: &HubCtx) -> Vec<HarnessSkill> {
    let Some(paths) = &ctx.config.config_paths else {
        return Vec::new();
    };
    let service = AgentConfigService::new(paths.clone(), ctx.config.auth_probe.0.clone());
    let target = ConfigTarget {
        agent: harness_agent_config::AgentKind::Harness,
        scope: ConfigScope::User,
    };
    let Ok(entries) = service.list(Concept::Skill, target).await else {
        return Vec::new();
    };
    entries
        .into_iter()
        .filter_map(|entry| match entry.body {
            EntryBody::Skill(skill) => Some(HarnessSkill {
                name: skill.name,
                description: skill.description.unwrap_or_default(),
                body: skill.body,
                path: entry.source_path,
            }),
            _ => None,
        })
        .collect()
}

pub(crate) fn skill_prompt(skill: &HarnessSkill, input: &str) -> String {
    let dir = skill
        .path
        .parent()
        .map_or_else(String::new, |d| d.display().to_string());
    let input = match input.trim() {
        "" => "(no extra input)",
        text => text,
    };
    format!(
        "<harness-skill name=\"{name}\" dir=\"{dir}\">\n{body}\n</harness-skill>\nThe skill's other files are in {dir}; read them when the skill refers to them.\n\nFollow the Harness skill \"{name}\" above for this request: {input}",
        name = skill.name,
        body = skill.body.trim(),
    )
}

pub(crate) fn skill_call(text: &str) -> Option<(&str, &str)> {
    let rest = text.trim_start().strip_prefix('/')?;
    let end = rest.find(char::is_whitespace).unwrap_or(rest.len());
    let (name, input) = rest.split_at(end);
    (!name.is_empty()).then_some((name, input))
}

/// Lazy chat caches shared by the hub and its actors.
#[derive(Default)]
pub(crate) struct ChatState {
    /// `promptCapabilities` per agent, learned when a native session is ready.
    prompt_caps: Mutex<HashMap<AgentId, Caps>>,
    /// Last `available_commands_update` per agent (cache of store keys `agent_commands:<id>`).
    commands: Mutex<HashMap<AgentId, Option<Vec<SlashCommand>>>>,
}

impl ChatState {
    pub(crate) fn set_caps(&self, agent: &AgentId, caps: Caps) {
        if let Ok(mut map) = self.prompt_caps.lock() {
            map.insert(agent.clone(), caps);
        }
    }

    pub(crate) fn caps(&self, agent: &AgentId) -> Caps {
        self.prompt_caps
            .lock()
            .ok()
            .and_then(|m| m.get(agent).copied())
            .unwrap_or(Caps::UNKNOWN)
    }

    /// The agent's advertised commands; `None` when it never advertised any.
    pub(crate) async fn agent_commands(
        &self,
        ctx: &HubCtx,
        agent: &AgentId,
    ) -> Option<Vec<SlashCommand>> {
        if let Some(cached) = self
            .commands
            .lock()
            .ok()
            .and_then(|m| m.get(agent).cloned())
        {
            return cached;
        }
        let key = format!("{COMMANDS_PREFIX}{agent}");
        let stored = ctx
            .store
            .run(move |c| store::get_setting(c, &key))
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Vec<SlashCommand>>(&json).ok());
        if let Ok(mut map) = self.commands.lock() {
            map.insert(agent.clone(), stored.clone());
        }
        stored
    }

    /// Single write path of the command cache: memory, store, global `CommandsChanged`.
    pub(crate) async fn record_commands(
        &self,
        ctx: &HubCtx,
        agent: &AgentId,
        commands: Vec<SlashCommand>,
    ) {
        if self.agent_commands(ctx, agent).await.as_ref() == Some(&commands) {
            return;
        }
        if let Ok(mut map) = self.commands.lock() {
            map.insert(agent.clone(), Some(commands.clone()));
        }
        let key = format!("{COMMANDS_PREFIX}{agent}");
        match serde_json::to_string(&commands) {
            Ok(json) => {
                if let Err(error) = ctx
                    .store
                    .run(move |c| store::set_setting(c, &key, &json))
                    .await
                {
                    tracing::warn!(%error, "saving agent commands failed");
                }
            }
            Err(error) => tracing::warn!(%error, "encoding agent commands failed"),
        }
        ctx.events.emit(
            None,
            EventKind::CommandsChanged {
                agent: agent.clone(),
            },
        );
    }
}

/// Case-insensitive rank: exact > prefix > word prefix > substring > subsequence.
pub(crate) fn score(query: &str, text: &str) -> Option<u32> {
    let query = query.trim().to_lowercase();
    let text = text.to_lowercase();
    if query.is_empty() {
        return Some(0);
    }
    if text == query {
        return Some(1000);
    }
    if text.starts_with(&query) {
        return Some(800);
    }
    if text
        .split(|c: char| !c.is_alphanumeric())
        .any(|w| w.starts_with(&query))
    {
        return Some(600);
    }
    if text.contains(&query) {
        return Some(400);
    }
    let mut chars = text.chars();
    query.chars().all(|q| chars.any(|t| t == q)).then_some(200)
}

/// Lower-cases char by char, keeping one char per char (offsets stay valid).
fn fold(text: &str, case_sensitive: bool) -> Vec<char> {
    text.chars()
        .map(|c| {
            if case_sensitive {
                c
            } else {
                c.to_lowercase().next().unwrap_or(c)
            }
        })
        .collect()
}

fn find_all(text: &str, query: &[char], case_sensitive: bool) -> Vec<(usize, String)> {
    let hay = fold(text, case_sensitive);
    let original: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    if query.is_empty() || hay.len() < query.len() {
        return out;
    }
    let mut start = 0;
    while start + query.len() <= hay.len() {
        if hay[start..start + query.len()] == *query {
            let from = start.saturating_sub(15);
            let to = (start + query.len() + 25).min(original.len());
            out.push((start, original[from..to].iter().collect()));
            start += query.len();
        } else {
            start += 1;
        }
    }
    out
}

fn as_u32(n: usize) -> u32 {
    u32::try_from(n).unwrap_or(u32::MAX)
}

impl Hub {
    pub(crate) async fn load_row(&self, id: &SessionId) -> ApiResult<SessionRow> {
        let lookup = id.clone();
        self.ctx
            .store
            .run(move |c| store::load_session(c, &lookup))
            .await?
            .ok_or_else(|| ApiError::NotFound(format!("session {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scorer_orders_match_kinds() {
        assert_eq!(score("open", "open"), Some(1000));
        assert_eq!(score("op", "Open file"), Some(800));
        assert_eq!(score("fi", "Open file"), Some(600));
        assert_eq!(score("pen", "Open file"), Some(400));
        assert_eq!(score("ofl", "Open file"), Some(200));
        assert_eq!(score("xyz", "Open file"), None);
    }

    #[test]
    fn find_reports_char_offsets() {
        let hits = find_all("Ärger und ärger", &fold("ärger", false), false);
        assert_eq!(hits.iter().map(|h| h.0).collect::<Vec<_>>(), vec![0, 10]);
    }
}
