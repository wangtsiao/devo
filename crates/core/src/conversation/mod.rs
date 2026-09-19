pub mod event_projection;
pub mod history;
pub mod legacy_rollout_migrate;
pub mod native_from_turn_item;
pub mod native_replay;
pub mod rollout_v2;
pub mod rollout_write;
pub mod rollout_write_state;
pub mod session_tree;

mod records;

pub use devo_protocol::{ItemId, SessionId, SessionTitleState, TurnId, TurnStatus, TurnUsage};
pub use event_projection::{
    DerivedEvent, EVENT_SCHEMA_VERSION, events_from_v2_line, session_stream_id, sessions_stream_id,
    source_fact_id,
};
pub use history::{CanonicalHistory, HistoryReadError, read_canonical_history};
pub use legacy_rollout_migrate::project_legacy_line;
pub use native_from_turn_item::native_item_from_turn_item;
pub use native_replay::{
    NativeReplayError, item_record_from_native, legacy_compaction_line_from_native,
    legacy_lines_from_internal, legacy_rollback_line_from_native, legacy_title_line_from_native,
    session_record_from_native, turn_record_from_native,
};
pub use records::{
    ApprovalDecisionItem, ApprovalRequestItem, CommandExecutionItem, CompactionSnapshotLine,
    ItemLine, ItemRecord, MessageEditRecordedLine, RolloutLine, SessionContextUpdatedLine,
    SessionMetaLine, SessionRecord, SessionRollbackLine, SessionSettingsField, SessionSettingsLine,
    SessionTitleUpdatedLine, TextItem, ToolCallItem, ToolProgressItem, ToolResultItem, TurnError,
    TurnItem, TurnLine, TurnRecord, TurnSupersededLine, TurnWorkspaceChangeRecordedLine,
    TurnWorkspaceCheckpointRecordedLine, TurnWorkspaceRestoreCompletedLine,
    TurnWorkspaceRestoreStartedLine, Worklog,
};
pub use rollout_v2::{
    InternalRecordV2, ParsedRolloutLine, ROLLOUT_FORMAT_VERSION, RolloutLineReadError,
    RolloutLineV2, SessionPersistenceExtras, TurnPersistenceExtras, parse_rollout_line,
};
pub use rollout_write::{
    canonical_turn_from_record, compaction_snapshot_line_v2, message_edit_line_v2, native_item_id,
    native_session_from_record, native_session_id, native_turn_id, session_context_line_v2,
    session_leaf_line_v2, session_line_v2, session_line_v2_from_record,
    session_persistence_extras_from_record, session_rollback_line_v2, settings_line_v2,
    title_line_v2, tree_edge_line_v2, turn_line_v2, turn_line_v2_from_record,
    turn_persistence_extras_from_record, turn_superseded_line_v2, workspace_change_line_v2,
    workspace_checkpoint_line_v2, workspace_restore_completed_line_v2,
    workspace_restore_started_line_v2,
};
pub use rollout_write_state::{LegacyProjectError, RolloutWriteState};
pub use session_tree::{
    active_path_item_ids, build_session_tree, is_tree_visible_item, path_root_to_leaf,
    resolve_leaf_id, resolve_parent_map,
};
