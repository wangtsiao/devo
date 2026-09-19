//! Ask-wired host bridge for kernel `host_request` mutating actions.
//!
//! Owns the session/turn capability context (`PermissionChecker`, FS, MCP) and
//! runs bash/write/edit/mcp/web through the same authorize → execute path as tools.
//! Goal / agent_message actions upgrade a [`Weak`][`std::sync::Weak`] to
//! [`ServerRuntime`] when available.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use super::ServerRuntime;
use devo_core::McpManager;
use devo_core::tools::contracts::ToolBudgets;
use devo_core::tools::handlers::{
    EditHandler, QuestionHandler, ShellCommandHandler, WebFetchHandler, WebSearchHandler,
    WriteHandler,
};
use devo_core::tools::{
    ClientFilesystem, FileReadLedger, PermissionChecker, SandboxPermissionRequest, ToolAgentScope,
    ToolCallError, ToolCallId, ToolContext, ToolHandler, ToolPermissionRequest, ToolResult,
    ToolResultContent, ToolTerminalStatus,
};
use devo_protocol::CollaborationMode;
use devo_protocol::InputModality;
use devo_protocol::native::ids::{SessionId, TurnId};
use devo_safety::ResourceKind;

const LOCAL_WEB_SEARCH_CONFIG_KEY: &str = "__devo_local_web_search";

/// Per-turn host capability context shared by kernel `host_request` handlers.
pub(crate) struct HostBridge {
    pub session_id: SessionId,
    pub turn_id: Option<TurnId>,
    pub cwd: PathBuf,
    /// Session artifact directory (parent of rollout JSONL); harness local store root.
    pub session_dir: Option<PathBuf>,
    pub collaboration_mode: CollaborationMode,
    pub permission: PermissionChecker,
    pub client_filesystem: Option<Arc<dyn ClientFilesystem>>,
    pub file_read_ledger: Arc<FileReadLedger>,
    pub sandbox_profile: Option<String>,
    pub cancel_token: CancellationToken,
    pub mcp_manager: Option<Arc<dyn McpManager>>,
    pub agent_scope: ToolAgentScope,
    pub local_web_search: Option<Value>,
    pub network_proxy: Option<String>,
    pub network_no_proxy: Option<String>,
    /// Weak to avoid Arc cycles (runtime → session → kernel → handler → runtime).
    pub runtime: Weak<ServerRuntime>,
    /// Current turn model id (wire / catalog id) for `model.info`.
    pub model_id: Option<String>,
    /// Input modalities for `model.info` / attach-image vision gate.
    pub input_modalities: Vec<InputModality>,
    pub context_window: Option<u32>,
}

impl HostBridge {
    /// Minimal always-allow bridge for unit tests / session-id-only handlers.
    pub(crate) fn for_tests(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            turn_id: None,
            cwd: std::env::temp_dir(),
            session_dir: None,
            collaboration_mode: CollaborationMode::Build,
            permission: PermissionChecker::always_allow(),
            client_filesystem: None,
            file_read_ledger: Arc::new(FileReadLedger::new()),
            sandbox_profile: None,
            cancel_token: CancellationToken::new(),
            mcp_manager: None,
            agent_scope: ToolAgentScope::Parent,
            local_web_search: None,
            network_proxy: None,
            network_no_proxy: None,
            runtime: Weak::new(),
            model_id: None,
            input_modalities: vec![InputModality::Text],
            context_window: None,
        }
    }

    pub(crate) fn runtime(&self) -> Option<Arc<ServerRuntime>> {
        self.runtime.upgrade()
    }

    /// Payload for kernel `host_request("model.info")` (attach-image vision gate).
    pub(crate) fn model_info_payload(&self) -> Value {
        let mut input = Vec::new();
        if self.input_modalities.is_empty()
            || self.input_modalities.contains(&InputModality::Text)
        {
            input.push("text");
        }
        let vision = self.input_modalities.contains(&InputModality::Image);
        if vision {
            input.push("image");
        }
        json!({
            "id": self.model_id.clone().unwrap_or_else(|| "unknown".into()),
            "input": input,
            "vision": vision,
            "context_window": self.context_window,
        })
    }
}

