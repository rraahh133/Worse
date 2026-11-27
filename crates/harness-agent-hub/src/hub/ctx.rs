use super::{
    json, now_secs, store, Advertised, AgentErrorInfo, AgentId, ApiError, ApiResult, ErrorClass,
    EventKind, HubCtx, QuotaState, SessionId, Value, CACHE_PREFIX,
};

impl HubCtx {
    /// The agent a new session asked for `agent` gets. A member of a key pool (several API keys of one
    /// service, see `providers.rs`) is replaced by the pool's next member that is installed and not at its
    /// usage limit, so sessions take the keys in turn. Anything else, or a pool with nobody usable, stays.
    pub(crate) fn rotated(&self, agent: AgentId) -> AgentId {
        if !self.config.rotate_keys {
            return agent;
        }
        let Some((index, pool)) = crate::providers::pool_of(&self.config.key_pools, &agent) else {
            return agent;
        };
        let Ok(mut next) = self.rotation.lock() else {
            return agent;
        };
        let start = next.get(&index).copied().unwrap_or(0);
        let picked = (0..pool.len())
            .map(|offset| (start + offset) % pool.len())
            .find(|i| self.is_installed(&pool[*i]) && self.quota.is_usable(&pool[*i]));
        match picked {
            Some(i) => {
                next.insert(index, (i + 1) % pool.len());
                pool[i].clone()
            }
            None => agent,
        }
    }

    pub(crate) fn display_name(&self, agent: &AgentId) -> String {
        self.specs
            .get(agent)
            .map_or_else(|| agent.to_string(), |s| s.display_name.clone())
    }

    pub(crate) fn is_installed(&self, agent: &AgentId) -> bool {
        self.specs
            .get(agent)
            .is_some_and(|s| s.probe().is_available())
    }

    pub(crate) fn require_available(&self, agent: &AgentId) -> ApiResult<()> {
        let spec = self
            .specs
            .get(agent)
            .ok_or_else(|| ApiError::NotFound(format!("agent {agent}")))?;
        match spec.probe() {
            harness_core_api::Availability::Available { .. } => Ok(()),
            harness_core_api::Availability::NotInstalled { hint }
            | harness_core_api::Availability::AdapterMissing { hint, .. } => Err(
                ApiError::AgentUnavailable(format!("{}: {hint}", spec.display_name)),
            ),
        }
    }

    pub(crate) fn fallback_order(&self) -> Vec<AgentId> {
        self.fallback.lock().map(|o| o.clone()).unwrap_or_default()
    }

    pub(crate) fn classify(&self, agent: &AgentId, error: &AgentErrorInfo) -> ErrorClass {
        self.specs
            .get(agent)
            .map_or(ErrorClass::Fatal, |s| s.classify_error(error))
    }

    /// Single write path of quota state: registry, store, event.
    pub(crate) async fn set_quota(&self, agent: &AgentId, quota: QuotaState) {
        self.quota.set(agent, quota.clone());
        let id = agent.clone();
        let stored = quota.clone();
        if let Err(error) = self
            .store
            .run(move |c| store::save_quota(c, &id, &stored, now_secs()))
            .await
        {
            tracing::warn!(%error, "saving quota state failed");
        }
        self.events.emit(
            None,
            EventKind::AgentQuota {
                agent: agent.clone(),
                quota,
            },
        );
    }

