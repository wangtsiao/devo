//! Round-trip property tests (P3a): legacy `RolloutLine` →
//! `project_legacy_line` (`RolloutWriteState`) → v2 JSON →
//! `parse_rollout_line` → Native replay record adapters → legacy replay
//! records must remain equivalent to the original.
//!
//! Equivalence is full-object equality after `normalize_expected` applies
//! the documented allow-list of unrecoverable fields (see the comments in
//! `native_replay.rs` and the normalize function below) and reshapes packed
//! records the way the one-record-one-item v2 writer does.

use std::collections::HashMap;
use std::path::PathBuf;

use chrono::{DateTime, TimeZone, Utc};
use devo_core::{
    ApprovalDecisionItem, ApprovalRequestItem, CommandExecutionItem, ItemId, ItemLine, ItemRecord,
    NativeReplayError, ParsedRolloutLine, RolloutLine, RolloutLineReadError, RolloutLineV2,
    RolloutWriteState, SessionRecord, SessionTitleFinalSource, SessionTitleState, TextItem,
    ToolCallItem, ToolResultItem, TurnItem, TurnKind, TurnRecord, TurnStatus,
    item_record_from_native, legacy_compaction_line_from_native, legacy_lines_from_internal,
    legacy_rollback_line_from_native, legacy_title_line_from_native, parse_rollout_line,
    project_legacy_line, session_record_from_native, turn_record_from_native,
};
use pretty_assertions::assert_eq;
use uuid::Uuid;

/// Marker id for positions where the round-trip legitimately produces a
/// fresh random id (approval decision records, synthesized internal-entry
/// records, expanded packed payloads). The comparator skips id equality
/// there.
fn sentinel_id() -> ItemId {
    ItemId::from_legacy_uuid(Uuid::nil())
}

fn ts(second: u32) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 1, 12, 0, second).unwrap()
}

fn fixture_lines(name: &str) -> Vec<RolloutLine> {
    let path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("tests/fixtures/rollout_v1/{name}"));
    std::fs::read_to_string(&path)
        .expect("read fixture")
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| {
                panic!("fixture {name} legacy line must deserialize as RolloutLine: {e}")
            })
        })
        .collect()
}

/// The full round-trip: forward project, serialize to v2 JSONL, re-parse
/// through the version dispatch (proving the v2 output is well-formed), then
/// map back into the retained replay records.
fn round_trip(lines: &[RolloutLine]) -> Vec<RolloutLine> {
    let mut forward = RolloutWriteState::new();
    let mut out = Vec::new();
    for line in lines {
        let v2_lines = project_legacy_line(&mut forward, line).expect("forward projection");
        for v2 in &v2_lines {
            let raw = serde_json::to_string(v2).expect("serialize v2 line");
            let ParsedRolloutLine::V2(parsed) =
                parse_rollout_line(&raw).expect("v2 line re-parses");
            match *parsed {
                RolloutLineV2::SessionMeta {
                    timestamp,
                    session,
                    extras,
                    ..
                } => out.push(RolloutLine::SessionMeta(Box::new(
                    devo_core::SessionMetaLine {
                        timestamp,
                        session: session_record_from_native(&session, extras.as_deref())
                            .expect("session replay mapping"),
                    },
                ))),
                RolloutLineV2::Turn {
                    timestamp,
                    turn,
                    extras,
                    ..
                } => out.push(RolloutLine::Turn(Box::new(devo_core::TurnLine {
                    timestamp,
                    turn: turn_record_from_native(&turn, extras.as_deref())
                        .expect("turn replay mapping"),
                }))),
                RolloutLineV2::Item { item, .. } => {
                    if let Some(item) = item_record_from_native(&item).expect("item replay mapping")
                    {
                        out.push(RolloutLine::Item(Box::new(ItemLine {
                            timestamp: item.timestamp,
                            item,
                        })));
                    }
                }
                RolloutLineV2::Internal {
                    timestamp,
                    session_id,
                    turn_id,
                    seq,
                    entry,
                    ..
                } => out.extend(
                    legacy_lines_from_internal(
                        timestamp,
                        &session_id,
                        turn_id.as_ref(),
                        seq,
                        &entry,
                    )
                    .expect("internal inverse projection"),
                ),
                RolloutLineV2::SessionTitleUpdated {
                    timestamp,
                    session_id,
                    title,
                    previous_title,
                    ..
                } => out.push(
                    legacy_title_line_from_native(timestamp, &session_id, title, previous_title)
                        .expect("title inverse projection"),
                ),
                RolloutLineV2::CompactionSnapshot {
                    timestamp,
                    session_id,
                    turn_id,
                    summary_item_id,
                    preserved_item_ids,
                    context_occupancy,
                    ..
                } => out.push(
                    legacy_compaction_line_from_native(
                        timestamp,
                        &session_id,
                        &turn_id,
                        &summary_item_id,
                        &preserved_item_ids,
                        context_occupancy,
                    )
                    .expect("compaction inverse projection"),
                ),
                RolloutLineV2::SessionRollback {
                    timestamp,
                    session_id,
                    retained_turn_ids,
                    retained_item_ids,
                    latest_turn_id,
                    ..
                } => out.push(
                    legacy_rollback_line_from_native(
                        timestamp,
                        &session_id,
                        &retained_turn_ids,
                        &retained_item_ids,
                        latest_turn_id.as_ref(),
                    )
                    .expect("rollback inverse projection"),
                ),
                RolloutLineV2::WorkspaceCheckpoint { .. }
                | RolloutLineV2::WorkspaceChange { .. }
                | RolloutLineV2::WorkspaceRestoreStarted { .. }
                | RolloutLineV2::WorkspaceRestoreCompleted { .. } => {}
            }
        }
    }
    out
}

