//! LLM Continual Harness refine planner (`UsagePurpose::Refine`).
//!
//! Builds a JSON proposal via the session model; callers fall back to the
//! heuristic planner on parse/provider failure. Auxiliary model slots are
//! shared (cap 1) with title polish per L2-DES-RLM-001 DD-15.

use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde::Deserialize;
use serde_json::json;

use super::refine::{PendingRefine, plan_refine_proposal};
use super::*;
use crate::json_extract::extract_json_object;
use devo_harness::{
    HarnessEntry, HarnessKind, HarnessScope, HarnessState, RefineEdit, RefineEditOp, RefineProposal,
};
use devo_protocol::{
    ModelRequest, RequestContent, RequestMessage, ResponseContent, SamplingControls,
};

const REFINE_MAX_TOKENS: usize = 2048;
const REFINE_AUX_TIMEOUT: Duration = Duration::from_secs(45);

const REFINE_JSON_SHAPE: &str = r#"{"summary":"string","evidence":"string","expected_outcome":"string","edits":[{"op":"create|update|delete","kind":"prompt|memory|skill|subagent","id":"stable_id","after":{"id":"same","kind":"memory","title":"string","content":"string"}}]}"#;

const REFINE_SYSTEM_PROMPT: &str = "You plan Continual Harness refinements. Reply with JSON only. Prefer small memory Create edits that capture durable guidance from the instructions. Never edit id base_system_prompt. Never grant tools, MCP, or sandbox privileges via skill/subagent edits.";

pub(crate) fn build_refine_plan_request(
    model: String,
    pending: &PendingRefine,
    harness_snapshot: &str,
) -> ModelRequest {
    let instructions = pending
        .instructions
        .as_deref()
        .unwrap_or("(auto-refine: improve harness from recent trajectory)")
        .trim();
    let user = format!(
        "Proposal id: {}\nAutonomous: {}\nInstructions:\n{}\n\nCurrent harness JSON:\n{}\n\nReturn a RefineProposal JSON body (no markdown fences).",
        pending.proposal_id, pending.autonomous, instructions, harness_snapshot
    );
    ModelRequest {
        model_slug: devo_protocol::ModelProfileKey::Generic,
        model,
        system: Some(format!(
            "{REFINE_SYSTEM_PROMPT} JSON shape: {REFINE_JSON_SHAPE}."
        )),
        messages: vec![RequestMessage {
            role: "user".to_string(),
            content: vec![RequestContent::Text { text: user }],
        }],
        max_tokens: REFINE_MAX_TOKENS,
        tools: None,
        hosted_tools: Vec::new(),
        sampling: SamplingControls {
            temperature: Some(0.0),
            ..SamplingControls::default()
        },
        request_thinking: None,
        reasoning_effort: None,
        extra_body: None,
    }
}

#[derive(Debug, Deserialize)]
struct LlmRefineProposal {
    summary: Option<String>,
    evidence: Option<String>,
    expected_outcome: Option<String>,
    #[serde(default)]
    edits: Vec<LlmRefineEdit>,
}

#[derive(Debug, Deserialize)]
struct LlmRefineEdit {
    op: String,
    kind: String,
    id: String,
    #[serde(default)]
    after: Option<LlmHarnessEntry>,
}

#[derive(Debug, Deserialize)]
struct LlmHarnessEntry {
    id: Option<String>,
    kind: Option<String>,
    title: Option<String>,
    content: Option<String>,
    path: Option<String>,
    scope: Option<String>,
}

fn parse_kind(raw: &str) -> Option<HarnessKind> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "prompt" => Some(HarnessKind::Prompt),
        "memory" => Some(HarnessKind::Memory),
        "skill" => Some(HarnessKind::Skill),
        "subagent" => Some(HarnessKind::Subagent),
        _ => None,
    }
}

fn parse_op(raw: &str) -> Option<RefineEditOp> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "create" => Some(RefineEditOp::Create),
        "update" => Some(RefineEditOp::Update),
        "delete" => Some(RefineEditOp::Delete),
        _ => None,
    }
}

fn parse_scope(raw: Option<&str>) -> Option<HarnessScope> {
    match raw.map(|s| s.trim().to_ascii_lowercase()).as_deref() {
        Some("global") => Some(HarnessScope::Global),
        Some("local") => Some(HarnessScope::Local),
        _ => Some(HarnessScope::Local),
    }
}

