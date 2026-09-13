//! Kernel `host_request` dispatch table (P2).
//!
//! Maps action names from the CPython REPL to host behavior. Ask-gated actions
//! return structured pending/error payloads until reverse-RPC approval is fully
//! wired; schedule actions reuse existing refine/compact façades.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, LazyLock, Mutex};

use serde_json::{json, Value};
use uuid::Uuid;

use crate::runtime::compact_host::{
    compact_status_from_occupancy, compact_status_json, peek_pending_compact, schedule_compact_run,
};
use crate::runtime::refine::peek_pending_refine;
use devo_protocol::native::ids::SessionId;
use devo_protocol::native::rpc_session::SessionRefineRunParams;

/// In-memory bash completion / consumed notices keyed by session id.
static BASH_NOTICES: LazyLock<Mutex<HashMap<String, VecDeque<Value>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Dispatch one host_request payload. `data` is the kernel event `data` object.
pub async fn dispatch_host_request(session_id: &str, data: Value) -> Value {
    let action = data
        .get("type")
        .or_else(|| data.get("action"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let params = data.get("params").cloned().unwrap_or_else(|| {
        let mut obj = data.as_object().cloned().unwrap_or_default();
        obj.remove("action");
        obj.remove("type");
        Value::Object(obj)
    });

    match action.as_str() {
        "" => error_reply("missing host_request type/action"),
        "rlm.create_session" => {
            error_reply("denied: rlm.create_session (daemon out of scope this release)")
        }
        a if a.starts_with("rlm_heartbeat") => {
            error_reply(format!("denied: {action} (out of scope this release)"))
        }
        "rlm.run" => {
            let handle = format!("child_{}", Uuid::new_v4());
            json!({
                "status": "ok",
                "pending": true,
                "handle": {
                    "sessionId": handle,
                    "note": "child session spawn stub; Native agent APIs own the real path"
                }
            })
        }
        "rlm.find_models" | "rlm.list_subagents" | "rlm.delete_subagent" => {
            json!({ "status": "ok", "items": [] })
        }
        "compact.status" => {
            let sid = parse_session_id(session_id);
            let scheduled = sid
                .as_ref()
                .map(peek_pending_compact)
                .unwrap_or(false);
            // Occupancy is filled when the turn bridge supplies last usage;
            // percent may be null until then (Prime-compatible).
            let status =
                compact_status_from_occupancy(None, scheduled, /*usage_known*/ false);
            let mut body = compact_status_json(&status);
            body["status"] = json!("ok");
            body
        }
        "compact.run" => {
            let Some(sid) = parse_session_id(session_id) else {
                return error_reply("invalid session id");
            };
            let instructions = params
                .get("instructions")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            let result = schedule_compact_run(&sid, instructions, /*turn_active*/ true);
            json!({
                "status": if result.scheduled { "ok" } else { "error" },
                "scheduled": result.scheduled,
                "note": result.note,
                "reason": result.reason,
            })
        }
        "refine.run" => {
            let Some(sid) = parse_session_id(session_id) else {
                return error_reply("invalid session id");
            };
            let rpc_params = SessionRefineRunParams {
                session_id: sid.clone(),
                instructions: params
                    .get("instructions")
                    .and_then(|v| v.as_str())
                    .map(str::to_string),
                global: params
                    .get("global")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                rollback_id: None,
            };
            let result = crate::runtime::refine::schedule_refine_run(
                &sid,
                &rpc_params,
                /*is_root*/ true,
                None,
            );
            json!({
                "status": if result.scheduled { "ok" } else { "error" },
                "scheduled": result.scheduled,
                "note": result.note,
                "reason": result.reason,
            })
        }
        "refine.status" => {
            let Some(sid) = parse_session_id(session_id) else {
                return error_reply("invalid session id");
            };
            json!({
                "status": "ok",
                "pending": peek_pending_refine(&sid),
                "inFlight": false,
            })
        }
        "goal.get" => {
            json!({
                "status": "ok",
                "goal": null,
                "note": "goal.get reads session/goal/read via host bridge (stub null)"
            })
        }
        "goal.create" => {
            json!({
                "status": "error",
                "error": "goal.create → session/goal/set pending full host bridge"
            })
        }
        "goal.complete" => {
            json!({
                "status": "error",
                "error": "goal.complete → session/goal/complete pending full host bridge"
            })
        }
        "bash" | "bash.start" => {
            let command = params
                .get("command")
                .or_else(|| params.get("cmd"))
                .cloned()
                .unwrap_or(Value::Null);
            json!({
                "status": "pending",
                "ask_required": true,
                "pending": {
                    "kind": "bash",
                    "command": command,
                    "note": "Ask preflight via authorize_tool_request not yet connected"
                }
            })
        }
        "bash.completed" => {
            push_bash_notice(session_id, json!({
                "kind": "async_bash_completion",
                "params": params,
            }));
            json!({ "status": "ok", "accepted": true })
        }
        "bash.consumed" => {
            let _ = take_bash_notices(session_id);
            json!({ "status": "ok", "accepted": true })
        }
        "write" | "edit" => {
            json!({
                "status": "pending",
                "ask_required": true,
                "pending": {
                    "kind": action,
                    "note": "authorize + apply_patch / file_write under Ask not yet connected"
                }
            })
        }
        "web.search" | "web.fetch" => {
            json!({
                "status": "pending",
                "ask_required": true,
                "pending": {
                    "kind": action,
                    "note": "network Ask + existing adapters pending host bridge"
                }
            })
        }
        "mcp.call" | "mcp.list_tools" => {
            json!({
                "status": "pending",
                "ask_required": true,
                "pending": {
                    "kind": action,
                    "note": "Rust MCP under Ask; Python-side MCP denied"
                }
            })
        }
        "mcp.config" | "mcp.refresh" => {
            json!({ "status": "ok", "servers": [] })
        }
        "model.info" => {
            json!({
                "status": "ok",
                "vision": false,
                "context_window": null
            })
        }
        "question" => {
            json!({
                "status": "pending",
                "ask_required": true,
                "pending": {
                    "kind": "question",
                    "note": "maps to Native userInput/request"
                }
            })
        }
        "agent_message.send" => {
            json!({
                "status": "error",
                "error": "agent_message.send family routing pending; use Native agent/message"
            })
        }
        "agent_observe.list" | "agent_observe.get" | "agent_observe.recent" => {
            json!({
                "status": "ok",
                "agents": [],
                "messages": [],
                "note": "family reach stub (parent/child)"
            })
        }
        other => error_reply(format!("unknown host_request action: {other}")),
    }
}

/// Build a [`devo_kernel::HostRequestHandler`] bound to `session_id`.
pub fn host_handler_for_session(session_id: String) -> devo_kernel::HostRequestHandler {
    Arc::new(move |_req_id, data| {
        let session_id = session_id.clone();
        Box::pin(async move { dispatch_host_request(&session_id, data).await })
    })
}

fn push_bash_notice(session_id: &str, notice: Value) {
    BASH_NOTICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session_id.to_string())
        .or_default()
        .push_back(notice);
}

#[allow(dead_code)] // wake-notice consumer on turn continue
pub(crate) fn take_bash_notices(session_id: &str) -> Vec<Value> {
    BASH_NOTICES
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id)
        .map(|q| q.into_iter().collect())
        .unwrap_or_default()
}