// ── Expected-shape normalization (the explicit allow-list) ─────────────

struct Normalizer {
    next_seq: u64,
    approval_seqs: HashMap<String, u64>,
}

impl Normalizer {
    fn new() -> Self {
        Self {
            next_seq: 1,
            approval_seqs: HashMap::new(),
        }
    }

    fn normalize(&mut self, lines: &[RolloutLine]) -> Vec<RolloutLine> {
        let mut out = Vec::new();
        for line in lines {
            match line {
                RolloutLine::SessionMeta(line) => {
                    out.push(RolloutLine::SessionMeta(Box::new(
                        devo_core::SessionMetaLine {
                            timestamp: line.timestamp,
                            session: normalize_session(&line.session),
                        },
                    )));
                }
                RolloutLine::Turn(line) => {
                    out.push(RolloutLine::Turn(Box::new(devo_core::TurnLine {
                        timestamp: line.timestamp,
                        turn: normalize_turn(&line.turn),
                    })));
                }
                RolloutLine::Item(line) => self.normalize_item_record(line, &mut out),
                RolloutLine::SessionTitleUpdated(line) => {
                    out.push(RolloutLine::SessionTitleUpdated(
                        devo_core::SessionTitleUpdatedLine {
                            title_state: SessionTitleState::Final(
                                SessionTitleFinalSource::ExplicitCreate,
                            ),
                            ..line.clone()
                        },
                    ));
                }
                other => out.push(other.clone()),
            }
        }
        out
    }

    /// Expands one packed legacy record into the one-payload-per-record
    /// shape the v2 writer produces, applying the item-level allow-list:
    ///
    /// - internal payloads (HookPrompt/ToolProgress/TurnSummary) become
    ///   synthesized records with an unstable id and the seq of the position
    ///   where they appeared;
    /// - approval decisions fold onto their request's id/seq in v2, so the
    ///   inverse decision record gets an unstable id, the request's seq, and
    ///   normalized decision/scope strings;
    /// - orphan decisions (Warning in v2) are dropped;
    /// - `ToolResult.tool_name` and `CommandExecution.tool_name` do not
    ///   survive the canonical variants;
    /// - `attempt_placement`/`turn_status` are not modeled on the canonical
    ///   envelope, and the inverse stamps the current schema version.
    fn normalize_item_record(&mut self, line: &ItemLine, out: &mut Vec<RolloutLine>) {
        let record = &line.item;
        for (index, payload) in record
            .input_items
            .iter()
            .chain(&record.output_items)
            .enumerate()
        {
            // Payloads after the first get a fresh bare-UUID id in v2; their
            // round-tripped id is unstable.
            let stable_id = (index == 0).then_some(record.id);
            match payload {
                TurnItem::HookPrompt(_) | TurnItem::ToolProgress(_) | TurnItem::TurnSummary(_) => {
                    // Internal entries consume one sequence position in v2
                    // and the inverse restores it verbatim; the record id
                    // is still synthesized (internal entries have no item
                    // id), so it stays unstable.
                    let seq = self.next_seq;
                    self.next_seq += 1;
                    out.push(item_line(sentinel_id(), record, seq, payload.clone()));
                }
                TurnItem::ApprovalRequest(request) => {
                    let seq = self.next_seq;
                    self.next_seq += 1;
                    self.approval_seqs.insert(request.approval_id.clone(), seq);
                    out.push(item_line(
                        stable_id.unwrap_or_else(sentinel_id),
                        record,
                        seq,
                        payload.clone(),
                    ));
                }
                TurnItem::ApprovalDecision(decision) => {
                    // The Warning a v2 orphan decision becomes is skipped by
                    // the inverse: no line survives, but the seq counter
                    // still advanced.
                    let Some(seq) = self.approval_seqs.get(&decision.approval_id) else {
                        self.next_seq += 1;
                        continue;
                    };
                    out.push(item_line(
                        sentinel_id(),
                        record,
                        *seq,
                        TurnItem::ApprovalDecision(ApprovalDecisionItem {
                            approval_id: decision.approval_id.clone(),
                            decision: normalize_decision_string(&decision.decision),
                            scope: normalize_scope_string(&decision.scope),
                            decision_source: decision.decision_source,
                        }),
                    ));
                }
                payload => {
                    let seq = self.next_seq;
                    self.next_seq += 1;
                    out.push(item_line(
                        stable_id.unwrap_or_else(sentinel_id),
                        record,
                        seq,
                        normalize_payload(payload),
                    ));
                }
            }
        }
    }
}

