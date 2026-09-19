//! Shared compaction summary + snapshot persistence helpers.
//!
//! Manual `/compact` and query-loop auto/proactive compaction both need to:
//! - derive preserved item ids from the compacted response suffix
//! - build a durable `ContextCompaction` summary item
//! - append a `CompactionSnapshot` so resume can rebuild `prompt_messages`

use std::sync::Arc;

use chrono::Utc;
use devo_core::CompactionSnapshotLine;
use devo_core::Message;
use devo_core::ResponseItem;
use devo_protocol::approx_tokens_from_byte_count;
use devo_protocol::native::ids::{ItemId, SessionId, TurnId};
use devo_protocol::native::item::ContextOccupancy;
use devo_protocol::native::item::Item;
use devo_protocol::native::item::UserInput;
use devo_protocol::native::turn::TurnKind;

use super::ServerRuntime;
use crate::execution::PersistedTurnItem;
use crate::persisted_native_item::PersistedNativeItem;
use crate::persisted_native_item::context_compaction_item;
use crate::persisted_native_item::history_entry_from_native_item;
use crate::persistence::RolloutStore;

/// Match the compacted preserve suffix against the prompt-visible journal tail.
pub(crate) fn preserved_item_ids_from_compacted(
    persisted_turn_items: &[PersistedTurnItem],
    compacted_items: &[ResponseItem],
) -> Vec<devo_protocol::native::ids::ItemId> {
    let mut normalized_persisted_items = Vec::new();
    for item in persisted_turn_items {
        if !crate::persistence::prompt_visible_persisted_turn_item(item) {
            continue;
        }

        // The compactor returns a summary followed by the prompt-visible suffix it
        // kept verbatim. Normalize persisted items into that same response shape
        // without allocating a short intermediate Vec for every journal item.
        match &item.item {
            Item::UserMessage { content, .. } => {
                let text = content
                    .iter()
                    .filter_map(|part| match part {
                        UserInput::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                normalized_persisted_items
                    .push((item.item_id, ResponseItem::Message(Message::user(text))));
            }
            Item::AssistantMessage { text, .. } => {
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::Message(Message::assistant_text(text.clone())),
                ));
            }
            Item::Plan { entries } => {
                let text = entries
                    .iter()
                    .map(|entry| entry.step.as_str())
                    .collect::<Vec<_>>()
                    .join("\n");
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::Message(Message::assistant_text(text)),
                ));
            }
            Item::HostedToolCall { output, .. } => {
                let text = output
                    .as_ref()
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::Message(Message::assistant_text(text)),
                ));
            }
            Item::ContextCompaction { summary, .. } => {
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::Message(Message::assistant_text(
                        summary.clone().unwrap_or_default(),
                    )),
                ));
            }
            Item::Reasoning { .. } => {}
            Item::ToolCall {
                call_id,
                tool_name,
                input,
                ..
            } => {
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::ToolCall {
                        id: call_id.clone(),
                        name: tool_name.clone(),
                        input: input.clone().unwrap_or(serde_json::Value::Null),
                    },
                ));
            }
            Item::ToolResult {
                call_id,
                output,
                is_error,
                ..
            } => {
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::ToolCallOutput {
                        tool_use_id: call_id.clone(),
                        content: match output {
                            serde_json::Value::String(text) => text.clone(),
                            other => other.to_string(),
                        },
                        is_error: *is_error,
                    },
                ));
            }
            Item::CommandExecution {
                call_id,
                input,
                output,
                is_error,
                ..
            } => {
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::ToolCall {
                        id: call_id.clone(),
                        name: "exec_command".into(),
                        input: input.clone().unwrap_or(serde_json::Value::Null),
                    },
                ));
                normalized_persisted_items.push((
                    item.item_id,
                    ResponseItem::ToolCallOutput {
                        tool_use_id: call_id.clone(),
                        content: match output.clone().unwrap_or(serde_json::Value::Null) {
                            serde_json::Value::String(text) => text,
                            other => other.to_string(),
                        },
                        is_error: *is_error,
                    },
                ));
            }
            Item::Approval { .. }
            | Item::FileChange { .. }
            | Item::UserInputRequest { .. }
            | Item::SubAgent { .. }
            | Item::BackgroundTask { .. }
            | Item::GoalProgress { .. }
            | Item::Refinement { .. }
            | Item::Warning { .. }
            | Item::BranchSummary { .. } => {}
        }
    }
    let preserved = compacted_items.get(1..).unwrap_or(&[]);
    if preserved.is_empty() {
        return Vec::new();
    }
    let preserved_len = preserved.len();
    if normalized_persisted_items.len() < preserved_len {
        return Vec::new();
    }
    let suffix = &normalized_persisted_items[normalized_persisted_items.len() - preserved_len..];
    if suffix.iter().map(|(_, item)| item).eq(preserved.iter()) {
        suffix.iter().map(|(item_id, _)| *item_id).collect()
    } else {
        Vec::new()
    }
}

