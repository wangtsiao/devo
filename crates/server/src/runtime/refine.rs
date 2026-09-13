//! Continual Harness refine host entry (`session/refine/run`).
//!
//! Mid-turn / mid-ipython only **schedules** (never applies). Apply runs in the
//! post-`MergeTurn` pipeline via [`apply_pending_refine_at_boundary`] using
//! `apply_proposal_re_read`. Planning via `crates/provider` is still a stub
//! (placeholder proposal with empty edits until the auxiliary planner ships).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use super::*;
use devo_harness::{
    append_refinement, apply_proposal_re_read, record_from_proposal, should_auto_refine,
    AutoRefineSettings, HarnessState, RefineProposal,
};
use devo_protocol::native::item::{Item, ItemEnvelope, ItemState};
use devo_protocol::native::rpc_session::{SessionRefineRunParams, SessionRefineRunResult};

/// Last-write-wins pending refine requests keyed by session id (in-memory).
static PENDING_REFINES: LazyLock<Mutex<HashMap<String, PendingRefine>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Successful user-visible assistant turns since last auto-refine (root only).
static AUTO_REFINE_TURN_COUNTS: LazyLock<Mutex<HashMap<String, u32>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

#[derive(Debug, Clone)]
#[allow(dead_code)] // global / requested_at reserved for planner + telemetry
pub(crate) struct PendingRefine {
    pub proposal_id: String,
    pub instructions: Option<String>,
    pub global: bool,
    pub rollback_id: Option<String>,
    pub requested_at: chrono::DateTime<Utc>,
    /// True when scheduled by root auto-interval (Plan Mode skips apply).
    pub autonomous: bool,
}

/// Ensure the session harness directory exists with a valid (or empty) state file.
pub(crate) fn ensure_session_harness(session_dir: &Path) -> Result<PathBuf, String> {
    let path = HarnessState::file_path(session_dir);
    if path.exists() {
        HarnessState::load(&path).map_err(|e| e.to_string())?;
        return Ok(path);
    }
    let state = HarnessState::default();
    state.save_atomic(&path).map_err(|e| e.to_string())?;
    Ok(path)
}

pub(crate) fn take_pending_refine(
    session_id: &devo_protocol::native::ids::SessionId,
) -> Option<PendingRefine> {
    PENDING_REFINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id.as_str())
}

pub(crate) fn peek_pending_refine(session_id: &devo_protocol::native::ids::SessionId) -> bool {
    PENDING_REFINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .contains_key(session_id.as_str())
}

/// Abort / interrupt: drop pending refine (and compact, via compact_host).
pub(crate) fn clear_pending_refine(session_id: &devo_protocol::native::ids::SessionId) {
    let _ = take_pending_refine(session_id);
}

pub(crate) fn auto_refine_settings_from_session(
    enabled: Option<bool>,
    interval: Option<u32>,
) -> AutoRefineSettings {
    let mut settings = AutoRefineSettings::default();
    if let Some(enabled) = enabled {
        settings.enabled = enabled;
    }
    if let Some(interval) = interval {
        settings.turn_interval = interval.max(1);
    }
    settings
}

/// Bump the root user-visible turn counter; schedule auto-refine when due.
pub(crate) fn maybe_schedule_auto_refine(
    session_id: &devo_protocol::native::ids::SessionId,
    settings: &AutoRefineSettings,
    is_root: bool,
    is_goal_continuation: bool,
    turn_succeeded: bool,
    session_dir: Option<&Path>,
) -> bool {
    if !turn_succeeded || !is_root || is_goal_continuation {
        return false;
    }
    let count = {
        let mut map = AUTO_REFINE_TURN_COUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let entry = map.entry(session_id.as_str().to_string()).or_insert(0);
        *entry = entry.saturating_add(1);
        *entry
    };
    if !should_auto_refine(settings, count, is_root, is_goal_continuation) {
        return false;
    }
    let params = SessionRefineRunParams {
        session_id: session_id.clone(),
        instructions: Some("auto-interval".into()),
        global: false,
        rollback_id: None,
    };
    let result = schedule_refine_run_inner(
        session_id,
        &params,
        /*is_root*/ true,
        session_dir,
        /*autonomous*/ true,
    );
    if result.scheduled {
        let mut map = AUTO_REFINE_TURN_COUNTS
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.insert(session_id.as_str().to_string(), 0);
    }
    result.scheduled
}

