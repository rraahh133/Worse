use super::{
    harness_skills, score, store, store_chat, ActionEntry, AgentConfigService, ApiResult,
    CommandList, CommandSearch, CommandSearchResult, CommandSource, Concept, ConfigScope,
    ConfigTarget, EntryBody, Hub, ScoredId, SearchScope, SessionHit, SessionId, SlashCommand,
};

impl Hub {
    pub(crate) async fn do_list_commands(&self, session: SessionId) -> ApiResult<CommandList> {
        self.check_open()?;
        let row = self.load_row(&session).await?;
        let agent = row.active_agent.clone();
        let advertised = self.ctx.chat.agent_commands(&self.ctx, &agent).await;
        let mut merged: Vec<SlashCommand> = Vec::new();
        if let (Some(kind), Some(paths)) = (self.agent_kind(&agent), &self.ctx.config.config_paths)
        {
            let mut paths = paths.clone();
            paths.workspace = Some(std::path::PathBuf::from(&row.workspace_path));
            let service = AgentConfigService::new(paths, self.ctx.config.auth_probe.0.clone());
            for (scope, source) in [
                (ConfigScope::Workspace, CommandSource::Workspace),
                (ConfigScope::User, CommandSource::User),
            ] {
                // 不支持的作用域（如 Codex 工作区）或读失败只是没有该来源的命令，不影响列表。
                let Ok(entries) = service
                    .list(Concept::Command, ConfigTarget { agent: kind, scope })
                    .await
                else {
                    continue;
                };
                for entry in entries {
                    let EntryBody::Command(command) = entry.body else {
                        continue;
                    };
                    if merged.iter().any(|c| c.name == command.name) {
                        continue;
                    }
                    merged.push(SlashCommand {
                        name: command.name,
                        description: command.description.unwrap_or_default(),
                        input_hint: command.argument_hint,
                        source,
                        path: Some(entry.source_path.to_string_lossy().into_owned()),
                    });
                }
            }
        }
        if let Some(agent_commands) = &advertised {
            // 同名时 agent 广播的命令优先（spec §9.2.1）。
            merged.retain(|c| !agent_commands.iter().any(|a| a.name == c.name));
            merged.extend(agent_commands.iter().cloned());
        }
        if super::shared_commands::needs_shared(&self.ctx, &agent) {
            for (command, path) in super::shared_commands::shared_commands(&self.ctx).await {
                if merged.iter().any(|c| c.name == command.name) {
                    continue;
                }
                merged.push(SlashCommand {
                    name: command.name,
                    description: command.description.unwrap_or_default(),
                    input_hint: command.argument_hint,
                    source: CommandSource::User,
                    path: Some(path.to_string_lossy().into_owned()),
                });
            }
        }
        let skills = harness_skills(&self.ctx).await;
        merged.retain(|c| !skills.iter().any(|s| s.name == c.name));
        merged.extend(skills.into_iter().map(|s| SlashCommand {
            name: s.name,
            description: s.description,
            input_hint: None,
            source: CommandSource::Harness,
            path: Some(s.path.to_string_lossy().into_owned()),
        }));
        merged.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(CommandList {
            agent,
            known: advertised.is_some(),
            commands: merged,
        })
    }

    // ---- command center ------------------------------------------------------------------------------

    pub(crate) async fn do_command_search(
        &self,
        request: CommandSearch,
    ) -> ApiResult<CommandSearchResult> {
        self.check_open()?;
        let limit = match request.limit {
            0 => 10,
            n => n.min(50),
        };
        let wants = |s: SearchScope| request.scopes.is_empty() || request.scopes.contains(&s);
        let query = request.query.trim().to_owned();
        let mut result = CommandSearchResult::default();
        if wants(SearchScope::Actions) {
            let mut ranked: Vec<ScoredId> = request
                .actions
                .iter()
                .filter_map(|a: &ActionEntry| {
                    let best = std::iter::once(score(&query, &a.title))
                        .chain(
                            a.keywords
                                .iter()
                                .map(|k| score(&query, k).map(|s| s.saturating_sub(50))),
                        )
                        .flatten()
                        .max()?;
                    Some(ScoredId {
                        id: a.id.clone(),
                        score: best,
                    })
                })
                .collect();
            ranked.sort_by_key(|r| std::cmp::Reverse(r.score));
            ranked.truncate(limit as usize);
            result.actions = ranked;
        }
        if wants(SearchScope::Sessions) {
            result.sessions = self.search_sessions(&query, limit).await?;
        }
        if wants(SearchScope::Files) {
            if let Some(workspace) = &request.workspace_path {
                if let Some(files) = self.search_files(workspace, &query, limit).await? {
                    result.files = files;
                    result.files_available = true;
                }
            } else {
                result.files_available = self.ctx.config.file_search.is_some();
            }
        }
        Ok(result)
    }

    pub(super) async fn search_sessions(
        &self,
        query: &str,
        limit: u32,
    ) -> ApiResult<Vec<SessionHit>> {
        let all = self
            .ctx
            .store
            .run(|c| store::list_sessions(c, None, None, 2000))
            .await?;
        if query.is_empty() {
            return Ok(all
                .into_iter()
                .take(limit as usize)
                .map(|summary| SessionHit {
                    summary,
                    snippet: String::new(),
                    score: 0,
                })
                .collect());
        }
        let mut hits: Vec<SessionHit> = Vec::new();
        for summary in &all {
            if let Some(n) = score(query, &summary.title).filter(|n| *n >= 400) {
                hits.push(SessionHit {
                    summary: summary.clone(),
                    snippet: String::new(),
                    score: n,
                });
            }
        }
        let pattern = query.to_owned();
        let transcript = self
            .ctx
            .store
            .run(move |c| store_chat::transcript_matches(c, &pattern, 200))
            .await?;
        for (id, snippet) in transcript {
            if hits.iter().any(|h| h.summary.id == id) {
                continue;
            }
            if let Some(summary) = all.iter().find(|s| s.id == id) {
                hits.push(SessionHit {
                    summary: summary.clone(),
                    snippet,
                    score: 100,
                });
            }
        }
        hits.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then(b.summary.updated_at.cmp(&a.summary.updated_at))
        });
        hits.truncate(limit as usize);
        Ok(hits)
    }
}