fn item_line(id: ItemId, original: &ItemRecord, seq: u64, payload: TurnItem) -> RolloutLine {
    RolloutLine::Item(Box::new(ItemLine {
        timestamp: original.timestamp,
        item: ItemRecord {
            id,
            session_id: original.session_id,
            turn_id: original.turn_id,
            seq,
            timestamp: original.timestamp,
            started_at: original.started_at,
            attempt_placement: None,
            turn_status: None,
            sibling_turn_ids: Vec::new(),
            input_items: Vec::new(),
            output_items: vec![payload],
            worklog: None,
            error: None,
            schema_version: 1,
        },
    }))
}

fn normalize_payload(payload: &TurnItem) -> TurnItem {
    match payload {
        // The canonical result variants do not carry the legacy tool name.
        TurnItem::ToolResult(result) => TurnItem::ToolResult(ToolResultItem {
            tool_name: None,
            ..result.clone()
        }),
        TurnItem::CommandExecution(command) => TurnItem::CommandExecution(CommandExecutionItem {
            tool_name: "exec_command".into(),
            ..command.clone()
        }),
        other => other.clone(),
    }
}

fn normalize_decision_string(decision: &str) -> String {
    match decision.to_ascii_lowercase().as_str() {
        "approve" | "approved" | "allow" => "approve",
        "deny" | "denied" => "deny",
        _ => "cancel",
    }
    .into()
}

fn normalize_scope_string(scope: &str) -> String {
    match scope.to_ascii_lowercase().as_str() {
        "once" => "once",
        "turn" => "turn",
        "session" => "session",
        "path_prefix" => "path_prefix",
        "host" => "host",
        "tool" => "tool",
        "command_prefix" => "command_prefix",
        "command_prefix_persist" => "command_prefix_persist",
        _ => "once",
    }
    .into()
}

/// SessionRecord fields the canonical model does not carry (the allow-list,
/// each with its justification in `native_replay.rs`).
fn normalize_session(session: &SessionRecord) -> SessionRecord {
    let last_activity_at = session.last_activity_at.unwrap_or(session.updated_at);
    SessionRecord {
        rollout_path: PathBuf::new(),
        updated_at: last_activity_at,
        last_activity_at: Some(last_activity_at),
        agent_nickname: None,
        agent_path: None,
        model_binding_id: None,
        title_state: if session.title.is_some() {
            SessionTitleState::Final(SessionTitleFinalSource::ExplicitCreate)
        } else {
            SessionTitleState::Unset
        },
        approval_mode: {
            let mode = session.approval_mode.to_ascii_lowercase();
            if mode.contains("auto") {
                "auto-review".into()
            } else if mode.contains("full") {
                "full-access".into()
            } else {
                "on-request".into()
            }
        },
        archived_at: session.archived_at.map(|_| session.created_at),
        latest_turn_context: None,
        collaboration_mode: None,
        permission_preset: None,
        schema_version: 2,
        ..session.clone()
    }
}