/// Schedule a refine for apply at the next turn boundary / idle.
pub(crate) fn schedule_refine_run(
    session_id: &devo_protocol::native::ids::SessionId,
    params: &SessionRefineRunParams,
    is_root: bool,
    session_dir: Option<&Path>,
) -> SessionRefineRunResult {
    schedule_refine_run_inner(session_id, params, is_root, session_dir, /*autonomous*/ false)
}

fn schedule_refine_run_inner(
    session_id: &devo_protocol::native::ids::SessionId,
    params: &SessionRefineRunParams,
    is_root: bool,
    session_dir: Option<&Path>,
    autonomous: bool,
) -> SessionRefineRunResult {
    if !is_root {
        return SessionRefineRunResult {
            scheduled: false,
            note: None,
            reason: Some("refine.run is not available on child sessions".into()),
            refinement_id: None,
        };
    }
    if let Some(dir) = session_dir {
        if let Err(error) = ensure_session_harness(dir) {
            return SessionRefineRunResult {
                scheduled: false,
                note: None,
                reason: Some(format!("harness state unavailable: {error}")),
                refinement_id: None,
            };
        }
    }
    let proposal_id = format!("refine_{}", Uuid::new_v4());
    let pending = PendingRefine {
        proposal_id: proposal_id.clone(),
        instructions: params.instructions.clone(),
        global: params.global,
        rollback_id: params.rollback_id.clone(),
        requested_at: Utc::now(),
        autonomous,
    };
    PENDING_REFINES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .insert(session_id.as_str().to_string(), pending);
    SessionRefineRunResult {
        scheduled: true,
        note: Some(
            "Refinement runs when the current turn ends (or immediately if idle); applied edits appear as a Refinement item."
                .into(),
        ),
        reason: None,
        refinement_id: Some(proposal_id),
    }
}

/// Build a no-op proposal placeholder used until auxiliary model planning ships.
pub(crate) fn placeholder_proposal(pending: &PendingRefine) -> RefineProposal {
    RefineProposal {
        id: pending.proposal_id.clone(),
        trigger: pending
            .instructions
            .clone()
            .unwrap_or_else(|| "manual".into()),
        summary: "Refine scheduled (planning pending host pipeline)".into(),
        evidence: String::new(),
        expected_outcome: String::new(),
        edits: Vec::new(),
    }
}

#[allow(dead_code)] // host_request refine.status
pub(crate) fn pending_status_json(
    session_id: &devo_protocol::native::ids::SessionId,
) -> serde_json::Value {
    json!({
        "pending": peek_pending_refine(session_id),
        "inFlight": false,
    })
}