    /// What `agent` advertised last (store cache on first use).
    pub(crate) async fn advertised(&self, agent: &AgentId) -> Advertised {
        if let Some(cached) = self
            .advertised
            .lock()
            .ok()
            .and_then(|m| m.get(agent).cloned())
        {
            return cached;
        }
        let key = format!("{CACHE_PREFIX}{agent}");
        let stored = self
            .store
            .run(move |c| store::get_setting(c, &key))
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str::<Advertised>(&json).ok())
            .unwrap_or_default();
        if let Ok(mut map) = self.advertised.lock() {
            return map.entry(agent.clone()).or_insert(stored).clone();
        }
        stored
    }

    /// Fills an empty advertisement cache by asking the agent once (spec phase-9-parity.md §9.1 `agent_options`).
    ///
    /// 修复：Settings › Agents 的默认 mode/model/effort 下拉框完全为空。原因是这些列表只来自 agent 在
    /// `session/new` 中的广播，而从未在 Harness 里聊过天的 agent（例如刚装好适配器的 Claude Code）没有缓存。
    /// 这里在缓存为空时发一次不带 prompt 的 `session/new`（cwd 为 `<HARNESS_HOME>/probe`），结果仍走唯一写入路径
    /// `record_advertised`。
    pub(crate) async fn probe_advertised(&self, agent: &AgentId) {
        let empty = |a: &Advertised| a.modes.is_none() && a.models.is_none() && a.config.is_none();
        if !empty(&self.advertised(agent).await) {
            return;
        }
        let Some(spec) = self.specs.get(agent) else {
            return;
        };
        if !matches!(
            spec.probe(),
            harness_core_api::Availability::Available { .. }
        ) {
            return;
        }
        let _probing = self.probe_lock.lock().await;
        // 等锁期间可能已有会话或另一次探测写入了缓存。
        if !empty(&self.advertised(agent).await) {
            return;
        }
        let cwd = self.config.home.join("probe");
        if let Err(error) = tokio::fs::create_dir_all(&cwd).await {
            tracing::warn!(%error, %agent, "agent options probe: no probe folder");
            return;
        }
        let connection = match self.pool.acquire(agent, false).await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(%error, %agent, "agent options probe: agent did not start");
                return;
            }
        };
        let result = connection
            .call(
                "session/new",
                json!({"cwd": cwd.to_string_lossy(), "mcpServers": []}),
            )
            .await;
        self.pool.release(agent, connection.generation()).await;
        match result {
            Ok(result) => {
                self.record_advertised(agent, |a| a.merge_response(&result))
                    .await;
            }
            Err(error) => {
                tracing::warn!(error = %error.info().message, %agent, "agent options probe failed");
            }
        }
    }

    /// Merges a new advertisement of `agent` (single write path of the cache: memory, store). Returns true
    /// when it changed.
    pub(crate) async fn record_advertised(
        &self,
        agent: &AgentId,
        merge: impl FnOnce(&mut Advertised) -> bool,
    ) -> bool {
        let mut current = self.advertised(agent).await;
        if !merge(&mut current) {
            return false;
        }
        if let Ok(mut map) = self.advertised.lock() {
            map.insert(agent.clone(), current.clone());
        }
        let key = format!("{CACHE_PREFIX}{agent}");
        match serde_json::to_string(&current) {
            Ok(json) => {
                if let Err(error) = self
                    .store
                    .run(move |c| store::set_setting(c, &key, &json))
                    .await
                {
                    tracing::warn!(%error, "saving agent options failed");
                }
            }
            Err(error) => tracing::warn!(%error, "encoding agent options failed"),
        }
        true
    }

    /// `mcpServers` of `session/new` / `session/load`: `harness-memory` first, then the enabled entries of the
    /// Harness overlay `<home>/mcp.json` for `agent` (read fresh each time; spec phase-9-parity.md §9.1.4).
    pub(crate) async fn mcp_servers(
        &self,
        agent: &AgentId,
        cwd: &str,
        caps: (bool, bool),
        session: &SessionId,
    ) -> Value {
        let mut servers = vec![self.memory_mcp_server(cwd, session, &self.display_name(agent))];
        let top_level = !self.subagents.is_child(session) && !self.routers.is_router(session);
        if !top_level || crate::orchestra::may_delegate(self, agent).await {
            if let Some(delegate) =
                self.delegate_mcp_server(session, crate::agy_ops::is_agy(self, agent))
            {
                servers.push(delegate);
            }
        }
        let off = harness_agent_config::overlay::disabled_builtins(&self.config.home).await;
        let on = |name: &str| !off.iter().any(|n| n == name);
        if matches!(
            self.config.helpers,
            crate::helpers::HelperLaunch::SelfExe { .. }
        ) {
            let helper = crate::helpers::Helper::Computer;
            // 插件页可关闭内置的 Office/电脑操作服务器：关闭后新会话不再带上它们。
            servers.extend(on(harness_agent_config::overlay::COMPUTER_SERVER).then(|| {
                json!({
                    "name": "harness-computer",
                    "command": self.config.helpers.program(helper).to_string_lossy(),
                    "args": self.config.helpers.args(helper),
                    "env": [
                        {"name": "HARNESS_HOME", "value": self.config.home.to_string_lossy()},
                        {"name": "HARNESS_MACHINE_SESSION", "value": session.as_str()}
                    ]
                })
            }));
            let work_helper = crate::helpers::Helper::Work;
            servers.extend(on(harness_agent_config::overlay::WORK_SERVER).then(|| {
                json!({
                    "name": "harness-work",
                    "command": self.config.helpers.program(work_helper).to_string_lossy(),
                    "args": self.config.helpers.args(work_helper),
                    "env": [
                        {"name": "HARNESS_HOME", "value": self.config.home.to_string_lossy()},
                        {"name": "HARNESS_WORK_PROJECT", "value": cwd}
                    ]
                })
            }));
        }
        let caps = harness_agent_config::overlay::AcpMcpCapabilities {
            http: caps.0,
            sse: caps.1,
        };
        let connector = crate::helpers::Helper::Connector;
        let connectors = harness_connectors::servers(
            &self.config.home,
            self.config.helpers.program(connector),
            &self.config.helpers.args(connector),
        )
        .await;
        match harness_agent_config::overlay::servers(&self.config.home, agent.as_str()).await {
            Ok(mut extra) => {
                // 连接器（远程 MCP）以 stdio 桥接进程提供：令牌只在桥接进程里，所有 agent 都能用。
                extra.retain(|s| !connectors.iter().any(|c| c.name == s.name));
                extra.extend(connectors);
                servers.extend(self.overlay_entries(agent, &extra, caps));
            }
            // 叠加配置损坏时不能让轮次失败：只记录告警并仅传 harness-memory（spec §9.1.4）。
            Err(error) => tracing::warn!(%error, "ignoring unreadable MCP overlay"),
        }
        // The project's Graphify graph, unless the user already configured a server of that name.
        let named = |server: &Value| {
            server.get("name").and_then(Value::as_str) == Some(crate::graphify::MCP_NAME)
        };
        if !servers.iter().any(named) && crate::recall::Store::new(&self.config.home).config().graph
        {
            if let Some(graph) = crate::graphify::mcp_server(cwd) {
                servers.push(graph);
            }
        }
        Value::Array(servers)
    }

    /// Overlay servers in ACP shape. agy and API provider agents drop or choke on hundreds of tools, so their
    /// stdio servers go behind one `harness-tools` gateway (`find_tools` / `call_tool`); others get them direct.
    // 修复：overlay 里的 dokploy 等服务器有数百个工具，agy 只加载了一部分，连 harness-delegate 的工具也被挤掉。
    fn overlay_entries(
        &self,
        agent: &AgentId,
        servers: &[harness_agent_config::McpServer],
        caps: harness_agent_config::overlay::AcpMcpCapabilities,
    ) -> Vec<Value> {
        let gated = crate::agy_ops::is_agy(self, agent)
            || self
                .specs
                .get(agent)
                .is_some_and(crate::providers::is_provider);
        let stdio = |s: &&harness_agent_config::McpServer| {
            s.transport == harness_agent_config::McpTransport::Stdio
        };
        let mut out: Vec<Value> = servers
            .iter()
            .filter(|s| !(gated && stdio(s)))
            .filter_map(|s| harness_agent_config::overlay::to_acp(s, caps))
            .collect();
        if gated && servers.iter().any(|s| stdio(&s)) {
            let helper = crate::helpers::Helper::ToolGateway;
            out.push(json!({
                "name": harness_agent_config::overlay::GATEWAY_SERVER,
                "command": self.config.helpers.program(helper).to_string_lossy(),
                "args": self.config.helpers.args(helper),
                "env": [
                    {"name": "HARNESS_HOME", "value": self.config.home.to_string_lossy()},
                    {"name": "HARNESS_GATEWAY_AGENT", "value": agent.as_str()}
                ]
            }));
        }
        out
    }

    /// ACP `McpServer` entry of `harness-delegate` (§9.8). Subagents get the `child` role, or `nested` (delegate tools
    /// plus `notify_parent`) while settings allow subagents to start subagents.
    pub(super) fn delegate_mcp_server(&self, session: &SessionId, compact: bool) -> Option<Value> {
        let endpoint = self.subagents.endpoint.get()?;
        // §9.8.1：子会话也拿到该服务器，但只是 child 角色（notify_parent / claim_paths），仍不能再委派。
        let role = if self.routers.is_router(session) {
            "router"
        } else if self.subagents.is_child(session) && self.config.delegation.nested() {
            "nested"
        } else if self.subagents.is_child(session) {
            "child"
        } else {
            "parent"
        };
        let helper = crate::helpers::Helper::Delegate;
        Some(json!({
            "name": "harness-delegate",
            "command": self.config.helpers.program(helper).to_string_lossy(),
            "args": self.config.helpers.args(helper),
            "env": [
                {"name": "HARNESS_DELEGATE_ADDR", "value": endpoint.addr},
                {"name": "HARNESS_DELEGATE_TOKEN", "value": endpoint.token},
                {"name": "HARNESS_DELEGATE_SESSION", "value": session.as_str()},
                {"name": "HARNESS_DELEGATE_ROLE", "value": role},
                {"name": "HARNESS_DELEGATE_COMPACT", "value": if compact { "1" } else { "0" }}
            ]
        }))
    }

    /// ACP `McpServer` (stdio) entry for the Harness memory server. `session` is passed as `--session` so the
    /// server can read and write the working notes of this session, and `agent` (a display name) as `--agent`
    /// so the memories it saves say who wrote them.
    pub(crate) fn memory_mcp_server(
        &self,
        workspace_path: &str,
        session: &SessionId,
        agent: &str,
    ) -> Value {
        let helper = crate::helpers::Helper::MemoryMcp;
        let mut args = self.config.helpers.args(helper);
        args.extend([
            "--project".to_owned(),
            workspace_path.to_owned(),
            "--session".to_owned(),
            session.as_str().to_owned(),
            "--agent".to_owned(),
            agent.to_owned(),
        ]);
        json!({
            "name": "harness-memory",
            "command": self.config.helpers.program(helper).to_string_lossy(),
            "args": args,
            "env": [{"name": "HARNESS_HOME", "value": self.config.home.to_string_lossy()}]
        })
    }
}
