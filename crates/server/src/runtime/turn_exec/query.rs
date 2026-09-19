use std::sync::Arc;

use devo_core::tools::handlers::ensure_kernel;
use devo_core::tools::{
    AgentToolCoordinator, ClientFilesystem, ToolAgentScope, ToolCall, ToolExecutionOptions,
    ToolPlanConfig, ToolRuntime, ToolRuntimeContext,
};
use devo_core::{ContentBlock, Message, QueryEvent, QueryOptions, Role, TurnConfig, query};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::super::*;
use super::event_stream::enqueue_query_event;
use super::tool_display::without_agent_coordination_tools;
use super::types::TurnQueryOutcome;

pub(crate) struct TurnModelQueryParams<'a> {
    pub state: &'a mut SessionActorState,
    pub turn_id: devo_protocol::native::ids::TurnId,
    pub turn_config: &'a TurnConfig,
    pub input: &'a str,
    pub input_messages: &'a [String],
    pub input_images: &'a [devo_protocol::PromptImagePart],
    pub collaboration_mode: devo_protocol::CollaborationMode,
    pub input_mode: super::super::TurnInputMode,
    pub usage_parent_session_id: Option<devo_core::SessionId>,
    pub event_tx: mpsc::Sender<QueryEvent>,
}

impl ServerRuntime {
    pub(crate) async fn run_turn_model_query(
        self: &Arc<Self>,
        params: TurnModelQueryParams<'_>,
    ) -> TurnQueryOutcome {
        let TurnModelQueryParams {
            state,
            turn_id,
            turn_config,
            input,
            input_messages,
            input_images,
            collaboration_mode,
            input_mode,
            usage_parent_session_id,
            event_tx,
        } = params;
        let session_id = state.session_id();
        let agent_scope = if state.summary.parent_session_id().is_some() {
            ToolAgentScope::Subagent
        } else {
            ToolAgentScope::Parent
        };
        let agent_tool_policy = state.agent_tool_policy;
        let session_tool_registry = self.tool_registry_for_actor_state(state);
        let runtime_context = Arc::clone(&state.runtime_context);
        let turn_goal = match &input_mode {
            super::super::TurnInputMode::VisibleUserMessage => {
                let stores = self.goal_stores.lock().await;
                stores
                    .get(&session_id)
                    .and_then(crate::GoalStore::get)
                    .map(crate::goal::Goal::to_thread_goal)
            }
            super::super::TurnInputMode::HiddenGoalContinuation { goal } => Some(goal.clone()),
            super::super::TurnInputMode::ApprovalResume | super::super::TurnInputMode::Recovery => {
                None
            }
        };
        state.core.config.token_budget = turn_config
            .token_budget_for_session(state.core.config.effective_context_window_override);
        state.core.collaboration_mode = collaboration_mode;
        state.summary.collaboration_mode = collaboration_mode;
        if let Some(goal) = turn_goal {
            state.core.set_active_goal(goal);
        } else if !matches!(input_mode, super::super::TurnInputMode::Recovery) {
            state.core.clear_active_goal();
        }
        if input_mode.emits_user_message() {
            push_resolved_user_input(&mut state.core, input, input_messages, input_images);
        }
        let event_callback_tx = event_tx.clone();
        let callback: devo_core::EventCallback = std::sync::Arc::new(move |event: QueryEvent| {
            let event_callback_tx = event_callback_tx.clone();
            Box::pin(async move {
                enqueue_query_event(&event_callback_tx, event).await;
            })
        });
        let tool_execution_start_tx = event_tx.clone();
        let registry = match agent_tool_policy {
            devo_protocol::AgentToolPolicy::Inherit if usage_parent_session_id.is_some() => {
                Arc::new(without_agent_coordination_tools(&session_tool_registry))
            }
            devo_protocol::AgentToolPolicy::Inherit => session_tool_registry,
            devo_protocol::AgentToolPolicy::DenyAll => {
                Arc::new(devo_core::tools::ToolRegistry::new())
            }
        };
        let permission_mode = state.core.config.permission_mode;
        let permission_profile = state.core.config.permission_profile.clone();
        let hook_context = Self::hook_context_from_actor_state(state, session_id);
        // Share the turn's live sandbox handle with tool execution so a
        // mid-turn settings override applies at the next spawn (Phase 3).
        let sandbox_profile_live = if let Some(stream) = self.active_stream_state(session_id).await
        {
            let stream = stream.lock().await;
            stream
                .turn_inline
                .as_ref()
                .map(|inline| Arc::clone(&inline.sandbox_profile_live))
        } else {
            None
        };
        // Same for the live turn-settings channel (Phase 4): seed it with the
        // turn-start config so a mid-turn model/effort overlay has a base to
        // modify; an overlay that already landed (generation > 0) wins.
        let live_turn_settings = if let Some(stream) = self.active_stream_state(session_id).await {
            let stream = stream.lock().await;
            stream
                .turn_inline
                .as_ref()
                .map(|inline| Arc::clone(&inline.live_turn_settings))
        } else {
            None
        };
        let last_model_request = if let Some(stream) = self.active_stream_state(session_id).await {
            let stream = stream.lock().await;
            stream
                .turn_inline
                .as_ref()
                .map(|inline| Arc::clone(&inline.last_model_request))
        } else {
            None
        };
        if let Some(live) = &live_turn_settings {
            let mut live = live.lock().expect("live settings mutex poisoned");
            if live.generation == 0 && live.turn_config.is_none() {
                live.turn_config = Some(turn_config.clone());
            }
            if live.python_cell_first_wait_ms.is_none() {
                live.python_cell_first_wait_ms = state.summary.settings.python_cell_first_wait_ms;
            }
        }
        let turn_cancel_token = self
            .active_turns
            .cancel_token(session_id)
            .await
            .unwrap_or_else(CancellationToken::new);
        let query_cancel_token = turn_cancel_token.clone();
        let provider_http = runtime_context
            .config_store
            .lock()
            .expect("app config store mutex should not be poisoned")
            .effective_config()
            .provider_http
            .clone();
        let output_store = state.rollout_path.as_ref().map(|path| {
            Arc::new(devo_core::tools::output_store::OutputStore::new(
                path.with_extension("outputs"),
                session_id.to_string(),
            ))
        });
        let harness_digest = state
            .rollout_path
            .as_ref()
            .and_then(|path| {
                path.parent().map(|dir| {
                    // Cold-boundary inject: after skills (prefix), before goal in query.
                    // HarnessDigestInjector fails closed on corrupt JSON (empty omit).
                    let digest = devo_harness::HarnessDigestInjector::digest_or_empty(dir);
                    if digest.is_empty() {
                        None
                    } else {
                        Some(digest)
                    }
                })
            })
            .flatten();
        if let (Some(store), Some(path)) = (&output_store, &state.rollout_path) {
            let path = path.clone();
            match tokio::task::spawn_blocking(move || {
                devo_core::output_replay::read_output_references(&path)
            })
            .await
            {
                Ok(Ok(artifacts)) => store.restore_references(artifacts),
                error => tracing::warn!(?error, "output references could not be restored"),
            }
        }
        let session_dir = state
            .rollout_path
            .as_ref()
            .and_then(|path| crate::persistence::RolloutStore::rlm_session_dir_for_rollout(path));
        // Session-scoped RLM kernel: create on the turn task (not the actor mailbox).
        // Soft-fail when the runtime is unavailable so Discrete sessions still run.
        let kernel =
            match ensure_kernel(&state.kernel, &state.core.cwd, session_dir.as_deref()).await {
                Ok(arc) => {
                    // Fresh kernel spawn: record the OS-fence outcome in the
                    // rollout (design doc §5.3) — downgraded-unfenced kernels
                    // must leave an audit trail, fenced ones keep it complete.
                    if let Some(rollout_path) = state.rollout_path.as_ref() {
                        let label = match arc.fence_state() {
                            devo_kernel::FenceState::Fenced => "fenced",
                            devo_kernel::FenceState::DowngradedUnfenced => {
                                "downgradedUnfenced"
                            }
                            devo_kernel::FenceState::NotRequested => "notRequested",
                        };
                        if let Err(err) = self.rollout_store.append_kernel_fence_state(
                            rollout_path,
                            session_id,
                            label,
                        ) {
                            tracing::warn!(
                                %err,
                                "kernel fence state could not be recorded in rollout"
                            );
                        }
                    }
                    // Explicit downgrade surfaces as a conversation-visible
                    // Warning item (design doc §5.3): persisted + broadcast to
                    // both clients; fenced/not-requested spawns stay silent.
                    // `[permission] warn_unfenced_kernel = false` ("don't
                    // remind") suppresses the visible warning — the rollout
                    // kernelFence audit event is recorded regardless.
                    let warn_unfenced = runtime_context
                        .config_store
                        .lock()
                        .expect("app config store mutex should not be poisoned")
                        .effective_config()
                        .permission
                        .warn_unfenced_kernel;
                    if warn_unfenced
                        && matches!(
                            arc.fence_state(),
                            devo_kernel::FenceState::DowngradedUnfenced
                        )
                    {
                        self.emit_turn_native_item(
                            session_id,
                            turn_id,
                            devo_protocol::native::item::Item::Warning {
                                code: "rlmKernelUnfenced".to_string(),
                                message: "RLM kernel runs UNFENCED with full user \
                                          permissions: the OS sandbox fence could not be \
                                          raised. Run the Windows sandbox setup to enable \
                                          it (design doc §5.3)."
                                    .to_string(),
                                retryable: false,
                            },
                        )
                        .await;
                    }
                    state.kernel = Some(Arc::clone(&arc));
                    Some(arc)
                }
                Err(err) => {
                    tracing::debug!(%err, "RLM kernel not available for this turn");
                    state.kernel.as_ref().map(Arc::clone)
                }
            };
        let permission =
            self.build_permission_checker(session_id, turn_id, permission_mode, permission_profile);
        if let Some(ref kernel) = kernel {
            let host_bridge = Arc::new(super::super::kernel_host_bridge::HostBridge {
                session_id,
                turn_id: Some(turn_id),
                cwd: state.core.cwd.clone(),
                session_dir: session_dir.clone(),
                collaboration_mode,
                permission: permission.clone(),
                client_filesystem: Some(Arc::clone(self) as Arc<dyn ClientFilesystem>),
                file_read_ledger: Arc::clone(&state.file_read_ledger),
                sandbox_profile: state.core.config.sandbox_profile.clone(),
                cancel_token: turn_cancel_token.clone(),
                mcp_manager: Some(Arc::clone(&runtime_context.mcp_manager)),
                agent_scope,
                local_web_search: match &turn_config.web_search {
                    devo_core::ResolvedWebSearchConfig::Local(config) => {
                        serde_json::to_value(config).ok()
                    }
                    devo_core::ResolvedWebSearchConfig::Disabled
                    | devo_core::ResolvedWebSearchConfig::Provider => None,
                },
                network_proxy: provider_http.proxy_url.clone(),
                network_no_proxy: provider_http.no_proxy.clone(),
                runtime: Arc::downgrade(self),
                model_id: Some(turn_config.model.slug.clone()),
                input_modalities: turn_config.model.input_modalities.clone(),
                context_window: Some(turn_config.model.context_window),
            });
            kernel
                .set_host_handler(Some(super::super::kernel_host::host_handler_for_bridge(
                    host_bridge,
                )))
                .await;
        }
        // When a kernel is available, model tools are the RLM root set:
        // ipython + bash, plus MCP layered from the session manager.
        let registry = if kernel.is_some() {
            let plan = ToolPlanConfig {
                execution_surface: devo_kernel::ExecutionSurface::Rlm,
                ..Default::default()
            };
            Arc::new(
                devo_core::tools::handlers::build_registry_from_plan_with_mcp(
                    &plan,
                    Arc::clone(&runtime_context.mcp_manager),
                )
                .await,
            )
        } else {
            registry
        };
        let runtime = ToolRuntime::new_with_context_and_options(
            Arc::clone(&registry),
            permission,
            ToolRuntimeContext {
                session_id,
                turn_id: Some(turn_id),
                cwd: state.core.cwd.clone(),
                agent_scope,
                collaboration_mode,
                agent_coordinator: Some(Arc::clone(self) as Arc<dyn AgentToolCoordinator>),
                client_filesystem: Some(Arc::clone(self) as Arc<dyn ClientFilesystem>),
                file_read_ledger: Arc::clone(&state.file_read_ledger),
                local_web_search: match &turn_config.web_search {
                    devo_core::ResolvedWebSearchConfig::Local(config) => Some(config.clone()),
                    devo_core::ResolvedWebSearchConfig::Disabled
                    | devo_core::ResolvedWebSearchConfig::Provider => None,
                },
                hooks: hook_context,
                network_proxy: provider_http.proxy_url,
                network_no_proxy: provider_http.no_proxy,
                sandbox_profile: state.core.config.sandbox_profile.clone(),
                sandbox_profile_live,
                kernel,
                python_cell_first_wait_ms: state.summary.settings.python_cell_first_wait_ms,
                live_turn_settings: live_turn_settings.clone(),
                python_cell_watch: Some(Arc::new(
                    crate::runtime::python_cell_watch::ServerPythonCellWatch::new(
                        Arc::clone(self),
                        session_id,
                        turn_id,
                    ),
                )),
                python_cell_completion: Some(Arc::new(
                    crate::runtime::python_cell_watch::ServerPythonCellCompletionHook::new(Arc::clone(self)),
                )),
                session_dir: session_dir.clone(),
            },
            ToolExecutionOptions {
                output_store: output_store.clone(),
                cancel_token: turn_cancel_token,
                on_tool_execution_start: Some(Arc::new(move |call: ToolCall| {
                    let tool_execution_start_tx = tool_execution_start_tx.clone();
                    Box::pin(async move {
                        enqueue_query_event(
                            &tool_execution_start_tx,
                            QueryEvent::ToolExecutionStart { id: call.id },
                        )
                        .await;
                    })
                })),
                ..ToolExecutionOptions::default()
            },
        );
        // Turn I/O runs on the spawned active-turn task (not the session actor).
        // Race the query against the turn cancel token so interrupt unblocks
        // promptly even when the join handle has not been aborted yet.
        //
        // The stream-loop's biased select! checks the cancel token before every
        // chunk.  On interrupt it breaks immediately, the post-processing stage
        // commits partial assistant/reasoning text to session, then the cancel
        // guard before tool execution skips incomplete tool calls and ends the
        // turn cleanly.  The tool-execution cancel guard already handles the
        // "cancel during tools" case.
        let result = {
            let provider = self.usage_ledger.instrumented_provider(
                runtime_context.provider_for_route(turn_config.provider_route.clone()),
                session_id,
                Some(turn_id),
                devo_protocol::native::usage::UsagePurpose::TurnQuery,
            );
            let compaction_provider = self.usage_ledger.instrumented_provider(
                runtime_context.provider_for_route(turn_config.provider_route.clone()),
                session_id,
                Some(turn_id),
                devo_protocol::native::usage::UsagePurpose::Compaction,
            );
            let mut query_future = std::pin::pin!(query(
                &mut state.core,
                turn_config,
                provider,
                registry,
                &runtime,
                Some(callback),
                QueryOptions {
                    output_store,
                    journal: state.rollout_path.as_ref().map(|path| {
                        Arc::new(super::journal::RolloutToolJournal::new(
                            Arc::clone(self),
                            path.clone(),
                            session_id,
                            turn_id,
                        ))
                            as Arc<dyn devo_core::durable_execution::ToolIntentJournal>
                    }),
                    cancel_token: Some(query_cancel_token.clone()),
                    compaction_provider: Some(compaction_provider),
                    live_settings: live_turn_settings.clone(),
                    last_model_request,
                    harness_digest,
                },
            ));
            tokio::select! {
                biased;
                () = query_cancel_token.cancelled() => {
                    // Give the query a brief window so the stream-loop cancel
                    // check and post-processing commit can complete, then map
                    // any completion (including Aborted from the tool-execution
                    // guard) to the Interrupted status.
                    match tokio::time::timeout(
                        std::time::Duration::from_millis(100),
                        &mut query_future,
                    )
                    .await
                    {
                        Ok(Ok(())) | Ok(Err(devo_core::AgentError::Aborted)) | Err(_) => {
                            Err(devo_core::AgentError::Aborted)
                        }
                        Ok(Err(error)) => Err(error),
                    }
                }
                result = &mut query_future => result,
            }
        };
        if let Err(devo_core::AgentError::Provider(error)) = &result
            && devo_provider::recovery_hint_for_anyhow(error).as_deref()
                == Some(devo_provider::AUTH_HINT)
            && let devo_provider::ProviderRoute::Connection { provider_id, .. } =
                &turn_config.provider_route
        {
            self.notify_provider_auth_stale(
                provider_id.clone(),
                Some(
                    if error.to_string().to_ascii_lowercase().contains("oauth") {
                        "OAuth credential expired or refresh failed; run /login to reconnect"
                            .to_string()
                    } else {
                        "provider rejected the configured credential".to_string()
                    },
                ),
            )
            .await;
        }
        TurnQueryOutcome {
            result,
            session_total_input_tokens: state.core.total_input_tokens,
            session_total_output_tokens: state.core.total_output_tokens,
            session_total_tokens: state.core.total_tokens,
            session_total_cache_creation_tokens: state.core.total_cache_creation_tokens,
            session_total_cache_read_tokens: state.core.total_cache_read_tokens,
            session_last_input_tokens: state.core.last_input_tokens,
            session_prompt_token_estimate: state.core.prompt_token_estimate,
        }
    }
}

fn push_resolved_user_input(
    session: &mut devo_core::SessionState,
    input: &str,
    input_messages: &[String],
    input_images: &[devo_protocol::PromptImagePart],
) {
    if input_images.is_empty() {
        if input_messages.is_empty() {
            session.push_message(Message::user(input.to_string()));
        } else {
            for input_message in input_messages {
                session.push_message(Message::user(input_message.clone()));
            }
        }
        return;
    }

    let mut content = Vec::new();
    if input_messages.is_empty() {
        if !input.trim().is_empty() {
            content.push(ContentBlock::Text {
                text: input.to_string(),
            });
        }
    } else {
        for input_message in input_messages {
            if !input_message.trim().is_empty() {
                content.push(ContentBlock::Text {
                    text: input_message.clone(),
                });
            }
        }
    }
    for image in input_images {
        content.push(ContentBlock::Image {
            mime_type: image.mime_type.clone(),
            data_base64: image.data_base64.clone(),
        });
    }
    if !content.is_empty() {
        session.push_message(Message {
            role: Role::User,
            content,
        });
    }
}