/// Build the durable Native summary item from compacted history.
pub(crate) fn summary_item_from_compacted(compacted_items: &[ResponseItem]) -> Item {
    let summary_text = compacted_items
        .first()
        .and_then(|item| match item {
            ResponseItem::Message(message) => {
                message.content.iter().find_map(|block| match block {
                    devo_core::ContentBlock::Text { text } => Some(text.clone()),
                    devo_core::ContentBlock::Reasoning { .. }
                    | devo_core::ContentBlock::ProviderReasoning { .. }
                    | devo_core::ContentBlock::ToolUse { .. }
                    | devo_core::ContentBlock::HostedToolUse { .. }
                    | devo_core::ContentBlock::ToolResult { .. }
                    | devo_core::ContentBlock::Image { .. } => None,
                })
            }
            ResponseItem::Reason { text } => Some(text.clone()),
            ResponseItem::ToolCall { .. } | ResponseItem::ToolCallOutput { .. } => None,
        })
        .unwrap_or_default();
    context_compaction_item(summary_text)
}

/// Construct the rollout compaction snapshot line.
///
/// Bridges Native → legacy UUID only at the packed `CompactionSnapshotLine`
/// durable-record boundary (not for live registry lookups).
pub(crate) fn build_compaction_snapshot_line(
    session_id: &SessionId,
    turn_id: &TurnId,
    summary_item_id: &ItemId,
    preserved_item_ids: Vec<ItemId>,
    context_occupancy: Option<ContextOccupancy>,
) -> CompactionSnapshotLine {
    CompactionSnapshotLine {
        timestamp: Utc::now(),
        session_id: *session_id,
        turn_id: *turn_id,
        summary_item_id: *summary_item_id,
        preserved_item_ids: preserved_item_ids.clone(),
        context_occupancy,
    }
}

/// Inputs needed to append a compaction summary item and its snapshot.
pub(crate) struct CompactionSummaryPersist {
    pub(crate) session_id: SessionId,
    pub(crate) turn_id: TurnId,
    pub(crate) summary_item_id: ItemId,
    pub(crate) item_seq: u64,
    pub(crate) summary_item: Item,
    pub(crate) snapshot: CompactionSnapshotLine,
}

/// Append the summary item and compaction snapshot to the durable rollout.
pub(crate) fn append_compaction_summary_and_snapshot(
    rollout_store: &RolloutStore,
    rollout_path: &std::path::Path,
    persist: CompactionSummaryPersist,
) -> anyhow::Result<()> {
    use devo_protocol::native::item::ItemState;
    use devo_protocol::native::wire_projector::typed_item_envelope;

    let CompactionSummaryPersist {
        session_id,
        turn_id,
        summary_item_id,
        item_seq,
        summary_item,
        snapshot,
    } = persist;
    let envelope = typed_item_envelope(
        session_id,
        turn_id,
        summary_item_id,
        item_seq,
        &summary_item,
        ItemState::Completed,
        Utc::now(),
        None,
    );
    rollout_store.append_canonical_item_at(rollout_path, envelope)?;
    rollout_store.append_compaction_snapshot_at(rollout_path, snapshot)?;
    Ok(())
}

/// Build the in-memory journal entry for a compaction summary item.
pub(crate) fn compaction_persisted_turn_item(
    turn_id: devo_protocol::native::ids::TurnId,
    turn_kind: TurnKind,
    item_id: devo_protocol::native::ids::ItemId,
    summary_item: Item,
) -> PersistedTurnItem {
    PersistedNativeItem::new(turn_id, turn_kind, item_id, summary_item)
}

