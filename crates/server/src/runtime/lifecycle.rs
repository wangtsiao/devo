use super::*;
use std::collections::HashSet;
use std::path::Path;

use devo_protocol::native::item::{Item, UserInput, UserMessageEntry};
use devo_protocol::{PendingInputItem, PendingInputKind};

use crate::execution::RuntimeSession;
use crate::runtime::session_actor::SessionActorState;

impl ServerRuntime {
    /// Replays one rollout file and applies SQLite side effects for a single session.
    pub(crate) async fn hydrate_runtime_session(
        self: &Arc<Self>,
        session_id: SessionId,
        rollout_path: &Path,
    ) -> anyhow::Result<RuntimeSession> {
        let mut runtime_session = self
            .rollout_store
            .load_session_from_rollout(rollout_path, &self.deps)
            .await?;
        if runtime_session.summary.session_id() != session_id {
            anyhow::bail!(
                "rollout session id mismatch: expected {session_id}, got {}",
                runtime_session.summary.session_id()
            );
        }
        self.apply_persisted_session_side_effects(session_id, &mut runtime_session)
            .await?;
        Ok(runtime_session)
    }

    async fn apply_persisted_session_side_effects(
        self: &Arc<Self>,
        session_id: SessionId,
        runtime_session: &mut RuntimeSession,
    ) -> anyhow::Result<()> {
        if !runtime_session.summary.ephemeral
            && let Err(err) = self.deps.db.upsert_session(
                &runtime_session.summary,
                runtime_session.rollout_path.as_deref(),
            )
        {
            tracing::warn!(
                session_id = %session_id,
                error = %err,
                "failed to seed restored session metadata to database"
            );
        }

        match self.deps.db.get_stats(&session_id) {
            Ok(Some(stats)) => {
                runtime_session.summary.set_cumulative_usage(
                    stats.total_input_tokens,
                    stats.total_output_tokens,
                    stats.total_tokens,
                    stats.total_cache_creation_tokens,
                    stats.total_cache_read_tokens,
                );
                runtime_session.summary.prompt_token_estimate = stats.prompt_token_estimate;
                if let Ok(mut core) = runtime_session.core_session.try_lock() {
                    core.total_input_tokens = stats.total_input_tokens;
                    core.total_output_tokens = stats.total_output_tokens;
                    core.total_tokens = stats.total_tokens;
                    core.total_cache_creation_tokens = stats.total_cache_creation_tokens;
                    core.total_cache_read_tokens = stats.total_cache_read_tokens;
                    core.prompt_token_estimate = stats.prompt_token_estimate;
                    // Align auto-compact pressure with UI / occupancy tip. Never
                    // inflate `last_turn_tokens` with turn-cumulative DB values —
                    // `last_input_tokens` is latest-query input only.
                    if let Some(occupancy) = stats.last_context_occupancy.as_ref() {
                        core.last_turn_tokens = occupancy.total_tokens as usize;
                    }
                    if stats.last_input_tokens > 0 {
                        core.last_input_tokens = stats.last_input_tokens;
                    }
                }
                if let Some(occupancy) = stats.last_context_occupancy {
                    runtime_session.summary.last_query_total_tokens =
                        occupancy.total_tokens as usize;
                    runtime_session.summary.last_context_occupancy = Some(occupancy);
                }
                tracing::debug!(
                    session_id = %session_id,
                    "restored token stats from database"
                );
            }
            Ok(None) => {
                let stats = crate::db::SessionStats {
                    total_input_tokens: runtime_session.summary.total_input_tokens(),
                    total_output_tokens: runtime_session.summary.total_output_tokens(),
                    total_tokens: runtime_session.summary.total_tokens(),
                    total_cache_creation_tokens: runtime_session
                        .summary
                        .total_cache_creation_tokens(),
                    total_cache_read_tokens: runtime_session.summary.total_cache_read_tokens(),
                    last_input_tokens: 0,
                    turn_count: 0,
                    prompt_token_estimate: runtime_session.summary.prompt_token_estimate,
                    last_context_occupancy: runtime_session.summary.last_context_occupancy.clone(),
                };
                if let Err(err) = self.deps.db.update_stats(&session_id, &stats) {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %err,
                        "failed to persist initial token stats to database"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %err,
                    "failed to load token stats from database"
                );
            }
        }

        // Restore the turn queue non-destructively: SQLite stays the durable
        // mirror until items are consumed or removed, so a restart before the
        // queue drains does not lose pending input.
        match self
            .deps
            .db
            .list_pending(&session_id, crate::db::QueueType::Turn)
        {
            Ok(items) => {
                if !items.is_empty() {
                    let core_session = runtime_session.core_session.lock().await;
                    let mut queue = core_session
                        .pending_turn_queue
                        .lock()
                        .expect("pending turn queue mutex should not be poisoned");
                    queue.extend(items);
                    tracing::debug!(
                        session_id = %session_id,
                        pending_count = queue.len(),
                        "restored pending turn queue from database"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %err,
                    "failed to load pending turn queue from database"
                );
            }
        }

        // Stale steer inputs are no longer discarded (01 §4.3): they
        // degrade into the session turn queue like any other queued input.
        match self
            .deps
            .db
            .drain_pending(&session_id, crate::db::QueueType::Steer)
        {
            Ok(items) => {
                if !items.is_empty() {
                    let materialized = runtime_session
                        .rollout_path
                        .as_ref()
                        .map(|path| materialized_steer_keys(path))
                        .unwrap_or_default();
                    let core_session = runtime_session.core_session.lock().await;
                    let mut queue = core_session
                        .pending_turn_queue
                        .lock()
                        .expect("pending turn queue mutex should not be poisoned");
                    let mut restored_steer_count = 0usize;
                    for item in &items {
                        if steer_already_materialized(item, &materialized) {
                            tracing::debug!(
                                session_id = %session_id,
                                "skipped steer restore for already-materialized input"
                            );
                            continue;
                        }
                        queue.push_back(item.clone());
                        if let Err(error) =
                            self.deps
                                .db
                                .push_pending(&session_id, crate::db::QueueType::Turn, item)
                        {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %error,
                                "failed to restore steer input into the turn queue"
                            );
                        }
                        restored_steer_count += 1;
                    }
                    tracing::debug!(
                        session_id = %session_id,
                        restored_steer_count,
                        skipped_steer_count = items.len() - restored_steer_count,
                        "degraded stale steer inputs into the pending turn queue"
                    );
                }
            }
            Err(err) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %err,
                    "failed to restore stale steer inputs from database"
                );
            }
        }

        match self.goal_durable_store.replay_goal_store(session_id).await {
            Ok(Some(mut goal_store)) => {
                if let Some(goal) = goal_store.get()
                    && goal.status == crate::goal::GoalStatus::Active
                {
                    let previous_status = goal.status;
                    match goal_store.set_status(crate::goal::GoalStatus::Paused) {
                        Ok(paused_goal) => {
                            if let Err(error) = self
                                .goal_durable_store
                                .append_status_changed(
                                    &paused_goal,
                                    previous_status,
                                    Some(
                                        "Goal paused because the session was restored without explicit resume."
                                            .to_string(),
                                    ),
                                )
                                .await
                            {
                                tracing::warn!(
                                    session_id = %session_id,
                                    error = %error,
                                    "failed to persist restored goal pause record"
                                );
                            }
                        }
                        Err(error) => {
                            tracing::warn!(
                                session_id = %session_id,
                                error = %error,
                                "failed to pause restored active goal"
                            );
                        }
                    }
                }
                self.goal_stores.lock().await.insert(session_id, goal_store);
            }
            Ok(None) => {}
            Err(error) => {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "failed to replay durable goal records"
                );
            }
        }

        if let Some(rollout_path) = runtime_session.rollout_path.as_ref() {
            let host_session_id = runtime_session
                .summary
                .parent_session_id()
                .unwrap_or(session_id);
            self.restore_waiting_user_inputs_from_rollout(
                session_id,
                host_session_id,
                rollout_path,
            )
            .await;
            self.restore_waiting_approvals_from_rollout(session_id, host_session_id, rollout_path)
                .await;
        }

        Ok(())
    }

    /// Loads durable sessions from rollout files and installs them into the runtime map.
    /// Used by integration tests and bulk-restore tooling; production startup indexes only.
    pub async fn load_persisted_sessions(self: &Arc<Self>) -> anyhow::Result<()> {
        let sessions = self.rollout_store.load_sessions(&self.deps).await?;
        tracing::info!(session_count = sessions.len(), "loaded persisted sessions");

        for (session_id, mut runtime_session) in sessions {
            self.apply_persisted_session_side_effects(session_id, &mut runtime_session)
                .await?;
            if runtime_session.summary.parent_session_id().is_none() {
                self.insert_root_session_actor(runtime_session)
                    .await
                    .map_err(|error| anyhow::anyhow!("{error}"))?;
            } else {
                let session_id = runtime_session.summary.session_id();
                self.insert_session_actor(SessionActorState::from_runtime_session(runtime_session))
                    .await;
                self.resume_pending_queue_if_idle(session_id).await;
            }
            self.materialize_abandoned_turn_recovery_if_needed(session_id)
                .await?;
        }
        Ok(())
    }

    /// When a hydrated session has an abandoned non-terminal turn and no live
    /// registry owner, persist `RecoveryDisposition::Available` once so resume
    /// and `turn/recovery/read` agree for crash-kill (not only graceful shutdown).
    pub(crate) async fn materialize_abandoned_turn_recovery_if_needed(
        self: &Arc<Self>,
        session_id: SessionId,
    ) -> anyhow::Result<()> {
        let Some(recovery) = self.turn_recovery(session_id).await? else {
            return Ok(());
        };
        let Some(handle) = self.session(session_id).await else {
            return Ok(());
        };
        let Some(path) = handle
            .turn_persistence_snapshot()
            .await
            .and_then(|snapshot| snapshot.rollout_path)
        else {
            return Ok(());
        };
        let turn_id = recovery.turn_id;
        let replay = tokio::task::spawn_blocking(move || {
            devo_core::durable_execution::read_execution_replay(&path, &turn_id)
        })
        .await??;
        if replay.recovery.is_some() {
            return Ok(());
        }
        self.persist_recovery_disposition(
            session_id,
            recovery.turn_id,
            devo_core::durable_execution::RecoveryDisposition::Available,
            "Execution was lost while the application was not running.",
        )
        .await?;
        self.broadcast_recovery_state(session_id).await;
        Ok(())
    }

    /// Rebuilds the SQLite session index from rollout SessionMeta headers.
    pub fn refresh_session_index(&self) -> anyhow::Result<()> {
        self.rollout_store.index_rollout_metadata(&self.deps.db)
    }

    /// Backfills rollout metadata into SQLite only when legacy rows lack index fields.
    pub fn backfill_session_index_if_required(&self) -> anyhow::Result<bool> {
        if !self.deps.db.session_index_backfill_required()? {
            return Ok(false);
        }
        self.rollout_store.index_rollout_metadata(&self.deps.db)?;
        Ok(true)
    }

    /// Completes deferred (in-progress) items for all active turns and
    /// persists interrupted turn records. Called on graceful shutdown.
    pub async fn shutdown(self: &Arc<Self>) {
        self.command_exec_manager.terminate_all().await;
        let session_handles = self.list_session_handles().await;

        for session_handle in session_handles {
            let session_id = session_handle.id();

            self.run_session_hook(
                session_id,
                devo_core::HookEvent::SessionEnd,
                serde_json::Map::from_iter([("reason".to_string(), serde_json::json!("other"))]),
            )
            .await;

            let Some(snapshot) = session_handle.take_shutdown_deferred_snapshot().await else {
                continue;
            };
            let Some(legacy_turn_id) = snapshot.active_turn_id else {
                continue;
            };
            let turn_id = legacy_turn_id;

            // Stop the live turn writer before appending recovery / terminal
            // rollout lines. Otherwise a concurrent journal append can leave a
            // truncated JSONL row that is no longer the final line once
            // shutdown facts are written, and restart hydration fails closed.
            self.signal_active_turn_interrupt(session_id).await;
            self.active_turns.abort_task(session_id).await;
            // In-flight spawn_blocking appends are not cancelled by abort; yield
            // so they can finish under the rollout file lock before we write.
            tokio::task::yield_now().await;

            if let Err(error) = self
                .persist_recovery_disposition(
                    session_id,
                    turn_id,
                    devo_core::durable_execution::RecoveryDisposition::Available,
                    "Application shut down during this turn.",
                )
                .await
            {
                tracing::warn!(%session_id, %error, "failed to save turn recovery state");
            }

            if let Some(ref turn) = snapshot.active_turn
                && turn.native.status == devo_protocol::native::turn::TurnStatus::WaitingApproval
            {
                if snapshot.rollout_path.is_some()
                    && let Err(error) = self.persist_turn_line_deduped(session_id, turn).await
                {
                    tracing::warn!(
                        session_id = %session_id,
                        error = %error,
                        "failed to persist waiting-approval turn on shutdown"
                    );
                }
                tracing::info!(
                    session_id = %session_id,
                    turn_id = %turn.turn_id(),
                    "preserved waiting-approval turn on shutdown"
                );
                continue;
            }

            let active_turn = snapshot.active_turn.clone();
            let native_session_id = if let Some(turn) = active_turn.as_ref() {
                turn.native.session_id
            } else if let Some(summary) = session_handle.summary().await {
                summary.native.id
            } else {
                // boundary: legacy session id when summary unavailable
                session_id
            };
            let native_turn_id = active_turn
                .as_ref()
                .map(|turn| turn.native.id)
                .unwrap_or_else(|| {
                    // boundary: shutdown snapshot legacy turn id without RuntimeTurn
                    turn_id
                });
            if let Some((item_id, item_seq, text)) = snapshot.deferred_assistant
                && !text.trim().is_empty()
            {
                self.complete_native_item(
                    native_session_id,
                    native_turn_id,
                    item_id,
                    item_seq,
                    Item::AssistantMessage { text: text.clone() },
                )
                .await;
            }
            if let Some((item_id, item_seq, text)) = snapshot.deferred_reasoning {
                self.complete_native_item(
                    native_session_id,
                    native_turn_id,
                    item_id,
                    item_seq,
                    Item::Reasoning {
                        text: text.clone(),
                        provider_payload_ref: None,
                    },
                )
                .await;
            }

            let Some(interrupted_turn) = session_handle.interrupt_active_turn().await.flatten()
            else {
                continue;
            };
            if interrupted_turn.turn_id() != turn_id {
                continue;
            }

            if snapshot.rollout_path.is_some()
                && let Err(error) = self
                    .persist_turn_line_deduped(session_id, &interrupted_turn)
                    .await
            {
                tracing::warn!(
                    session_id = %session_id,
                    error = %error,
                    "failed to persist interrupted turn on shutdown"
                );
            }

            tracing::info!(
                session_id = %session_id,
                turn_id = %interrupted_turn.turn_id(),
                "completed deferred items and interrupted turn on shutdown"
            );
        }

        let session_ids: Vec<SessionId> = self
            .list_session_handles()
            .await
            .into_iter()
            .map(|handle| handle.id())
            .collect();
        for session_id in session_ids {
            if let Some(handle) = self.sessions.lock().await.get(&session_id).cloned() {
                handle.shutdown().await;
            }
            self.remove_session_actor(session_id).await;
        }
    }
}