/// Apply pending refine at the turn boundary. Never called from ipython.
///
/// Returns true when an apply ran successfully.
pub(crate) fn apply_pending_refine_at_boundary(
    session_id: &devo_protocol::native::ids::SessionId,
    session_dir: &Path,
    plan_mode: bool,
) -> Option<AppliedRefine> {
    let Some(pending) = take_pending_refine(session_id) else {
        return None;
    };
    if plan_mode && pending.autonomous {
        // Re-queue autonomous refine until Plan Mode ends.
        PENDING_REFINES
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(session_id.as_str().to_string(), pending);
        return None;
    }

    let harness_path = match ensure_session_harness(session_dir) {
        Ok(path) => path,
        Err(error) => {
            tracing::warn!(%error, "refine apply skipped: harness unavailable");
            return None;
        }
    };

    let proposal = placeholder_proposal(&pending);
    match apply_proposal_re_read(&harness_path, &proposal) {
        Ok(_state) => {
            let mut record = record_from_proposal(&proposal, &harness_path, Some("local"));
            record.rollback_of = pending.rollback_id.clone();
            if let Err(error) = append_refinement(session_dir, &record) {
                tracing::warn!(%error, "failed to append refinements.jsonl");
            }
            Some(AppliedRefine {
                proposal,
                autonomous: pending.autonomous,
            })
        }
        Err(error) => {
            tracing::warn!(%error, "refine apply failed");
            None
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct AppliedRefine {
    pub proposal: RefineProposal,
    #[allow(dead_code)]
    pub autonomous: bool,
}

impl AppliedRefine {
    pub fn native_item(&self) -> Item {
        Item::Refinement {
            refinement_id: self.proposal.id.clone(),
            trigger: self.proposal.trigger.clone(),
            summary: self.proposal.summary.clone(),
            changes: self
                .proposal
                .edits
                .iter()
                .map(|e| format!("{:?} {:?}:{}", e.op, e.kind, e.id))
                .collect(),
            evidence: (!self.proposal.evidence.is_empty()).then(|| self.proposal.evidence.clone()),
            outcome: (!self.proposal.expected_outcome.is_empty())
                .then(|| self.proposal.expected_outcome.clone()),
        }
    }
}

impl ServerRuntime {
    /// Native `session/refine/run` (L2-DES-HARNESS-001).
    pub(super) async fn handle_native_session_refine_run(
        self: &Arc<Self>,
        request_id: serde_json::Value,
        params: serde_json::Value,
    ) -> serde_json::Value {
        let params: SessionRefineRunParams = match serde_json::from_value(params) {
            Ok(params) => params,
            Err(error) => {
                return self.error_response(
                    request_id,
                    ProtocolErrorCode::InvalidParams,
                    format!("invalid session/refine/run params: {error}"),
                );
            }
        };
        let Ok(legacy_id) = SessionId::try_from(params.session_id.as_str()) else {
            return self.error_response(
                request_id,
                ProtocolErrorCode::InvalidParams,
                format!("invalid sessionId: {}", params.session_id.as_str()),
            );
        };
        let (is_root, session_dir) = {
            let sessions = self.sessions.lock().await;
            let Some(handle) = sessions.get(&legacy_id) else {
                return self.error_response(
                    request_id,
                    ProtocolErrorCode::SessionNotFound,
                    format!("session {} not found", params.session_id.as_str()),
                );
            };
            let summary = handle.summary().await;
            let is_root = summary.as_ref().is_none_or(|s| s.agent_path.is_none());
            let session_dir = handle
                .record()
                .await
                .flatten()
                .and_then(|record| record.rollout_path.parent().map(Path::to_path_buf));
            (is_root, session_dir)
        };
        let result = schedule_refine_run(
            &params.session_id,
            &params,
            is_root,
            session_dir.as_deref(),
        );
        serde_json::to_value(SuccessResponse {
            id: request_id,
            result,
        })
        .expect("serialize session/refine/run response")
    }

    /// Persist `Item::Refinement` after a successful boundary apply.
    pub(crate) async fn persist_applied_refinement(
        self: &Arc<Self>,
        session_id: SessionId,
        applied: &AppliedRefine,
    ) {
        let Some(handle) = self.session(session_id).await else {
            return;
        };
        let Some(record) = handle.record().await.flatten() else {
            return;
        };
        let turn_id = handle
            .resume_snapshot()
            .await
            .and_then(|snap| snap.latest_turn.map(|t| t.turn_id))
            .unwrap_or_else(TurnId::new);
        let item_id = ItemId::new();
        let seq = handle.allocate_item_seq().await.unwrap_or(0);
        let now = Utc::now();
        let envelope = ItemEnvelope {
            id: devo_protocol::native::ids::ItemId::from_legacy_uuid(Uuid::from(item_id)),
            session_id: devo_protocol::native::ids::SessionId::from_legacy_uuid(Uuid::from(
                session_id,
            )),
            turn_id: devo_protocol::native::ids::TurnId::from_legacy_uuid(Uuid::from(turn_id)),
            seq,
            revision: 1,
            created_at: now,
            updated_at: now,
            state: ItemState::Completed,
            item: applied.native_item(),
        };
        if let Err(error) = self.rollout_store.append_canonical_item(&record, envelope) {
            tracing::warn!(%error, "failed to persist Item::Refinement");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: child sessions cannot schedule refine.run.
    #[test]
    fn children_cannot_schedule() {
        let id = devo_protocol::native::ids::SessionId::from_string("ses_child".into());
        let params = SessionRefineRunParams {
            session_id: id.clone(),
            instructions: None,
            global: false,
            rollback_id: None,
        };
        let result = schedule_refine_run(&id, &params, false, None);
        assert!(!result.scheduled);
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: root schedule last-write-wins pending refine.
    #[test]
    fn root_schedule_is_pending() {
        let id = devo_protocol::native::ids::SessionId::from_string(format!(
            "ses_{}",
            Uuid::new_v4()
        ));
        let params = SessionRefineRunParams {
            session_id: id.clone(),
            instructions: Some("focus".into()),
            global: false,
            rollback_id: None,
        };
        let result = schedule_refine_run(&id, &params, true, None);
        assert!(result.scheduled);
        assert!(peek_pending_refine(&id));
        let taken = take_pending_refine(&id).expect("pending");
        assert_eq!(taken.instructions.as_deref(), Some("focus"));
        assert!(!taken.autonomous);
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: should_auto_refine integration schedules on interval for roots.
    #[test]
    fn auto_interval_schedules_on_root() {
        let id = devo_protocol::native::ids::SessionId::from_string(format!(
            "ses_{}",
            Uuid::new_v4()
        ));
        let settings = AutoRefineSettings {
            enabled: true,
            turn_interval: 2,
            ..AutoRefineSettings::default()
        };
        assert!(!maybe_schedule_auto_refine(
            &id,
            &settings,
            true,
            false,
            true,
            None
        ));
        assert!(!peek_pending_refine(&id));
        assert!(maybe_schedule_auto_refine(
            &id,
            &settings,
            true,
            false,
            true,
            None
        ));
        assert!(peek_pending_refine(&id));
        let taken = take_pending_refine(&id).expect("auto pending");
        assert!(taken.autonomous);
        assert_eq!(taken.instructions.as_deref(), Some("auto-interval"));
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: children never auto-refine even at interval.
    #[test]
    fn auto_interval_skips_children() {
        let id = devo_protocol::native::ids::SessionId::from_string(format!(
            "ses_{}",
            Uuid::new_v4()
        ));
        let settings = AutoRefineSettings {
            turn_interval: 1,
            ..AutoRefineSettings::default()
        };
        assert!(!maybe_schedule_auto_refine(
            &id,
            &settings,
            false,
            false,
            true,
            None
        ));
        assert!(!peek_pending_refine(&id));
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: boundary apply writes refinements.jsonl (never mid-ipython).
    #[test]
    fn apply_at_boundary_persists_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let id = devo_protocol::native::ids::SessionId::from_string(format!(
            "ses_{}",
            Uuid::new_v4()
        ));
        let params = SessionRefineRunParams {
            session_id: id.clone(),
            instructions: Some("manual".into()),
            global: false,
            rollback_id: None,
        };
        assert!(schedule_refine_run(&id, &params, true, Some(dir.path())).scheduled);
        let applied = apply_pending_refine_at_boundary(&id, dir.path(), false).expect("applied");
        assert!(!applied.proposal.id.is_empty());
        let log = std::fs::read_to_string(devo_harness::refinements_log_path(dir.path())).unwrap();
        assert!(log.contains(&applied.proposal.id));
        assert!(!peek_pending_refine(&id));
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: Plan Mode holds autonomous pending without applying.
    #[test]
    fn plan_mode_defers_autonomous_apply() {
        let dir = tempfile::tempdir().unwrap();
        let id = devo_protocol::native::ids::SessionId::from_string(format!(
            "ses_{}",
            Uuid::new_v4()
        ));
        let settings = AutoRefineSettings {
            turn_interval: 1,
            ..AutoRefineSettings::default()
        };
        assert!(maybe_schedule_auto_refine(
            &id,
            &settings,
            true,
            false,
            true,
            Some(dir.path())
        ));
        assert!(apply_pending_refine_at_boundary(&id, dir.path(), true).is_none());
        assert!(peek_pending_refine(&id));
        clear_pending_refine(&id);
    }

    #[test]
    fn default_interval_matches_spec() {
        assert_eq!(devo_harness::DEFAULT_TURN_INTERVAL, 25);
        assert_eq!(
            auto_refine_settings_from_session(None, None).turn_interval,
            25
        );
        assert!(auto_refine_settings_from_session(None, None).enabled);
        assert!(!auto_refine_settings_from_session(Some(false), Some(10)).enabled);
        assert_eq!(
            auto_refine_settings_from_session(Some(true), Some(10)).turn_interval,
            10
        );
    }
}