/// TurnRecord fields the canonical model does not carry.
///
/// Aggregate `usage` is replaced by `latest_query_usage` when that field is
/// present: the forward projector prefers the latest query for canonical
/// `Turn.usage`, so the inverse cannot recover a distinct aggregate.
///
/// Zero optional cache/reasoning counters become `None` after Native migrate
/// round-trip (`to_native` / `from_native`).
fn normalize_usage(usage: &devo_core::TurnUsage) -> devo_core::TurnUsage {
    devo_core::TurnUsage {
        cache_creation_input_tokens: usage.cache_creation_input_tokens.filter(|v| *v > 0),
        cache_read_input_tokens: usage.cache_read_input_tokens.filter(|v| *v > 0),
        reasoning_output_tokens: usage.reasoning_output_tokens.filter(|v| *v > 0),
        total_tokens: Some(
            usage
                .total_tokens
                .unwrap_or(usage.input_tokens + usage.output_tokens),
        ),
        ..usage.clone()
    }
}

fn normalize_turn(turn: &TurnRecord) -> TurnRecord {
    let usage_source = turn.latest_query_usage.as_ref().or(turn.usage.as_ref());
    TurnRecord {
        status: match turn.status.clone() {
            TurnStatus::Pending | TurnStatus::Running | TurnStatus::WaitingApproval => {
                TurnStatus::Running
            }
            status => status,
        },
        kind: match &turn.kind {
            TurnKind::Regular | TurnKind::Review | TurnKind::Other(_) => TurnKind::Regular,
            TurnKind::ManualCompaction => TurnKind::ManualCompaction,
        },
        model: if turn.request_model.is_empty() {
            turn.model.clone()
        } else {
            turn.request_model.clone()
        },
        usage: usage_source.map(normalize_usage),
        latest_query_usage: turn.latest_query_usage.as_ref().map(normalize_usage),
        schema_version: 4,
        ..turn.clone()
    }
}

// ── Comparison with unstable-id wildcards ───────────────────────────────

fn assert_lines_equivalent(actual: &[RolloutLine], expected: &[RolloutLine]) {
    assert_eq!(actual.len(), expected.len(), "line count mismatch");
    for (index, (actual, expected)) in actual.iter().zip(expected).enumerate() {
        let context = format!("line {index}");
        match (actual, expected) {
            (RolloutLine::Item(actual), RolloutLine::Item(expected)) => {
                assert_eq!(actual.timestamp, expected.timestamp, "{context}: timestamp");
                // Align the id so unstable positions (sentinel) compare equal;
                // every other field must match exactly.
                let actual_aligned = ItemRecord {
                    id: expected.item.id,
                    ..actual.item.clone()
                };
                assert_eq!(&actual_aligned, &expected.item, "{context}: record");
            }
            _ => assert_eq!(actual, expected, "{context}"),
        }
    }
}

// ── Tests ───────────────────────────────────────────────────────────────

#[test]
fn basic_session_fixture_round_trips_through_v2() {
    let original = fixture_lines("basic_session.jsonl");
    let expected = Normalizer::new().normalize(&original);
    assert_lines_equivalent(&round_trip(&original), &expected);
}

#[test]
fn internal_lines_fixture_round_trips_through_v2() {
    let original = fixture_lines("internal_lines.jsonl");
    let expected = Normalizer::new().normalize(&original);
    assert_lines_equivalent(&round_trip(&original), &expected);
}

#[test]
fn orphan_decision_fixture_round_trips_with_warning_dropped() {
    let original = fixture_lines("orphan_decision.jsonl");
    let expected = Normalizer::new().normalize(&original);
    let actual = round_trip(&original);
    // The orphan decision became a v2 Warning, which the inverse skips.
    assert!(
        actual
            .iter()
            .all(|line| !matches!(line, RolloutLine::Item(line) if line.item.output_items.iter().any(|item| matches!(item, TurnItem::ApprovalDecision(_)))))
    );
    assert_lines_equivalent(&actual, &expected);
}

