//! Continual Harness refine host entry (`session/refine/run`).
//!
//! Mid-turn / mid-ipython only **schedules** (never applies). Apply runs in the
//! post-`MergeTurn` pipeline via [`apply_pending_refine_at_boundary`] using
//! `apply_proposal_re_read`. Planning prefers LLM `UsagePurpose::Refine` with
//! heuristic fallback ([`plan_refine_proposal`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use super::*;
use devo_harness::{
    AutoRefineSettings, HarnessEntry, HarnessKind, HarnessScope, HarnessState, RefineEdit,
    RefineEditOp, RefineProposal, append_refinement, apply_proposal_re_read, record_from_proposal,
    should_auto_refine,
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
        session_id: *session_id,
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
    schedule_refine_run_inner(
        session_id,
        params,
        is_root,
        session_dir,
        /*autonomous*/ false,
    )
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
    if let Some(dir) = session_dir
        && let Err(error) = ensure_session_harness(dir) {
            return SessionRefineRunResult {
                scheduled: false,
                note: None,
                reason: Some(format!("harness state unavailable: {error}")),
                refinement_id: None,
            };
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

/// Build a no-op proposal placeholder used when instructions are absent.
pub(crate) fn placeholder_proposal(pending: &PendingRefine) -> RefineProposal {
    RefineProposal {
        id: pending.proposal_id.clone(),
        trigger: pending
            .instructions
            .clone()
            .unwrap_or_else(|| "manual".into()),
        summary: "Refine scheduled (no instruction edits)".into(),
        evidence: String::new(),
        expected_outcome: String::new(),
        edits: Vec::new(),
    }
}

fn truncate_for_harness(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Heuristic refine planner: with non-empty instructions, emit a memory Create
/// edit. Used as fallback when LLM planning is unavailable.
pub(crate) fn plan_refine_proposal(pending: &PendingRefine, harness_path: &Path) -> RefineProposal {
    let _ = HarnessState::load(harness_path);
    let Some(instructions) = pending
        .instructions
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    else {
        return placeholder_proposal(pending);
    };

    let uuid_hex = Uuid::new_v4().as_simple().to_string();
    let mem_id = format!("mem_refine_{}", &uuid_hex[..8]);
    let title = truncate_for_harness(instructions, 80);
    let content = truncate_for_harness(instructions, 500);
    let now = Utc::now();
    let entry = HarnessEntry {
        id: mem_id.clone(),
        kind: HarnessKind::Memory,
        title: title.clone(),
        content: content.clone(),
        path: None,
        scope: Some(HarnessScope::Local),
        reference: json!({}),
        arguments: json!({}),
        metadata: json!({ "source": "refine_heuristic" }),
        source: "refine".into(),
        created_at: now,
        updated_at: now,
        version: 1,
    };

    RefineProposal {
        id: pending.proposal_id.clone(),
        trigger: instructions.to_string(),
        summary: format!("Record refine instruction as memory: {title}"),
        evidence: format!("User/agent refine instructions: {content}"),
        expected_outcome: format!("Harness memory `{mem_id}` captures the refine focus."),
        edits: vec![RefineEdit {
            op: RefineEditOp::Create,
            kind: HarnessKind::Memory,
            id: mem_id,
            before: None,
            after: Some(entry),
        }],
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

/// Apply pending refine at the turn boundary (heuristic planner).
///
/// Used by unit tests; production post-turn uses
/// [`ServerRuntime::apply_pending_refine_at_boundary`].
#[cfg(test)]
pub(crate) fn apply_pending_refine_at_boundary(
    session_id: &devo_protocol::native::ids::SessionId,
    session_dir: &Path,
    plan_mode: bool,
) -> Option<AppliedRefine> {
    apply_pending_refine_with_proposal(
        session_id,
        session_dir,
        plan_mode,
        plan_refine_proposal,
    )
}

#[cfg(test)]
fn apply_pending_refine_with_proposal(
    session_id: &devo_protocol::native::ids::SessionId,
    session_dir: &Path,
    plan_mode: bool,
    plan: impl FnOnce(&PendingRefine, &Path) -> RefineProposal,
) -> Option<AppliedRefine> {
    let pending = take_pending_refine(session_id)?;
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

    let proposal = plan(&pending, &harness_path);
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

impl ServerRuntime {
    /// Apply pending refine with LLM planning (`UsagePurpose::Refine`) and
    /// heuristic fallback.
    pub(crate) async fn apply_pending_refine_at_boundary(
        self: &Arc<Self>,
        session_id: SessionId,
        native_session_id: &devo_protocol::native::ids::SessionId,
        session_dir: &Path,
        plan_mode: bool,
    ) -> Option<AppliedRefine> {
        let pending = take_pending_refine(native_session_id)?;
        if plan_mode && pending.autonomous {
            PENDING_REFINES
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(native_session_id.as_str().to_string(), pending);
            return None;
        }
        let harness_path = match ensure_session_harness(session_dir) {
            Ok(path) => path,
            Err(error) => {
                tracing::warn!(%error, "refine apply skipped: harness unavailable");
                return None;
            }
        };
        let proposal = self
            .plan_refine_proposal_llm(session_id, &pending, &harness_path)
            .await;
        // Re-insert was already consumed; apply directly.
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
        let legacy_id = params.session_id;
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
            let session_dir = handle.rollout_path().await.flatten().and_then(|path| {
                crate::persistence::RolloutStore::rlm_session_dir_for_rollout(&path)
            });
            (is_root, session_dir)
        };
        let result =
            schedule_refine_run(&params.session_id, &params, is_root, session_dir.as_deref());
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
        let Some(rollout_path) = handle.rollout_path().await.flatten() else {
            return;
        };
        let resume_snapshot = handle.resume_snapshot().await;
        let native_session_id = if let Some(snap) = resume_snapshot.as_ref() {
            snap.summary.native.id
        } else if let Some(session) = handle.native_session().await {
            session.id
        } else {
            // boundary: session actor has no native session projection
            session_id
        };
        let native_turn_id = resume_snapshot
            .and_then(|snap| snap.latest_turn.map(|turn| turn.native.id))
            .unwrap_or_else(|| {
                // boundary: no RuntimeTurn on actor when refinement applied
                TurnId::new()
            });
        let seq = handle.allocate_item_seq().await.unwrap_or(0);
        let now = Utc::now();
        let envelope = ItemEnvelope {
            id: devo_protocol::native::ids::ItemId::new(),
            session_id: native_session_id,
            turn_id: native_turn_id,
            seq,
            revision: 1,
            created_at: now,
            updated_at: now,
            state: ItemState::Completed,
            item: applied.native_item(),
            parent_id: None,
        };
        if let Err(error) = self
            .rollout_store
            .append_canonical_item_at(&rollout_path, envelope)
        {
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
            session_id: id,
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
        let id =
            devo_protocol::native::ids::SessionId::from_string(format!("ses_{}", Uuid::new_v4()));
        let params = SessionRefineRunParams {
            session_id: id,
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
        let id =
            devo_protocol::native::ids::SessionId::from_string(format!("ses_{}", Uuid::new_v4()));
        let settings = AutoRefineSettings {
            enabled: true,
            turn_interval: 2,
            ..AutoRefineSettings::default()
        };
        assert!(!maybe_schedule_auto_refine(
            &id, &settings, true, false, true, None
        ));
        assert!(!peek_pending_refine(&id));
        assert!(maybe_schedule_auto_refine(
            &id, &settings, true, false, true, None
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
        let id =
            devo_protocol::native::ids::SessionId::from_string(format!("ses_{}", Uuid::new_v4()));
        let settings = AutoRefineSettings {
            turn_interval: 1,
            ..AutoRefineSettings::default()
        };
        assert!(!maybe_schedule_auto_refine(
            &id, &settings, false, false, true, None
        ));
        assert!(!peek_pending_refine(&id));
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: boundary apply writes refinements.jsonl (never mid-ipython).
    #[test]
    fn apply_at_boundary_persists_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let id =
            devo_protocol::native::ids::SessionId::from_string(format!("ses_{}", Uuid::new_v4()));
        let params = SessionRefineRunParams {
            session_id: id,
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
        let id =
            devo_protocol::native::ids::SessionId::from_string(format!("ses_{}", Uuid::new_v4()));
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

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: plan_refine_proposal with instructions yields a memory Create edit.
    #[test]
    fn plan_refine_proposal_with_instructions_emits_memory_create() {
        let dir = tempfile::tempdir().unwrap();
        let path = HarnessState::file_path(dir.path());
        HarnessState::default().save_atomic(&path).unwrap();
        let pending = PendingRefine {
            proposal_id: "refine_test".into(),
            instructions: Some("  focus on harness digest wiring  ".into()),
            global: false,
            rollback_id: None,
            requested_at: Utc::now(),
            autonomous: false,
        };
        let proposal = plan_refine_proposal(&pending, &path);
        assert_eq!(proposal.id, "refine_test");
        assert!(!proposal.edits.is_empty());
        assert_eq!(proposal.edits.len(), 1);
        let edit = &proposal.edits[0];
        assert_eq!(edit.op, RefineEditOp::Create);
        assert_eq!(edit.kind, HarnessKind::Memory);
        assert!(edit.id.starts_with("mem_refine_"));
        let after = edit.after.as_ref().expect("after");
        assert!(after.content.contains("harness digest"));
        assert!(proposal.summary.contains("harness digest"));
        assert!(proposal.evidence.contains("harness digest"));
        assert!(proposal.expected_outcome.contains(&edit.id));
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: empty/missing instructions keep the no-op empty-edits proposal.
    #[test]
    fn plan_refine_proposal_without_instructions_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let path = HarnessState::file_path(dir.path());
        HarnessState::default().save_atomic(&path).unwrap();
        let pending = PendingRefine {
            proposal_id: "refine_empty".into(),
            instructions: Some("   ".into()),
            global: false,
            rollback_id: None,
            requested_at: Utc::now(),
            autonomous: false,
        };
        let proposal = plan_refine_proposal(&pending, &path);
        assert!(proposal.edits.is_empty());
        let pending_none = PendingRefine {
            instructions: None,
            ..pending
        };
        assert!(plan_refine_proposal(&pending_none, &path).edits.is_empty());
    }
}