fn parse_session_id(session_id: &str) -> Option<SessionId> {
    session_id.parse().ok()
}

fn error_reply(msg: impl Into<String>) -> Value {
    json!({ "status": "error", "error": msg.into() })
}

#[cfg(test)]
mod tests {
    use pretty_assertions::assert_eq;
    use serde_json::json;

    use super::*;

    /// Trace: L2-DES-RLM-001
    /// Verifies: rlm.create_session is denied.
    #[tokio::test]
    async fn denies_create_session() {
        let reply = dispatch_host_request(
            "ses_00000000-0000-0000-0000-000000000001",
            json!({ "type": "rlm.create_session" }),
        )
        .await;
        assert_eq!(reply["status"], "error");
        assert!(
            reply["error"].as_str().unwrap_or("").contains("denied"),
            "{reply}"
        );
    }

    /// Trace: L2-DES-CONTEXT-002, L2-DES-RLM-001
    /// Verifies: compact.status returns shape with percent field (null ok).
    #[tokio::test]
    async fn compact_status_shape_includes_percent() {
        let reply = dispatch_host_request(
            "ses_00000000-0000-0000-0000-000000000001",
            json!({ "type": "compact.status" }),
        )
        .await;
        assert_eq!(reply["status"], "ok");
        assert!(reply.get("scheduled").is_some(), "{reply}");
        assert!(
            reply.get("percent").is_some(),
            "percent key required (null ok): {reply}"
        );
        assert!(reply["percent"].is_null(), "{reply}");
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: bash.completed enqueues an in-memory notice.
    #[tokio::test]
    async fn bash_completed_enqueues_notice() {
        let sid = format!("ses_{}", Uuid::new_v4());
        let reply = dispatch_host_request(
            &sid,
            json!({
                "type": "bash.completed",
                "params": { "exitCode": 0 }
            }),
        )
        .await;
        assert_eq!(reply["status"], "ok");
        let notices = take_bash_notices(&sid);
        assert_eq!(notices.len(), 1);
        assert_eq!(notices[0]["kind"], "async_bash_completion");
    }
}
