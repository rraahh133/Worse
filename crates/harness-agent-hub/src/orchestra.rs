//! Orchestra graph (phase-9-parity.md §9.11). The store setting `orchestra` holds the JSON; written only here.

use harness_core_api::{AgentId, ApiError, ApiResult, Orchestra};

use crate::hub::{Hub, HubCtx};
use crate::store;

const KEY: &str = "orchestra";

impl Hub {
    pub(crate) async fn do_get_orchestra(&self) -> ApiResult<Orchestra> {
        self.check_open()?;
        Ok(self.load_orchestra().await)
    }

    pub(crate) async fn do_set_orchestra(&self, orchestra: Orchestra) -> ApiResult<()> {
        self.check_open()?;
        orchestra.validate().map_err(ApiError::InvalidArgument)?;
        let json =
            serde_json::to_string(&orchestra).map_err(|e| ApiError::Internal(e.to_string()))?;
        self.ctx
            .store
            .run(move |c| store::set_setting(c, KEY, &json))
            .await?;
        Ok(())
    }

    /// The stored graph; empty when unset or unreadable.
    pub(crate) async fn load_orchestra(&self) -> Orchestra {
        self.ctx
            .store
            .run(|c| store::get_setting(c, KEY))
            .await
            .ok()
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
            .unwrap_or_default()
    }
}

/// Whether a top-level session on `agent` gets the delegate tools (§9.11): always while Orchestra has no
/// orchestrator, else only when the agent is the orchestrator or connected to its team on the canvas.
// 修复：之前只要是顶层会话就能委派，Orchestra 里没有连线的 agent（例如 agy）也会随意启动子代理。
pub(crate) async fn may_delegate(ctx: &HubCtx, agent: &AgentId) -> bool {
    let orchestra = ctx
        .store
        .run(|c| store::get_setting(c, KEY))
        .await
        .ok()
        .flatten()
        .and_then(|json| serde_json::from_str::<Orchestra>(&json).ok())
        .unwrap_or_default();
    allows(&orchestra, |node| same_family(ctx, node, agent))
}

fn allows(orchestra: &Orchestra, mine: impl Fn(&AgentId) -> bool) -> bool {
    let Some(root) = orchestra.orchestrator() else {
        return true;
    };
    mine(&root.agent) || orchestra.team().iter().any(|node| mine(&node.agent))
}

/// Accounts of one CLI count as the same agent (a Claude or agy account after quota failover).
fn same_family(ctx: &HubCtx, a: &AgentId, b: &AgentId) -> bool {
    let config = &ctx.config;
    a == b
        || (crate::accounts::is_claude(config, a) && crate::accounts::is_claude(config, b))
        || (crate::agy_ops::is_agy(ctx, a) && crate::agy_ops::is_agy(ctx, b))
        || (crate::codex_delegate::is_codex(ctx, a) && crate::codex_delegate::is_codex(ctx, b))
}

#[cfg(test)]
mod tests {
    use harness_core_api::{AgentId, Orchestra, OrchestraEdge, OrchestraNode, OrchestraRole};

    fn node(id: &str, role: OrchestraRole) -> OrchestraNode {
        OrchestraNode {
            id: id.into(),
            agent: AgentId::new(id),
            role,
            x: 0.0,
            y: 0.0,
            instructions: String::new(),
        }
    }

    #[test]
    fn only_connected_agents_may_delegate() {
        let is = |name: &'static str| move |a: &AgentId| a.as_str() == name;
        assert!(super::allows(&Orchestra::default(), is("agy")));
        let graph = Orchestra {
            nodes: vec![
                node("claude-code", OrchestraRole::Orchestrator),
                node("codex", OrchestraRole::Subagent),
                node("agy", OrchestraRole::Subagent),
            ],
            edges: vec![OrchestraEdge {
                from: "claude-code".into(),
                to: "codex".into(),
            }],
        };
        assert!(super::allows(&graph, is("claude-code")));
        assert!(super::allows(&graph, is("codex")));
        assert!(!super::allows(&graph, is("agy")));
        assert!(!super::allows(&graph, is("deepseek")));
    }
}