fn entry_from_llm(kind: HarnessKind, id: &str, raw: &LlmHarnessEntry) -> HarnessEntry {
    let now = Utc::now();
    HarnessEntry {
        id: raw.id.clone().unwrap_or_else(|| id.to_string()),
        kind: raw.kind.as_deref().and_then(parse_kind).unwrap_or(kind),
        title: raw.title.clone().unwrap_or_else(|| id.to_string()),
        content: raw.content.clone().unwrap_or_default(),
        path: raw.path.clone(),
        scope: parse_scope(raw.scope.as_deref()),
        reference: json!({}),
        arguments: json!({}),
        metadata: json!({ "source": "refine_llm" }),
        source: "refine".into(),
        created_at: now,
        updated_at: now,
        version: 1,
    }
}

/// Parse model text into a [`RefineProposal`], filling ids from `pending`.
pub(crate) fn parse_refine_proposal_text(
    pending: &PendingRefine,
    raw: &str,
) -> Option<RefineProposal> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed: LlmRefineProposal = serde_json::from_str(trimmed)
        .ok()
        .or_else(|| extract_json_object(trimmed).and_then(|obj| serde_json::from_str(obj).ok()))?;
    let mut edits = Vec::new();
    for edit in parsed.edits {
        let op = parse_op(&edit.op)?;
        let kind = parse_kind(&edit.kind)?;
        if edit.id == "base_system_prompt" {
            return None;
        }
        let after = match op {
            RefineEditOp::Delete => None,
            RefineEditOp::Create | RefineEditOp::Update => {
                let raw_after = edit.after.as_ref()?;
                Some(entry_from_llm(kind, &edit.id, raw_after))
            }
        };
        edits.push(RefineEdit {
            op,
            kind,
            id: edit.id,
            before: None,
            after,
        });
    }
    let trigger = pending.instructions.clone().unwrap_or_else(|| {
        if pending.autonomous {
            "auto".into()
        } else {
            "manual".into()
        }
    });
    Some(RefineProposal {
        id: pending.proposal_id.clone(),
        trigger,
        summary: parsed
            .summary
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "LLM refine proposal".into()),
        evidence: parsed.evidence.unwrap_or_default(),
        expected_outcome: parsed.expected_outcome.unwrap_or_default(),
        edits,
    })
}

fn response_text(content: &[ResponseContent]) -> String {
    let mut combined = String::new();
    for block in content {
        if let ResponseContent::Text(text) = block {
            combined.push_str(text);
            combined.push('\n');
        }
    }
    combined
}

fn harness_snapshot_json(path: &std::path::Path) -> String {
    match HarnessState::load(path) {
        Ok(state) => serde_json::to_string_pretty(&state).unwrap_or_else(|_| "{}".into()),
        Err(_) => "{}".into(),
    }
}

impl ServerRuntime {
    /// Plan a refine proposal with the session model (`UsagePurpose::Refine`).
    /// Falls back to the heuristic planner on any failure.
    pub(crate) async fn plan_refine_proposal_llm(
        self: &Arc<Self>,
        session_id: SessionId,
        pending: &PendingRefine,
        harness_path: &std::path::Path,
    ) -> RefineProposal {
        match self
            .try_plan_refine_proposal_llm(session_id, pending, harness_path)
            .await
        {
            Some(proposal) if !proposal.edits.is_empty() || pending.instructions.is_none() => {
                proposal
            }
            Some(proposal) if proposal.edits.is_empty() && pending.instructions.is_some() => {
                tracing::debug!("refine LLM returned empty edits; using heuristic");
                plan_refine_proposal(pending, harness_path)
            }
            Some(proposal) => proposal,
            None => plan_refine_proposal(pending, harness_path),
        }
    }

