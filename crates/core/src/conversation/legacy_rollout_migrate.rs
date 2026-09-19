//! Legacy (v1) rollout → Native v2 projection for **migration / fixtures only**.
//!
//! **Live path:** construct [`RolloutLineV2`] via [`super::rollout_write`] (or
//! Native builders), append, and [`super::RolloutWriteState::observe_v2_line`]
//! — do not call this module.
//!
//! **Migrate path:** [`project_legacy_line`] only (fixtures / offline tooling).
//! Packed `ItemRecord` expansion is quarantined in
//! [`super::rollout_write_state::packed_expand`].
//! Pre-RLM on-disk v1 resume is refuse-closed (`parse_rollout_line`).

use crate::conversation::rollout_v2::{ROLLOUT_FORMAT_VERSION, RolloutLineV2};
use crate::conversation::rollout_write::{
    compaction_snapshot_line_v2, message_edit_line_v2, native_item_id, native_session_id,
    native_turn_id, session_context_line_v2, session_line_v2_from_record, settings_line_v2,
    title_line_v2, turn_line_v2_from_record, turn_superseded_line_v2, workspace_change_line_v2,
    workspace_checkpoint_line_v2, workspace_restore_completed_line_v2,
    workspace_restore_started_line_v2,
};
use crate::conversation::rollout_write_state::{LegacyProjectError, RolloutWriteState};
use crate::conversation::{RolloutLine, SessionMetaLine, TurnLine};

/// Projects one legacy rollout line into zero or more v2 lines.
///
/// Most lines map 1:1; an `Item` record expands to one line per packed
/// payload. Settings epochs and item seqs are assigned by `write_state`.
pub fn project_legacy_line(
    write_state: &mut RolloutWriteState,
    line: &RolloutLine,
) -> Result<Vec<RolloutLineV2>, LegacyProjectError> {
    match line {
        RolloutLine::SessionMeta(line) => project_session_meta(write_state, line),
        RolloutLine::Turn(line) => project_turn(line),
        RolloutLine::Item(line) => write_state.item_lines_from_record(&line.item, line.timestamp),
        RolloutLine::SessionTitleUpdated(line) => Ok(vec![title_line_v2(
            line.timestamp,
            native_session_id(line.session_id)?,
            line.title.clone(),
            line.previous_title.clone(),
        )]),
        RolloutLine::SessionContextUpdated(line) => Ok(vec![session_context_line_v2(
            line.timestamp,
            native_session_id(line.session_id)?,
            line.session_context.clone(),
        )]),
        RolloutLine::SessionSettings(line) => {
            let epoch = write_state.allocate_settings_epoch();
            Ok(vec![settings_line_v2(
                line.timestamp,
                native_session_id(line.session_id)?,
                line.field,
                line.value.clone(),
                epoch,
            )])
        }
        RolloutLine::CompactionSnapshot(line) => {
            Ok(vec![compaction_snapshot_line_v2(line.timestamp, line)?])
        }
        RolloutLine::MessageEditRecorded(line) => {
            Ok(vec![message_edit_line_v2(line.timestamp, &line.record)?])
        }
        RolloutLine::TurnSuperseded(line) => {
            Ok(vec![turn_superseded_line_v2(line.timestamp, &line.record)?])
        }
        RolloutLine::TurnWorkspaceCheckpointRecorded(line) => {
            Ok(vec![workspace_checkpoint_line_v2(
                line.timestamp,
                line.record.clone(),
            )])
        }
        RolloutLine::TurnWorkspaceChangeRecorded(line) => Ok(vec![workspace_change_line_v2(
            line.timestamp,
            line.record.clone(),
        )]),
        RolloutLine::TurnWorkspaceRestoreStarted(line) => {
            Ok(vec![workspace_restore_started_line_v2(
                line.timestamp,
                line.record.clone(),
            )])
        }
        RolloutLine::TurnWorkspaceRestoreCompleted(line) => {
            Ok(vec![workspace_restore_completed_line_v2(
                line.timestamp,
                line.record.clone(),
            )])
        }
        RolloutLine::SessionRollback(line) => Ok(vec![RolloutLineV2::SessionRollback {
            v: ROLLOUT_FORMAT_VERSION,
            timestamp: line.timestamp,
            session_id: native_session_id(line.session_id)?,
            retained_turn_ids: line
                .retained_turn_ids
                .iter()
                .map(native_turn_id)
                .collect::<Result<_, _>>()?,
            retained_item_ids: line
                .retained_item_ids
                .iter()
                .map(native_item_id)
                .collect::<Result<_, _>>()?,
            latest_turn_id: match &line.latest_turn_id {
                Some(id) => Some(native_turn_id(*id)?),
                None => None,
            },
        }]),
    }
}

fn project_session_meta(
    write_state: &mut RolloutWriteState,
    line: &SessionMetaLine,
) -> Result<Vec<RolloutLineV2>, LegacyProjectError> {
    // Learn cwd for subsequent CommandExecution fallbacks (legacy payloads
    // never recorded cwd).
    write_state.note_session_cwd(line.session.cwd.clone());
    Ok(vec![session_line_v2_from_record(
        &line.session,
        line.timestamp,
    )?])
}

fn project_turn(line: &TurnLine) -> Result<Vec<RolloutLineV2>, LegacyProjectError> {
    Ok(vec![turn_line_v2_from_record(&line.turn, line.timestamp)?])
}