/// Live-write shapes: one payload per record in `output_items`, the approval
/// pair, contexts in the extras, and a hook prompt.
fn live_write_lines() -> Vec<RolloutLine> {
    let session_id = devo_core::SessionId::new();
    let turn_id = devo_core::TurnId::new();
    let item = |seq: u64, payload: TurnItem| {
        RolloutLine::Item(Box::new(ItemLine {
            timestamp: ts(10 + seq as u32),
            item: ItemRecord {
                id: ItemId::new(),
                session_id,
                turn_id,
                seq,
                timestamp: ts(10 + seq as u32),
                started_at: None,
                attempt_placement: None,
                turn_status: Some(TurnStatus::Running),
                sibling_turn_ids: Vec::new(),
                input_items: Vec::new(),
                output_items: vec![payload],
                worklog: None,
                error: None,
                schema_version: 1,
            },
        }))
    };
    vec![
        RolloutLine::SessionMeta(Box::new(devo_core::SessionMetaLine {
            timestamp: ts(0),
            session: SessionRecord {
                id: session_id,
                rollout_path: "rollout.jsonl".into(),
                created_at: ts(0),
                updated_at: ts(1),
                last_activity_at: Some(ts(1)),
                source: "cli".into(),
                agent_nickname: None,
                agent_role: None,
                agent_path: None,
                model_provider: "openai".into(),
                model: Some("gpt-5.2".into()),
                model_binding_id: None,
                reasoning_effort_selection: Some("medium".into()),
                cwd: "/tmp/live".into(),
                additional_directories: vec!["/tmp/live-extra".into()],
                cli_version: "0.1.37".into(),
                title: None,
                title_state: SessionTitleState::Unset,
                sandbox_policy: "workspace-write".into(),
                approval_mode: "on-request".into(),
                effective_context_window: None,
                tokens_used: 0,
                first_user_message: None,
                archived_at: None,
                git_sha: None,
                git_branch: None,
                git_origin_url: None,
                parent_session_id: None,
                fork_from_id: None,
                fork_at_turn_id: None,
                session_context: None,
                latest_turn_context: None,
                collaboration_mode: None,
                permission_preset: None,
                schema_version: 2,
            },
        })),
        RolloutLine::Turn(Box::new(devo_core::TurnLine {
            timestamp: ts(2),
            turn: TurnRecord {
                id: turn_id,
                session_id,
                sequence: 1,
                started_at: ts(2),
                completed_at: None,
                status: TurnStatus::Running,
                kind: TurnKind::Regular,
                model: "gpt-5.2".into(),
                model_binding_id: Some("binding-1".into()),
                reasoning_effort_selection: Some("medium".into()),
                request_model: "gpt-5.2".into(),
                request_thinking: None,
                input_token_estimate: None,
                usage: None,
                latest_query_usage: None,
                context_occupancy: None,
                stop_reason: None,
                failure_reason: None,
                error: None,
                session_context: None,
                turn_context: None,
                schema_version: 4,
            },
        })),
        item(1, TurnItem::UserMessage(TextItem::text("hello"))),
        item(
            2,
            TurnItem::ToolCall(ToolCallItem {
                tool_call_id: "call-1".into(),
                tool_name: "exec_command".into(),
                input: serde_json::json!({ "command": "ls" }),
            }),
        ),
        item(
            3,
            TurnItem::ApprovalRequest(ApprovalRequestItem {
                approval_id: "appr-1".into(),
                action_summary: "Run ls".into(),
                justification: "listing".into(),
                resource: Some("ShellExec".into()),
                available_scopes: vec!["once".into()],
                command_pattern: Some(vec!["ls".into()]),
                command_prefix: Some(vec!["ls".into()]),
                path: Some("/tmp/live".into()),
                host: None,
                target: None,
            }),
        ),
        item(
            4,
            TurnItem::ApprovalDecision(ApprovalDecisionItem {
                approval_id: "appr-1".into(),
                decision: "approve".into(),
                scope: "once".into(),
                decision_source: None,
            }),
        ),
        item(5, TurnItem::HookPrompt(TextItem::text("hook"))),
    ]
}

#[test]
fn live_write_shapes_round_trips_through_v2() {
    let original = live_write_lines();
    let expected = Normalizer::new().normalize(&original);
    assert_lines_equivalent(&round_trip(&original), &expected);
}

/// Trace: L2-DES-RLM-001
/// Verifies: missing `v` lines are rejected (no dual-read).
#[test]
fn pre_rlm_legacy_lines_are_rejected() {
    let original = fixture_lines("basic_session.jsonl");
    let raw = serde_json::to_string(&original[0]).expect("serialize legacy");
    let err = parse_rollout_line(&raw).expect_err("legacy must fail");
    assert_eq!(err, RolloutLineReadError::LegacyUnsupported);
}