    async fn try_plan_refine_proposal_llm(
        self: &Arc<Self>,
        session_id: SessionId,
        pending: &PendingRefine,
        harness_path: &std::path::Path,
    ) -> Option<RefineProposal> {
        let _permit = match tokio::time::timeout(
            Duration::from_millis(50),
            self.auxiliary_model_slots.acquire(),
        )
        .await
        {
            Ok(Ok(permit)) => permit,
            Ok(Err(_)) => {
                tracing::debug!("refine LLM skipped: auxiliary semaphore closed");
                return None;
            }
            Err(_) => {
                tracing::debug!("refine LLM skipped: auxiliary slot busy");
                return None;
            }
        };

        let session_handle = self.session(session_id).await?;
        let title_context = session_handle.title_generation_context().await?;
        let primary_selection = title_context
            .model_selection
            .unwrap_or_else(|| title_context.runtime_context.default_model.clone());
        let runtime_context = title_context.runtime_context;
        let turn_config = runtime_context.resolve_turn_config(
            Some(primary_selection.as_str()),
            title_context.reasoning_effort_selection.clone(),
        );
        let resolved_request = turn_config
            .model
            .resolve_reasoning_effort_selection(turn_config.reasoning_effort_selection.as_deref());
        let catalog_request_model = resolved_request.request_model.clone();
        let request_model = turn_config.provider_request_model(&catalog_request_model);
        let provider = self.usage_ledger.instrumented_provider(
            runtime_context.provider_for_route(turn_config.provider_route.clone()),
            session_id,
            None,
            devo_protocol::native::usage::UsagePurpose::Refine,
        );
        let snapshot = harness_snapshot_json(harness_path);
        let model_request = build_refine_plan_request(request_model, pending, &snapshot);
        let response = match tokio::time::timeout(
            REFINE_AUX_TIMEOUT,
            provider.completion(model_request),
        )
        .await
        {
            Ok(Ok(response)) => response,
            Ok(Err(error)) => {
                tracing::warn!(%error, "refine LLM completion failed");
                return None;
            }
            Err(_) => {
                tracing::warn!("refine LLM completion timed out");
                return None;
            }
        };
        let text = response_text(&response.content);
        parse_refine_proposal_text(pending, &text).or_else(|| {
            tracing::warn!("refine LLM response was not a valid proposal");
            None
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn pending(instructions: Option<&str>) -> PendingRefine {
        PendingRefine {
            proposal_id: "refine_parse".into(),
            instructions: instructions.map(str::to_string),
            global: false,
            rollback_id: None,
            requested_at: Utc::now(),
            autonomous: false,
        }
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: LLM JSON parses into a memory Create RefineProposal.
    #[test]
    fn parse_refine_proposal_text_memory_create() {
        let raw = r#"{
          "summary": "Remember digest wiring",
          "evidence": "user asked",
          "expected_outcome": "memory stored",
          "edits": [{
            "op": "create",
            "kind": "memory",
            "id": "mem_digest",
            "after": {
              "id": "mem_digest",
              "kind": "memory",
              "title": "Digest",
              "content": "Wire harness digest"
            }
          }]
        }"#;
        let proposal =
            parse_refine_proposal_text(&pending(Some("focus digest")), raw).expect("parsed");
        assert_eq!(proposal.id, "refine_parse");
        assert_eq!(proposal.edits.len(), 1);
        assert_eq!(proposal.edits[0].op, RefineEditOp::Create);
        assert_eq!(proposal.edits[0].kind, HarnessKind::Memory);
        assert_eq!(
            proposal.edits[0].after.as_ref().map(|e| e.content.as_str()),
            Some("Wire harness digest")
        );
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: base_system_prompt edits are rejected.
    #[test]
    fn parse_rejects_immutable_base_prompt() {
        let raw = r#"{
          "summary": "bad",
          "edits": [{
            "op": "update",
            "kind": "prompt",
            "id": "base_system_prompt",
            "after": { "title": "x", "content": "y" }
          }]
        }"#;
        assert!(parse_refine_proposal_text(&pending(Some("x")), raw).is_none());
    }

    /// Trace: L2-DES-HARNESS-001
    /// Verifies: fenced / noisy model output still extracts JSON.
    #[test]
    fn parse_extracts_json_object_from_noise() {
        let raw = "Here you go:\n```json\n{\"summary\":\"ok\",\"edits\":[]}\n```\n";
        let proposal = parse_refine_proposal_text(&pending(None), raw).expect("parsed");
        assert_eq!(proposal.summary, "ok");
        assert!(proposal.edits.is_empty());
    }
}