impl ServerRuntime {
    /// Persist an in-turn (auto/proactive) compaction summary + snapshot.
    ///
    /// Must not block on the session-actor mailbox: the actor is waiting on the
    /// turn event stream. Mutate inline scratch under the stream lock, then write
    /// rollout after releasing the lock.
    pub(crate) async fn persist_in_turn_compaction(
        self: &Arc<Self>,
        session_id: SessionId,
        turn_id: TurnId,
        summary_item_id: ItemId,
        compacted_items: &[ResponseItem],
    ) -> Option<u64> {
        let Some(stream) = self.active_stream_state(session_id).await else {
            tracing::warn!(
                session_id = %session_id,
                turn_id = %turn_id,
                "in-turn compaction persist skipped: no active stream"
            );
            return None;
        };

        let spawn_stable_items = self
            .active_turns
            .spawn_snapshot_for_session(session_id)
            .await
            .map(|snapshot| snapshot.stable_items)
            .unwrap_or_default();

        let rollout = {
            let mut stream = stream.lock().await;
            let Some(inline) = stream.turn_inline.as_mut() else {
                tracing::warn!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    "in-turn compaction persist skipped: no inline state"
                );
                return None;
            };
            if inline.turn_id != turn_id {
                tracing::warn!(
                    session_id = %session_id,
                    turn_id = %turn_id,
                    inline_turn_id = %inline.turn_id,
                    "in-turn compaction persist skipped: turn mismatch"
                );
                return None;
            }

            let mut journal = spawn_stable_items;
            journal.extend(inline.persisted_turn_items.iter().cloned());
            let preserved_item_ids = preserved_item_ids_from_compacted(&journal, compacted_items);
            let summary_item = summary_item_from_compacted(compacted_items);

            let prompt_bytes = compacted_items
                .iter()
                .map(|item| serde_json::to_string(item).map_or(0, |json| json.len()))
                .sum::<usize>();
            let conversation_tokens = approx_tokens_from_byte_count(prompt_bytes);

            let model = inline
                .summary
                .model_name()
                .and_then(|slug| {
                    inline
                        .hook_context
                        .runtime_context
                        .model_catalog
                        .get(slug)
                        .or_else(|| self.deps.model_catalog.get(slug))
                })
                .or_else(|| {
                    inline.summary.model_binding_id().and_then(|binding| {
                        inline
                            .hook_context
                            .runtime_context
                            .model_catalog
                            .get(binding)
                            .or_else(|| self.deps.model_catalog.get(binding))
                    })
                });
            let window = inline
                .summary
                .settings
                .effective_context_window
                .or_else(|| model.map(super::context_occupancy::resolved_compaction_limit))
                .unwrap_or(0);
            let previous_occupancy = inline.summary.last_context_occupancy.clone();
            let occupancy = super::context_occupancy::occupancy_after_compaction(
                window,
                previous_occupancy.as_ref(),
                conversation_tokens,
                None,
            );
            inline.summary.last_context_occupancy = Some(occupancy.clone());
            inline.summary.last_query_total_tokens = occupancy.total_tokens as usize;
            inline.summary.prompt_token_estimate =
                conversation_tokens.try_into().unwrap_or(usize::MAX);

            let item_seq = inline.allocate_item_seq();
            let snapshot = build_compaction_snapshot_line(
                &session_id,
                &turn_id,
                &summary_item_id,
                preserved_item_ids,
                Some(occupancy),
            );
            inline.latest_compaction_snapshot = Some(snapshot.clone());
            inline
                .persisted_turn_items
                .push(compaction_persisted_turn_item(
                    inline.turn_id,
                    inline.turn_kind,
                    summary_item_id,
                    summary_item.clone(),
                ));
            if let Some(history_item) = history_entry_from_native_item(&summary_item) {
                inline.history_items.push(history_item);
            }

            inline
                .rollout_path
                .clone()
                .map(|path| (path, item_seq, summary_item, snapshot))
        };

        let (rollout_path, item_seq, summary_item, snapshot) = rollout?;
        append_compaction_summary_and_snapshot(
            &self.rollout_store,
            &rollout_path,
            CompactionSummaryPersist {
                session_id,
                turn_id,
                summary_item_id,
                item_seq,
                summary_item,
                snapshot,
            },
        )
        .map_err(
            |error| tracing::warn!(%session_id, %error, "compaction snapshot persistence failed"),
        )
        .ok()?;
        Some(item_seq)
    }
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;

    use super::*;

    #[test]
    fn preserved_item_ids_match_complete_command_execution_pair() {
        let command_item_id = ItemId::new();
        let command_input = serde_json::json!({ "cmd": "printf ok" });
        let command_output = serde_json::Value::String("ok".to_string());
        let persisted_turn_items = vec![PersistedNativeItem::new(
            TurnId::new(),
            TurnKind::Regular,
            command_item_id,
            Item::CommandExecution {
                call_id: "call-1".to_string(),
                command: "printf ok".to_string(),
                argv: None,
                cwd: Default::default(),
                input: Some(command_input.clone()),
                output: Some(command_output.clone()),
                exit_code: None,
                execution_handle: None,
                is_error: false,
                execution_mode: devo_protocol::native::item::ExecutionMode::Foreground,
                origin: devo_protocol::native::item::ExecOrigin::AgentTool,
                sandbox: None,
            },
        )];
        let compacted_items = vec![
            ResponseItem::Message(Message::assistant_text("summary")),
            ResponseItem::ToolCall {
                id: "call-1".to_string(),
                name: "exec_command".to_string(),
                input: command_input,
            },
            ResponseItem::ToolCallOutput {
                tool_use_id: "call-1".to_string(),
                content: "ok".to_string(),
                is_error: false,
            },
        ];

        assert_eq!(
            preserved_item_ids_from_compacted(&persisted_turn_items, &compacted_items),
            vec![command_item_id, command_item_id]
        );
    }
}