/// Build a [`devo_kernel::HostRequestHandler`] bound to a live [`HostBridge`].
pub fn host_handler_for_bridge(bridge: Arc<HostBridge>) -> devo_kernel::HostRequestHandler {
    Arc::new(move |_req_id, data| {
        let bridge = Arc::clone(&bridge);
        Box::pin(async move {
            let reply =
                crate::runtime::kernel_host::dispatch_host_request_with_bridge(&bridge, data).await;
            crate::runtime::kernel_host::ensure_host_reply_envelope(reply)
        })
    })
}

pub(crate) fn tool_context_from_bridge(bridge: &HostBridge, tool_call_id: String) -> ToolContext {
    ToolContext {
        output_store: None,
        tool_call_id: ToolCallId(tool_call_id),
        session_id: bridge.session_id,
        turn_id: bridge.turn_id,
        workspace_root: bridge.cwd.clone(),
        budgets: ToolBudgets {
            output_limit_bytes: 32 * 1024,
            wall_time_limit_ms: Some(6_000),
        },
        cancel_token: bridge.cancel_token.clone(),
        agent_scope: bridge.agent_scope,
        collaboration_mode: bridge.collaboration_mode,
        agent_coordinator: bridge
            .runtime()
            .map(|rt| rt as Arc<dyn devo_core::tools::AgentToolCoordinator>),
        client_filesystem: bridge.client_filesystem.clone(),
        file_read_ledger: Some(Arc::clone(&bridge.file_read_ledger)),
        network_proxy: bridge.network_proxy.clone(),
        network_no_proxy: bridge.network_no_proxy.clone(),
        sandbox_profile: bridge.sandbox_profile.clone(),
        sandbox_permission_overlay: None,
        kernel: None,
        python_cell_first_wait_ms: None,
        python_cell_watch: None,
        python_cell_completion: None,
        session_dir: bridge.session_dir.clone(),
    }
}

fn plan_mode_denies_mutation(bridge: &HostBridge) -> bool {
    matches!(bridge.collaboration_mode, CollaborationMode::Plan)
}

fn deny_plan_mutation(action: &str) -> Value {
    json!({
        "status": "error",
        "error": format!("denied: {action} blocked in Plan mode"),
    })
}

fn error_reply(msg: impl Into<String>) -> Value {
    json!({ "status": "error", "error": msg.into() })
}

fn synthetic_tool_call_id(prefix: &str) -> String {
    format!("host_{prefix}_{}", Uuid::new_v4())
}

#[allow(clippy::too_many_arguments)]
fn permission_request(
    bridge: &HostBridge,
    tool_call_id: String,
    tool_name: &str,
    input: Value,
    resource: ResourceKind,
    action_summary: String,
    path: Option<PathBuf>,
    target: Option<String>,
) -> ToolPermissionRequest {
    permission_request_with_host(
        bridge,
        tool_call_id,
        tool_name,
        input,
        resource,
        action_summary,
        path,
        /*host*/ None,
        target,
    )
}

#[allow(clippy::too_many_arguments)]
fn permission_request_with_host(
    bridge: &HostBridge,
    tool_call_id: String,
    tool_name: &str,
    input: Value,
    resource: ResourceKind,
    action_summary: String,
    path: Option<PathBuf>,
    host: Option<String>,
    target: Option<String>,
) -> ToolPermissionRequest {
    ToolPermissionRequest {
        tool_call_id,
        tool_name: tool_name.to_string(),
        input,
        cwd: bridge.cwd.clone(),
        session_id: bridge.session_id,
        turn_id: bridge.turn_id,
        resource,
        action_summary,
        justification: None,
        path,
        host,
        target,
        command_prefix: None,
        command_argv: None,
        command_pattern: None,
        sandbox_permissions: SandboxPermissionRequest::Default,
    }
}

fn host_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    let rest = url.split("://").nth(1).unwrap_or(url);
    let hostport = rest.split('/').next()?.split('@').next_back()?;
    let host = hostport.split(':').next()?.trim();
    (!host.is_empty()).then(|| host.to_string())
}

