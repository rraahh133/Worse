use super::{
    market_supports, message, plan_paths, skill_catalog, AgentConfigService, Concept, ConfigError,
    ConfigTarget, DocKind, EntryBody, PathBuf, Pending, PlanOp, Scope, SettingsCall, Worker,
    WritePlan, MARKET_AGENTS,
};

impl Worker {
    pub(super) async fn plan(
        &self,
        svc: &AgentConfigService,
        op: PlanOp,
    ) -> Result<(Pending, String, String, bool), String> {
        let plan = match op {
            PlanOp::Upsert {
                target,
                body,
                previous,
            } => svc.plan_upsert(target, &body, previous.as_deref()).await,
            PlanOp::Delete {
                concept,
                target,
                name,
            } => svc.plan_delete(concept, target, &name).await,
            PlanOp::Toggle {
                concept,
                target,
                name,
                on,
            } => svc.plan_set_enabled(concept, target, &name, on).await,
            PlanOp::Copy {
                concept,
                from,
                to,
                name,
            } => svc.plan_copy(concept, from, to, &name).await,
            PlanOp::Raw { path, text } => svc.plan_raw(&path, &text).await,
            PlanOp::Instructions { target, path, text } => {
                svc.plan_instructions(target, &path, &text).await
            }
            PlanOp::Memory { scope, text } => {
                let scope = self.memory_scope(scope);
                let plan = self
                    .call(|reply| SettingsCall::MemoryPlan(scope, text, reply))
                    .await?;
                let (diff, shown, no_op) = (plan.diff.clone(), plan.path.clone(), plan.no_op);
                return Ok((Pending::Memory(plan), diff, shown, no_op));
            }
            PlanOp::ImportMcp(_) => {
                return Err("import is planned separately".to_owned());
            }
            op @ (PlanOp::InstallPlugin { .. } | PlanOp::RemovePlugin { .. }) => {
                return super::plugins::plan(svc, &self.data_dir, op).await;
            }
            PlanOp::InstallCatalog { name, skill, extra } => {
                let mut merged = WritePlan::default();
                let mut planned = 0_u32;
                for agent in MARKET_AGENTS {
                    if !market_supports(svc, agent) {
                        continue;
                    }
                    let target = ConfigTarget::new(agent, Scope::User);
                    let body = EntryBody::Skill((*skill).clone());
                    let plan = svc
                        .plan_upsert(target, &body, None)
                        .await
                        .map_err(|e| message(&e))?;
                    let plan = skill_catalog::with_extra_files(plan, &extra)
                        .await
                        .map_err(|e| message(&e))?;
                    planned += 1;
                    merged.changes.extend(plan.changes);
                }
                if planned == 0 {
                    return Err(format!("no agent accepts skills ({name})"));
                }
                let path = plan_paths(&merged);
                let diff = merged.diff();
                let no_op = merged.is_no_op();
                return Ok((Pending::Plan(merged), diff, path, no_op));
            }
            PlanOp::RemoveCatalog { name } => {
                let mut merged = WritePlan::default();
                for agent in MARKET_AGENTS {
                    if !market_supports(svc, agent) {
                        continue;
                    }
                    let target = ConfigTarget::new(agent, Scope::User);
                    match svc.plan_delete(Concept::Skill, target, &name).await {
                        Ok(plan) => merged.changes.extend(plan.changes),
                        Err(ConfigError::NotFound(_)) => {}
                        Err(error) => return Err(message(&error)),
                    }
                }
                if merged.changes.is_empty() {
                    return Err(format!("{name} is not installed"));
                }
                let path = plan_paths(&merged);
                let diff = merged.diff();
                let no_op = merged.is_no_op();
                return Ok((Pending::Plan(merged), diff, path, no_op));
            }
        }
        .map_err(|e| message(&e))?;
        let path = plan_paths(&plan);
        let diff = plan.diff();
        let no_op = plan.is_no_op();
        Ok((Pending::Plan(plan), diff, path, no_op))
    }

    pub(super) async fn load_doc(
        &self,
        svc: &AgentConfigService,
        kind: DocKind,
        scope: Scope,
    ) -> Result<super::super::Doc, String> {
        match kind {
            DocKind::Memory => {
                let scope = self.memory_scope(scope);
                let doc = self
                    .call(|reply| SettingsCall::MemoryDoc(scope, reply))
                    .await?;
                Ok(super::super::Doc {
                    path: PathBuf::from(doc.path),
                    exists: doc.exists,
                    text: doc.content,
                    supported: true,
                    reason: String::new(),
                })
            }
            DocKind::Instructions(agent) => {
                let target = ConfigTarget::new(agent, scope);
                let support = svc
                    .capabilities(agent)
                    .into_iter()
                    .find(|c| c.concept == Concept::Instructions);
                let scope_ok = support.as_ref().is_some_and(|c| match scope {
                    Scope::User => c.user,
                    Scope::Workspace => c.workspace,
                });
                if !scope_ok {
                    return Ok(super::super::Doc {
                        path: PathBuf::new(),
                        exists: false,
                        text: String::new(),
                        supported: false,
                        reason: support.and_then(|c| c.reason).unwrap_or_default(),
                    });
                }
                let entries = svc
                    .list(Concept::Instructions, target)
                    .await
                    .map_err(|e| message(&e))?;
                // 优先已存在的候选文件，否则取第一个候选（新建）。
                let files: Vec<_> = entries
                    .into_iter()
                    .filter_map(|e| match e.body {
                        EntryBody::Instructions { file, .. } => Some(file),
                        _ => None,
                    })
                    .collect();
                let Some(file) = files
                    .iter()
                    .find(|f| f.exists)
                    .or_else(|| files.first())
                    .cloned()
                else {
                    return Ok(super::super::Doc {
                        path: PathBuf::new(),
                        exists: false,
                        text: String::new(),
                        supported: false,
                        reason: String::new(),
                    });
                };
                let text = svc
                    .read_instructions(target, &file.path)
                    .await
                    .map_err(|e| message(&e))?
                    .unwrap_or_default();
                Ok(super::super::Doc {
                    path: file.path,
                    exists: file.exists,
                    text,
                    supported: true,
                    reason: String::new(),
                })
            }
        }
    }
}