#[derive(Debug, Default)]
struct MaterializedSteerKeys {
    texts: HashSet<String>,
    client_user_message_ids: HashSet<String>,
}

fn materialized_steer_keys(rollout_path: &Path) -> MaterializedSteerKeys {
    let mut keys = MaterializedSteerKeys::default();
    let Ok(history) = devo_core::read_canonical_history(rollout_path) else {
        return keys;
    };
    for envelope in history.items {
        let Item::UserMessage {
            entry: UserMessageEntry::Steer,
            content,
            client_user_message_id,
        } = envelope.item
        else {
            continue;
        };
        if let Some(client_user_message_id) = client_user_message_id {
            keys.client_user_message_ids.insert(client_user_message_id);
        }
        if let Some(text) = user_message_preview_text(&content) {
            keys.texts.insert(text);
        }
    }
    keys
}

fn user_message_preview_text(content: &[UserInput]) -> Option<String> {
    content.iter().find_map(|part| match part {
        UserInput::Text { text } => text
            .lines()
            .next()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(str::to_owned),
        _ => None,
    })
}

fn pending_input_preview_text(item: &PendingInputItem) -> Option<String> {
    match &item.kind {
        PendingInputKind::UserText { text } => Some(text.clone()),
        PendingInputKind::UserInput { display_text, .. } => Some(display_text.clone()),
        _ => None,
    }
}

fn pending_client_user_message_id(item: &PendingInputItem) -> Option<String> {
    item.metadata.as_ref().and_then(|metadata| {
        metadata
            .get("clientUserMessageId")
            .or_else(|| metadata.get("client_user_message_id"))
            .and_then(|value| value.as_str())
            .map(str::to_owned)
    })
}

fn steer_already_materialized(item: &PendingInputItem, keys: &MaterializedSteerKeys) -> bool {
    if let Some(client_user_message_id) = pending_client_user_message_id(item)
        && keys
            .client_user_message_ids
            .contains(&client_user_message_id)
    {
        return true;
    }
    pending_input_preview_text(item).is_some_and(|text| keys.texts.contains(&text))
}