/// Trace: L2-DES-RLM-001
/// Verifies: prefixed native opaque ids (`ses_`/`turn_`/`item_`) round-trip
/// through native→legacy replay by stripping the prefix.
#[test]
fn native_replay_accepts_prefixed_canonical_ids() {
    let session_id = devo_protocol::native::ids::SessionId::new();
    assert!(
        session_id.as_str().starts_with("ses_"),
        "fixture must use a prefixed opaque id"
    );
    let line = RolloutLineV2::SessionMeta {
        v: devo_core::ROLLOUT_FORMAT_VERSION,
        timestamp: ts(0),
        session: Box::new(devo_protocol::native::session::Session {
            id: session_id,
            version: 1,
            cwd: PathBuf::from("/tmp"),
            additional_directories: Vec::new(),
            parent: None,
            fork_from_id: None,
            at_turn_id: None,
            ephemeral: false,
            created_at: ts(0),
            status: devo_protocol::native::session::SessionStatus::Idle,
            flags: Vec::new(),
            archived: false,
            activity: devo_protocol::native::session::SessionActivity::Idle,
            active_turn_id: None,
            queued_count: 0,
            title: None,
            title_state: SessionTitleState::Unset,
            model: devo_protocol::native::model::ModelBinding {
                provider: "openai".into(),
                model: "gpt-5.2".into(),
                variant: None,
                reasoning_effort: None,
            },
            settings: devo_protocol::native::session::SessionSettings {
                permission_profile: devo_protocol::native::model::PermissionProfile::Default,
                reasoning_effort: None,
                mode: None,
                sandbox_profile: None,
                effective_context_window: None,
                auto_refine_enabled: None,
                auto_refine_turn_interval: None,
                python_cell_first_wait_ms: None,
            },
            git_info: None,
            preview: String::new(),
            last_activity_at: ts(0),
            transcript_size_bytes: None,
            message_count: None,
            summary: None,
            task_state: None,
            usage: devo_protocol::native::usage::SessionUsage {
                total: devo_protocol::native::usage::UsageTotals::default(),
                by_purpose: Vec::new(),
                legacy: None,
                updated_at: ts(0),
            },
        }),
        extras: None,
    };
    let RolloutLineV2::SessionMeta {
        session, extras, ..
    } = line
    else {
        unreachable!();
    };
    let record = session_record_from_native(&session, extras.as_deref())
        .expect("prefixed opaque ids must replay without conversion");
    assert_eq!(record.id, session_id);
}

#[test]
fn native_replay_rejects_turn_scoped_internal_line_without_turn_id() {
    let line = RolloutLineV2::Internal {
        v: devo_core::ROLLOUT_FORMAT_VERSION,
        timestamp: ts(0),
        session_id: devo_protocol::native::ids::SessionId::from_legacy_uuid(Uuid::nil()),
        turn_id: None,
        seq: 1,
        entry: devo_core::InternalRecordV2::Entry {
            entry: devo_protocol::native::item::InternalEntry::TurnSummary { text: "1".into() },
        },
    };
    let RolloutLineV2::Internal {
        timestamp,
        session_id,
        turn_id,
        seq,
        entry,
        ..
    } = line
    else {
        unreachable!();
    };
    let error = legacy_lines_from_internal(timestamp, &session_id, turn_id.as_ref(), seq, &entry)
        .expect_err("missing turn id");
    assert_eq!(error, NativeReplayError::MissingTurnId);
}

/// A toggle-keyword effort selection ("enabled") must survive the v1→v2→v1
/// round trip: the forward projection carries the raw selection into the
/// canonical settings snapshot, and the inverse prefers that snapshot over
/// the enum-typed `ModelBinding` slot, which cannot represent the keyword.
/// Both directions used to parse through the `ReasoningEffort` enum and
/// silently dropped it.
#[test]
fn toggle_effort_selection_round_trips_through_v2() {
    let with_toggle: Vec<RolloutLine> = live_write_lines()
        .into_iter()
        .map(|line| match line {
            RolloutLine::SessionMeta(mut meta) => {
                meta.session.reasoning_effort_selection = Some("enabled".into());
                RolloutLine::SessionMeta(meta)
            }
            other => other,
        })
        .collect();
    let expected = Normalizer::new().normalize(&with_toggle);
    assert_lines_equivalent(&round_trip(&with_toggle), &expected);

    for line in round_trip(&with_toggle) {
        if let RolloutLine::SessionMeta(meta) = line {
            assert_eq!(
                meta.session.reasoning_effort_selection.as_deref(),
                Some("enabled"),
                "session selection survives the round trip"
            );
        }
    }
}
