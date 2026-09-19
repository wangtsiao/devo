#![allow(dead_code)]

use std::path::Path;

use anyhow::Context;
use anyhow::Result;
use devo_core::ParsedRolloutLine;
use devo_core::RolloutLine;
use devo_core::RolloutLineV2;
use devo_core::TurnWorkspaceChangeRecordedLine;
use devo_core::TurnWorkspaceCheckpointRecordedLine;
use devo_core::TurnWorkspaceRestoreCompletedLine;
use devo_core::TurnWorkspaceRestoreStartedLine;
use devo_core::item_record_from_native;
use devo_core::legacy_compaction_line_from_native;
use devo_core::legacy_lines_from_internal;
use devo_core::legacy_rollback_line_from_native;
use devo_core::legacy_title_line_from_native;
use devo_core::parse_rollout_line;
use devo_core::session_record_from_native;
use devo_core::turn_record_from_native;

/// Reads a rollout file into the legacy records inspected by integration tests.
/// Session/Turn/Item use the same direct record adapters as production replay;
/// only records still owned by the compatibility path become legacy lines.
pub fn read_rollout_lines_dual(path: &Path) -> Result<Vec<RolloutLine>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("read rollout {}", path.display()))?;
    let mut out = Vec::new();
    for raw in text.lines().filter(|line| !line.trim().is_empty()) {
        match parse_rollout_line(raw)
            .with_context(|| format!("parse line in {}", path.display()))?
        {
            ParsedRolloutLine::V2(line) => match *line {
                RolloutLineV2::SessionMeta {
                    timestamp,
                    session,
                    extras,
                    ..
                } => out.push(RolloutLine::SessionMeta(Box::new(
                    devo_core::SessionMetaLine {
                        timestamp,
                        session: session_record_from_native(&session, extras.as_deref())?,
                    },
                ))),
                RolloutLineV2::Turn {
                    timestamp,
                    turn,
                    extras,
                    ..
                } => out.push(RolloutLine::Turn(Box::new(devo_core::TurnLine {
                    timestamp,
                    turn: turn_record_from_native(&turn, extras.as_deref())?,
                }))),
                RolloutLineV2::Item { item, .. } => {
                    if let Some(item) = item_record_from_native(&item)? {
                        out.push(RolloutLine::Item(Box::new(devo_core::ItemLine {
                            timestamp: item.timestamp,
                            item,
                        })));
                    }
                }
                RolloutLineV2::WorkspaceCheckpoint {
                    timestamp, record, ..
                } => out.push(RolloutLine::TurnWorkspaceCheckpointRecorded(Box::new(
                    TurnWorkspaceCheckpointRecordedLine { timestamp, record },
                ))),
                RolloutLineV2::WorkspaceChange {
                    timestamp, record, ..
                } => out.push(RolloutLine::TurnWorkspaceChangeRecorded(Box::new(
                    TurnWorkspaceChangeRecordedLine { timestamp, record },
                ))),
                RolloutLineV2::WorkspaceRestoreStarted {
                    timestamp, record, ..
                } => out.push(RolloutLine::TurnWorkspaceRestoreStarted(Box::new(
                    TurnWorkspaceRestoreStartedLine { timestamp, record },
                ))),
                RolloutLineV2::WorkspaceRestoreCompleted {
                    timestamp, record, ..
                } => out.push(RolloutLine::TurnWorkspaceRestoreCompleted(Box::new(
                    TurnWorkspaceRestoreCompletedLine { timestamp, record },
                ))),
                RolloutLineV2::Internal {
                    timestamp,
                    session_id,
                    turn_id,
                    seq,
                    entry,
                    ..
                } => out.extend(legacy_lines_from_internal(
                    timestamp,
                    &session_id,
                    turn_id.as_ref(),
                    seq,
                    &entry,
                )?),
                RolloutLineV2::SessionTitleUpdated {
                    timestamp,
                    session_id,
                    title,
                    previous_title,
                    ..
                } => out.push(legacy_title_line_from_native(
                    timestamp,
                    &session_id,
                    title,
                    previous_title,
                )?),
                RolloutLineV2::CompactionSnapshot {
                    timestamp,
                    session_id,
                    turn_id,
                    summary_item_id,
                    preserved_item_ids,
                    context_occupancy,
                    ..
                } => out.push(legacy_compaction_line_from_native(
                    timestamp,
                    &session_id,
                    &turn_id,
                    &summary_item_id,
                    &preserved_item_ids,
                    context_occupancy,
                )?),
                RolloutLineV2::SessionRollback {
                    timestamp,
                    session_id,
                    retained_turn_ids,
                    retained_item_ids,
                    latest_turn_id,
                    ..
                } => out.push(legacy_rollback_line_from_native(
                    timestamp,
                    &session_id,
                    &retained_turn_ids,
                    &retained_item_ids,
                    latest_turn_id.as_ref(),
                )?),
            },
        }
    }
    Ok(out)
}