async fn check_or_error(bridge: &HostBridge, req: ToolPermissionRequest) -> Result<(), Value> {
    match bridge.permission.check(req).await {
        Ok(_) => Ok(()),
        Err(err) => Err(error_reply(err)),
    }
}

fn path_from_write_input(cwd: &Path, input: &Value) -> Option<PathBuf> {
    let raw = input
        .get("filePath")
        .or_else(|| input.get("path"))
        .or_else(|| input.get("file_path"))
        .and_then(|v| v.as_str())?;
    let path = PathBuf::from(raw);
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

fn tool_result_to_reply(result: ToolResult) -> Value {
    let (status, error_msg) = match &result.structured_status {
        ToolTerminalStatus::Completed => ("ok", None),
        ToolTerminalStatus::Denied { reason } | ToolTerminalStatus::BlockedByMode { reason } => {
            ("error", Some(reason.clone()))
        }
        ToolTerminalStatus::NeedsConfiguration { message } => ("error", Some(message.clone())),
        ToolTerminalStatus::InvalidInput { details } => ("error", Some(details.clone())),
        ToolTerminalStatus::Failed(err) => ("error", Some(err.to_string())),
        ToolTerminalStatus::Canceled | ToolTerminalStatus::Interrupted => {
            ("error", Some("cancelled".to_string()))
        }
    };
    let mut body = json!({
        "status": status,
        "summary": result.result_summary,
    });
    if let Some(msg) = error_msg {
        body["error"] = json!(msg);
    }
    if let Some(display) = result.display_content {
        body["display"] = json!(display);
    }
    match result.content {
        ToolResultContent::Text(text) => {
            body["stdout"] = json!(text);
        }
        ToolResultContent::Json(value) => {
            body["result"] = value;
        }
        ToolResultContent::Mixed { text, json: meta } => {
            if let Some(text) = text {
                body["stdout"] = json!(text);
            }
            if let Some(meta) = meta {
                if let Some(obj) = meta.as_object() {
                    for (key, value) in obj {
                        if body.get(key).is_none() {
                            body[key] = value.clone();
                        }
                    }
                } else {
                    body["result"] = meta;
                }
            }
        }
    }
    body
}

fn map_tool_outcome(result: Result<ToolResult, ToolCallError>) -> Value {
    match result {
        Ok(tool_result) => tool_result_to_reply(tool_result),
        Err(err) => error_reply(err.to_string()),
    }
}

pub(crate) async fn handle_bash(bridge: &HostBridge, params: &Value) -> Value {
    if plan_mode_denies_mutation(bridge) {
        return deny_plan_mutation("bash");
    }
    let command = params
        .get("command")
        .or_else(|| params.get("cmd"))
        .cloned()
        .unwrap_or(Value::Null);
    let command_str = command.as_str().unwrap_or("").to_string();
    if command_str.is_empty() {
        return error_reply("missing bash command");
    }
    let mut input = params.clone();
    if input.get("command").is_none() {
        input["command"] = json!(command_str.clone());
    }
    let tool_call_id = synthetic_tool_call_id("bash");
    let req = permission_request(
        bridge,
        tool_call_id.clone(),
        "shell_command",
        input.clone(),
        ResourceKind::ShellExec,
        format!("Run shell command: {command_str}"),
        None,
        Some(command_str),
    );
    if let Err(reply) = check_or_error(bridge, req).await {
        return reply;
    }
    let ctx = tool_context_from_bridge(bridge, tool_call_id);
    map_tool_outcome(ShellCommandHandler::new().handle(ctx, input, None).await)
}

pub(crate) async fn handle_write(bridge: &HostBridge, params: &Value) -> Value {
    if plan_mode_denies_mutation(bridge) {
        return deny_plan_mutation("write");
    }
    let input = params.clone();
    let path = path_from_write_input(&bridge.cwd, &input);
    let tool_call_id = synthetic_tool_call_id("write");
    let summary = path
        .as_ref()
        .map(|p| format!("Write file {}", p.display()))
        .unwrap_or_else(|| "Write file".to_string());
    let req = permission_request(
        bridge,
        tool_call_id.clone(),
        "write",
        input.clone(),
        ResourceKind::FileWrite,
        summary,
        path,
        None,
    );
    if let Err(reply) = check_or_error(bridge, req).await {
        return reply;
    }
    let ctx = tool_context_from_bridge(bridge, tool_call_id);
    map_tool_outcome(WriteHandler::new().handle(ctx, input, None).await)
}

pub(crate) async fn handle_edit(bridge: &HostBridge, params: &Value) -> Value {
    if plan_mode_denies_mutation(bridge) {
        return deny_plan_mutation("edit");
    }
    let input = params.clone();
    let path = path_from_write_input(&bridge.cwd, &input);
    let tool_call_id = synthetic_tool_call_id("edit");
    let summary = path
        .as_ref()
        .map(|p| format!("Edit file {}", p.display()))
        .unwrap_or_else(|| "Edit file".to_string());
    let req = permission_request(
        bridge,
        tool_call_id.clone(),
        "edit",
        input.clone(),
        ResourceKind::FileWrite,
        summary,
        path,
        None,
    );
    if let Err(reply) = check_or_error(bridge, req).await {
        return reply;
    }
    let ctx = tool_context_from_bridge(bridge, tool_call_id);
    map_tool_outcome(EditHandler::new().handle(ctx, input, None).await)
}

/// Front-door single-file read (design doc §6.1/§7): the Python facade tries a
/// direct `open()` first and only reaches this handler when the OS fence
/// refused (or the caller asked explicitly). Reads stay allowed in Plan mode.
/// The approval target is the canonicalized absolute path; a 1 MiB cap keeps
/// the mediated reply bounded.
pub(crate) async fn handle_fs_read(bridge: &HostBridge, params: &Value) -> Value {
    const MAX_READ_BYTES: u64 = 1024 * 1024;
    let Some(path_str) = params.get("path").and_then(Value::as_str) else {
        return error_reply("missing fs.read path");
    };
    let raw = Path::new(path_str);
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        bridge.cwd.join(raw)
    };
    let canonical = match std::fs::canonicalize(&joined) {
        Ok(p) => p,
        Err(err) => {
            return error_reply(format!(
                "fs.read {}: {err} (path not readable or missing)",
                joined.display()
            ))
        }
    };
    let tool_call_id = synthetic_tool_call_id("fs.read");
    let req = permission_request(
        bridge,
        tool_call_id,
        "read",
        json!({ "path": canonical.to_string_lossy() }),
        ResourceKind::FileRead,
        format!("Read file {}", canonical.display()),
        Some(canonical.clone()),
        None,
    );
    if let Err(reply) = check_or_error(bridge, req).await {
        return reply;
    }
    match std::fs::metadata(&canonical) {
        Ok(meta) if meta.len() > MAX_READ_BYTES => {
            return error_reply(format!(
                "fs.read {}: file is {} bytes; mediated read limit is {MAX_READ_BYTES} bytes",
                canonical.display(),
                meta.len()
            ));
        }
        Err(err) => {
            return error_reply(format!("fs.read {}: {err}", canonical.display()));
        }
        _ => {}
    }
    match std::fs::read(&canonical) {
        Ok(bytes) => {
            let n = bytes.len();
            let content = String::from_utf8(bytes)
                .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            json!({
                "status": "ok",
                "result": { "content": content, "bytes": n },
            })
        }
        Err(err) => error_reply(format!("fs.read {}: {err}", canonical.display())),
    }
}

pub(crate) async fn handle_mcp_call(bridge: &HostBridge, params: &Value) -> Value {
    if plan_mode_denies_mutation(bridge) {
        return deny_plan_mutation("mcp.call");
    }
    let Some(manager) = bridge.mcp_manager.as_ref() else {
        return error_reply("mcp manager not available on this host bridge");
    };
    let server = params
        .get("server")
        .or_else(|| params.get("serverId"))
        .or_else(|| params.get("server_id"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let tool_name = params
        .get("tool")
        .or_else(|| params.get("toolName"))
        .or_else(|| params.get("name"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    if server.is_empty() || tool_name.is_empty() {
        return error_reply("mcp.call requires server and tool");
    }
    let input = params
        .get("arguments")
        .or_else(|| params.get("input"))
        .or_else(|| params.get("args"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    let tool_call_id = synthetic_tool_call_id("mcp");
    let req = permission_request(
        bridge,
        tool_call_id,
        "mcp.call",
        json!({
            "server": server,
            "tool": tool_name,
            "arguments": input,
        }),
        ResourceKind::Custom("mcp.call".into()),
        format!("Call MCP tool {server}/{tool_name}"),
        None,
        Some(format!("{server}/{tool_name}")),
    );
    if let Err(reply) = check_or_error(bridge, req).await {
        return reply;
    }
    let server_id = devo_core::McpServerId(server.to_string());
    match manager.invoke_tool(&server_id, tool_name, input).await {
        Ok(result) => json!({
            "status": "ok",
            "result": result,
        }),
        Err(err) => error_reply(err.to_string()),
    }
}

pub(crate) async fn handle_mcp_list_tools(bridge: &HostBridge, _params: &Value) -> Value {
    // Listing is read-only; skip Ask.
    let Some(manager) = bridge.mcp_manager.as_ref() else {
        return error_reply("mcp manager not available on this host bridge");
    };
    match manager.discover_tools().await {
        Ok(tools) => {
            let items: Vec<Value> = tools
                .into_iter()
                .map(|info| {
                    json!({
                        "server": info.server_id.0,
                        "serverDisplayName": info.server_display_name,
                        "name": info.raw_tool_name,
                        "flatName": info.flat_name,
                        "description": info.description,
                        "readOnly": info.read_only_hint,
                    })
                })
                .collect();
            json!({ "status": "ok", "tools": items })
        }
        Err(err) => error_reply(err.to_string()),
    }
}

pub(crate) async fn handle_web_search(bridge: &HostBridge, params: &Value) -> Value {
    let query = params
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if query.is_empty() {
        return error_reply("web.search requires query");
    }
    let Some(config) = bridge.local_web_search.as_ref() else {
        return error_reply("local web_search provider is not configured for this turn");
    };
    let mut input = params.clone();
    input["query"] = json!(query.clone());
    input[LOCAL_WEB_SEARCH_CONFIG_KEY] = config.clone();
    let tool_call_id = synthetic_tool_call_id("web_search");
    let req = permission_request_with_host(
        bridge,
        tool_call_id.clone(),
        "web_search",
        input.clone(),
        ResourceKind::Network,
        format!("Web search: {query}"),
        None,
        Some("web_search".to_string()),
        Some(query),
    );
    if let Err(reply) = check_or_error(bridge, req).await {
        return reply;
    }
    let ctx = tool_context_from_bridge(bridge, tool_call_id);
    map_tool_outcome(WebSearchHandler::new().handle(ctx, input, None).await)
}

pub(crate) async fn handle_web_fetch(bridge: &HostBridge, params: &Value) -> Value {
    let url = params
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if url.is_empty() {
        return error_reply("web.fetch requires url");
    }
    let input = params.clone();
    let tool_call_id = synthetic_tool_call_id("webfetch");
    let host = host_from_url(&url);
    let req = permission_request_with_host(
        bridge,
        tool_call_id.clone(),
        "webfetch",
        input.clone(),
        ResourceKind::Network,
        format!("Web fetch: {url}"),
        None,
        host,
        Some(url),
    );
    if let Err(reply) = check_or_error(bridge, req).await {
        return reply;
    }
    let ctx = tool_context_from_bridge(bridge, tool_call_id);
    map_tool_outcome(WebFetchHandler::new().handle(ctx, input, None).await)
}

pub(crate) async fn handle_question(bridge: &HostBridge, params: &Value) -> Value {
    let tool_call_id = synthetic_tool_call_id("question");
    let ctx = tool_context_from_bridge(bridge, tool_call_id);
    if ctx.turn_id.is_none() {
        return error_reply("question requires an active turn");
    }
    if ctx.agent_coordinator.is_none() {
        return error_reply("question requires a live runtime (userInput reverse-RPC)");
    }
    map_tool_outcome(
        QuestionHandler::new()
            .handle(ctx, params.clone(), None)
            .await,
    )
}
