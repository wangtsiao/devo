//! Behavior test for L2-DES-CONV-002 Phase 3: tightening the permission
//! preset mid-turn takes effect at the next tool-call authorization. A
//! network-capable tool is allowed under `fullAccess` (yolo) but requires an
//! interactive approval under `default`; switching presets while the turn is
//! running must change the outcome of the *next* tool call.

use std::path::Path;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::Ordering;

use anyhow::Context;
use anyhow::Result;
use async_trait::async_trait;
use futures::StreamExt;
use futures::stream;
use pretty_assertions::assert_eq;
use serde_json::json;
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::sync::mpsc;
use tokio::time::Duration;
use tokio::time::timeout;

use devo_core::tools::ToolCallError;
use devo_core::tools::ToolRegistry;
use devo_core::tools::ToolResult;
use devo_core::tools::ToolResultContent;
use devo_core::tools::json_schema::JsonSchema;
use devo_core::tools::registry::ToolRegistryBuilder;
use devo_core::tools::tool_handler::ToolHandler;
use devo_core::tools::tool_spec::ToolCapabilityTag;
use devo_core::tools::tool_spec::ToolExecutionMode;
use devo_core::tools::tool_spec::ToolOutputMode;
use devo_core::tools::tool_spec::ToolSpec;
use devo_protocol::ModelRequest;
use devo_protocol::ModelResponse;
use devo_protocol::ResponseContent;
use devo_protocol::ResponseMetadata;
use devo_protocol::StopReason;
use devo_protocol::StreamEvent;
use devo_protocol::Usage;
use devo_provider::ModelProviderSDK;
use devo_server::ClientTransportKind;
use devo_server::ServerRuntime;
use devo_server::test_support::TestRuntime;

/// First stream request issues tool call `call-1`; the second waits for the
/// test to open the gate and then issues tool call `call-2`; later requests
/// end the turn with plain text.
struct TwoProbesProvider {
    stream_requests: Mutex<Vec<ModelRequest>>,
    go_second: Arc<Notify>,
}

/// Tool-call on request 1, plain-text done afterwards. The first stream is
/// gated so the test can patch settings before the next model request.
struct ToolThenDoneProvider {
    stream_requests: Mutex<Vec<ModelRequest>>,
    stream_started: Arc<Notify>,
    release_stream: Arc<Notify>,
}

#[async_trait]
impl ModelProviderSDK for TwoProbesProvider {
    async fn completion(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse {
            id: "title-1".into(),
            content: vec![ResponseContent::Text("Generated title".into())],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
            metadata: ResponseMetadata::default(),
        })
    }

    async fn completion_stream(
        &self,
        request: ModelRequest,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        let request_number = {
            let mut requests = self.stream_requests.lock().expect("stream request lock");
            requests.push(request);
            requests.len()
        };
        let tool_call_events = |id: &str, response_id: &str| {
            vec![
                Ok(StreamEvent::ToolCallStart {
                    index: 0,
                    id: id.into(),
                    name: "net_probe".into(),
                    input: json!({}),
                }),
                Ok(StreamEvent::ToolCallInputDelta {
                    index: 0,
                    partial_json: "{}".into(),
                }),
                Ok(StreamEvent::MessageDone {
                    response: ModelResponse {
                        id: response_id.into(),
                        content: vec![ResponseContent::ToolUse {
                            id: id.into(),
                            name: "net_probe".into(),
                            input: json!({}),
                        }],
                        stop_reason: Some(StopReason::ToolUse),
                        usage: Usage::default(),
                        metadata: ResponseMetadata::default(),
                    },
                }),
            ]
        };
        let done_events = || {
            vec![
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: "Done.".into(),
                }),
                Ok(StreamEvent::MessageDone {
                    response: ModelResponse {
                        id: "resp-done".into(),
                        content: vec![ResponseContent::Text("Done.".into())],
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Usage::default(),
                        metadata: ResponseMetadata::default(),
                    },
                }),
            ]
        };
        let stream: Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>> =
            match request_number {
                1 => Box::pin(stream::iter(tool_call_events("call-1", "resp-1"))),
                2 => {
                    let go_second = Arc::clone(&self.go_second);
                    let mut events = tool_call_events("call-2", "resp-2").into_iter();
                    let first = events.next().expect("tool call start event");
                    Box::pin(
                        stream::once(async move {
                            go_second.notified().await;
                            first
                        })
                        .chain(stream::iter(events.collect::<Vec<_>>())),
                    )
                }
                _ => Box::pin(stream::iter(done_events())),
            };
        Ok(stream)
    }

    fn name(&self) -> &str {
        "two-probes-provider"
    }
}

#[async_trait]
impl ModelProviderSDK for ToolThenDoneProvider {
    async fn completion(&self, _request: ModelRequest) -> Result<ModelResponse> {
        Ok(ModelResponse {
            id: "title-1".into(),
            content: vec![ResponseContent::Text("Generated title".into())],
            stop_reason: Some(StopReason::EndTurn),
            usage: Usage::default(),
            metadata: ResponseMetadata::default(),
        })
    }

    async fn completion_stream(
        &self,
        request: ModelRequest,
    ) -> Result<Pin<Box<dyn futures::Stream<Item = Result<StreamEvent>> + Send>>> {
        let request_number = {
            let mut requests = self.stream_requests.lock().expect("stream request lock");
            requests.push(request);
            requests.len()
        };
        let events = if request_number == 1 {
            vec![
                Ok(StreamEvent::ToolCallStart {
                    index: 0,
                    id: "call-1".into(),
                    name: "gated_probe".into(),
                    input: json!({}),
                }),
                Ok(StreamEvent::ToolCallInputDelta {
                    index: 0,
                    partial_json: "{}".into(),
                }),
                Ok(StreamEvent::MessageDone {
                    response: ModelResponse {
                        id: "resp-1".into(),
                        content: vec![ResponseContent::ToolUse {
                            id: "call-1".into(),
                            name: "gated_probe".into(),
                            input: json!({}),
                        }],
                        stop_reason: Some(StopReason::ToolUse),
                        usage: Usage::default(),
                        metadata: ResponseMetadata::default(),
                    },
                }),
            ]
        } else {
            vec![
                Ok(StreamEvent::TextDelta {
                    index: 0,
                    text: "Done.".into(),
                }),
                Ok(StreamEvent::MessageDone {
                    response: ModelResponse {
                        id: "resp-n".into(),
                        content: vec![ResponseContent::Text("Done.".into())],
                        stop_reason: Some(StopReason::EndTurn),
                        usage: Usage::default(),
                        metadata: ResponseMetadata::default(),
                    },
                }),
            ]
        };
        if request_number == 1 {
            let stream_started = Arc::clone(&self.stream_started);
            let release_stream = Arc::clone(&self.release_stream);
            let mut events = events.into_iter();
            let first = events.next().expect("first stream event");
            Ok(Box::pin(
                stream::once(async move {
                    stream_started.notify_one();
                    release_stream.notified().await;
                    first
                })
                .chain(stream::iter(events.collect::<Vec<_>>())),
            ))
        } else {
            Ok(Box::pin(stream::iter(events)))
        }
    }

    fn name(&self) -> &str {
        "tool-then-done-provider"
    }
}

/// Probe tool used to advance the query loop after the gated first stream.
struct GatedProbeTool;

#[async_trait]
impl ToolHandler for GatedProbeTool {
    fn spec(&self) -> &ToolSpec {
        Box::leak(Box::new(ToolSpec {
            name: "gated_probe".into(),
            description: "Blocks until released.".into(),
            input_schema: JsonSchema::object(Default::default(), None, None),
            output_mode: ToolOutputMode::Text,
            execution_mode: ToolExecutionMode::ReadOnly,
            capability_tags: vec![],
            supports_parallel: true,
            preparation_feedback: devo_core::tools::ToolPreparationFeedback::None,
            display_name: None,
            supports_cancellation: None,
            supports_streaming: None,
        }))
    }

    async fn handle(
        &self,
        _ctx: devo_core::tools::ToolContext,
        _input: serde_json::Value,
        _progress: Option<devo_core::tools::ToolProgressSender>,
    ) -> std::result::Result<ToolResult, ToolCallError> {
        Ok(ToolResult::success(
            ToolResultContent::Text("released".into()),
            "released",
        ))
    }
}

/// Network-capable probe tool: `ResourceKind::Network`, which the `default`
/// preset routes to an interactive approval and `fullAccess` auto-allows.
struct ProbeTool {
    invocations: Arc<AtomicUsize>,
}

#[async_trait]
impl ToolHandler for ProbeTool {
    fn spec(&self) -> &ToolSpec {
        Box::leak(Box::new(ToolSpec {
            name: "net_probe".into(),
            description: "Counts invocations.".into(),
            input_schema: JsonSchema::object(Default::default(), None, None),
            output_mode: ToolOutputMode::Text,
            execution_mode: ToolExecutionMode::ReadOnly,
            capability_tags: vec![ToolCapabilityTag::NetworkAccess],
            supports_parallel: true,
            preparation_feedback: devo_core::tools::ToolPreparationFeedback::None,
            display_name: None,
            supports_cancellation: None,
            supports_streaming: None,
        }))
    }

    async fn handle(
        &self,
        _ctx: devo_core::tools::ToolContext,
        _input: serde_json::Value,
        _progress: Option<devo_core::tools::ToolProgressSender>,
    ) -> std::result::Result<ToolResult, ToolCallError> {
        self.invocations.fetch_add(1, Ordering::SeqCst);
        Ok(ToolResult::success(
            ToolResultContent::Text("probed".into()),
            "probed",
        ))
    }
}

fn build_runtime(
    data_root: &Path,
    provider: Arc<dyn ModelProviderSDK>,
    registry: Arc<ToolRegistry>,
) -> Arc<ServerRuntime> {
    TestRuntime::new(provider)
        .registry(registry)
        .disabled_skills()
        .db_file("test_settings_mid_turn.db")
        .runtime(data_root)
}

async fn initialize_connection(
    runtime: &Arc<ServerRuntime>,
) -> Result<(u64, mpsc::Receiver<serde_json::Value>)> {
    let (notifications_tx, notifications_rx) = devo_server::test_outbound_channel(4096);
    let connection_id = runtime
        .register_connection(ClientTransportKind::Stdio, notifications_tx)
        .await;
    let initialize_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": 1,
                    "clientCapabilities": {},
                    "clientInfo": { "name": "test", "title": "test", "version": "1.0.0" },
                    "_meta": { "devo": { "protocol": "native" } }
                }
            }),
        )
        .await
        .context("initialize response")?;
    assert_eq!(
        initialize_response["result"]["agentInfo"]["name"],
        json!("devo-server")
    );
    Ok((connection_id, notifications_rx))
}

/// Trace: L2-DES-CONV-002
/// Verifies: tightening the permission preset mid-turn changes the next
/// tool-call authorization outcome — allowed under fullAccess, interactive
/// approval required after switching to default (Phase 3 behavior level).
#[tokio::test]
async fn mid_turn_tighten_to_default_triggers_approval_for_network_tool() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_root = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&workspace_root)?;

    let invocations = Arc::new(AtomicUsize::new(0));
    let go_second = Arc::new(Notify::new());
    let mut builder = ToolRegistryBuilder::new();
    builder.register_handler(
        "net_probe",
        Arc::new(ProbeTool {
            invocations: Arc::clone(&invocations),
        }),
    );
    builder.push_spec(ToolSpec {
        name: "net_probe".into(),
        description: "Counts invocations.".into(),
        input_schema: JsonSchema::object(Default::default(), None, None),
        output_mode: ToolOutputMode::Text,
        execution_mode: ToolExecutionMode::ReadOnly,
        capability_tags: vec![ToolCapabilityTag::NetworkAccess],
        supports_parallel: true,
        preparation_feedback: devo_core::tools::ToolPreparationFeedback::None,
        display_name: None,
        supports_cancellation: None,
        supports_streaming: None,
    });
    let provider = Arc::new(TwoProbesProvider {
        stream_requests: Mutex::new(Vec::new()),
        go_second: Arc::clone(&go_second),
    });
    let runtime = build_runtime(temp_dir.path(), provider, Arc::new(builder.build()));
    let (connection_id, mut notifications_rx) = initialize_connection(&runtime).await?;

    let session_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 2,
                "method": "session/new",
                "params": {
                    "cwd": workspace_root,
                    "idempotencyKey": "settings-mid-turn-1"
                }
            }),
        )
        .await
        .context("session/new response")?;
    let session_result: devo_protocol::native::rpc_session::SessionNewResult =
        serde_json::from_value(session_response["result"].clone())
            .with_context(|| format!("session/new response: {session_response}"))?;
    let session_id = session_result.session.id;

    let subscription_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 3,
                "method": "subscription/create",
                "params": {
                    "selectors": [{ "kind": "session", "sessionId": session_id.to_string() }],
                    "includeSnapshot": false
                }
            }),
        )
        .await
        .context("subscription/create response")?;
    assert!(
        subscription_response.get("error").is_none(),
        "subscription/create failed: {subscription_response}"
    );

    // Start permissive: the first probe must run without any approval.
    let settings_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 4,
                "method": "session/metadata/update",
                "params": {
                    "sessionId": session_id.to_string(),
                    "expectedVersion": 1,
                    "settings": { "permissionProfile": "fullAccess" }
                }
            }),
        )
        .await
        .context("settings update response")?;
    assert!(
        settings_response.get("error").is_none(),
        "settings update failed: {settings_response}"
    );

    let turn_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 5,
                "method": "turn/start",
                "params": {
                    "sessionId": session_id,
                    "input": [{ "type": "text", "text": "Probe twice." }],
                    "idempotencyKey": "settings-mid-turn-turn-1"
                }
            }),
        )
        .await
        .context("turn/start response")?;
    assert!(
        turn_response.get("error").is_none(),
        "turn/start failed: {turn_response}"
    );

    // The first probe executes without asking (fullAccess).
    let first_probe = async {
        while invocations.load(Ordering::SeqCst) < 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    };
    timeout(Duration::from_secs(10), first_probe)
        .await
        .context("first probe should execute under fullAccess without approval")?;

    // Tighten mid-turn; the override must reach the running turn.
    let tighten_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 6,
                "method": "session/metadata/update",
                "params": {
                    "sessionId": session_id.to_string(),
                    "expectedVersion": 2,
                    "settings": { "permissionProfile": "default" }
                }
            }),
        )
        .await
        .context("tighten response")?;
    let tighten_result: devo_protocol::native::rpc_session::SessionMetadataUpdateResult =
        serde_json::from_value(tighten_response["result"].clone())
            .with_context(|| format!("tighten response: {tighten_response}"))?;
    assert!(tighten_result.applied_to_active_turn);

    // Release the second probe: under default, network access requires an
    // interactive approval, so a Native approval/permission/request must
    // arrive and the tool must NOT execute.
    go_second.notify_one();
    let approval = timeout(Duration::from_secs(10), async {
        while let Some(value) = notifications_rx.recv().await {
            if value.get("method").and_then(serde_json::Value::as_str)
                == Some("approval/permission/request")
            {
                return Some(value);
            }
        }
        None
    })
    .await
    .context("second probe must require an approval under default")?
    .expect("approval request payload");
    assert!(approval.get("id").is_some());
    assert_eq!(
        invocations.load(Ordering::SeqCst),
        1,
        "the second probe must wait for approval instead of executing"
    );

    // Cleanup: interrupt the stalled turn.
    let _ = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 7,
                "method": "session/interrupt",
                "params": {
                    "scope": { "scope": "session", "sessionId": session_id }
                }
            }),
        )
        .await;
    Ok(())
}

/// Trace: L2-DES-CONV-002
/// Verifies: switching model and reasoning selection mid-turn makes the next
/// model request use the model-aware normalized selection.
#[tokio::test]
async fn mid_turn_model_switch_reaches_next_model_request() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let workspace_root = temp_dir.path().join("workspace");
    std::fs::create_dir_all(&workspace_root)?;

    let started = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let mut builder = ToolRegistryBuilder::new();
    builder.register_handler("gated_probe", Arc::new(GatedProbeTool));
    builder.push_spec(ToolSpec {
        name: "gated_probe".into(),
        description: "Blocks until released.".into(),
        input_schema: JsonSchema::object(Default::default(), None, None),
        output_mode: ToolOutputMode::Text,
        execution_mode: ToolExecutionMode::ReadOnly,
        capability_tags: vec![],
        supports_parallel: true,
        preparation_feedback: devo_core::tools::ToolPreparationFeedback::None,
        display_name: None,
        supports_cancellation: None,
        supports_streaming: None,
    });
    let provider = Arc::new(ToolThenDoneProvider {
        stream_requests: Mutex::new(Vec::new()),
        stream_started: Arc::clone(&started),
        release_stream: Arc::clone(&release),
    });
    let runtime = build_runtime(temp_dir.path(), provider.clone(), Arc::new(builder.build()));
    let (connection_id, _notifications_rx) = initialize_connection(&runtime).await?;

    let session_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 2,
                "method": "session/new",
                "params": {
                    "cwd": workspace_root,
                    "idempotencyKey": "settings-model-mid-turn-1"
                }
            }),
        )
        .await
        .context("session/new response")?;
    let session_result: devo_protocol::native::rpc_session::SessionNewResult =
        serde_json::from_value(session_response["result"].clone())
            .with_context(|| format!("session/new response: {session_response}"))?;
    let session_id = session_result.session.id;

    let subscription_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 3,
                "method": "subscription/create",
                "params": {
                    "selectors": [{ "kind": "session", "sessionId": session_id.to_string() }],
                    "includeSnapshot": false
                }
            }),
        )
        .await
        .context("subscription/create response")?;
    assert!(
        subscription_response.get("error").is_none(),
        "subscription/create failed: {subscription_response}"
    );

    let permission_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 4,
                "method": "session/metadata/update",
                "params": {
                    "sessionId": session_id,
                    "expectedVersion": 0,
                    "settings": { "permissionProfile": "fullAccess" }
                }
            }),
        )
        .await
        .context("permission setup response")?;
    assert!(
        permission_response.get("error").is_none(),
        "permission setup failed: {permission_response}"
    );

    let turn_response = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 5,
                "method": "turn/start",
                "params": {
                    "sessionId": session_id,
                    "input": [{ "type": "text", "text": "Probe, then continue." }],
                    "idempotencyKey": "model-mid-turn-turn-1"
                }
            }),
        )
        .await
        .context("turn/start response")?;
    assert!(
        turn_response.get("error").is_none(),
        "turn/start failed: {turn_response}"
    );

    // The first request runs on the turn-start model; its stream is blocked
    // before the first event, so the next request is not built yet.
    if timeout(Duration::from_secs(10), started.notified())
        .await
        .is_err()
    {
        anyhow::bail!(
            "gated stream did not start; model request count={}",
            provider
                .stream_requests
                .lock()
                .expect("requests lock")
                .len()
        );
    }

    // Switch model and exercise the migration vocabulary while the first
    // iteration is blocked. The final `on` maps to the first non-off level.
    for (request_id, raw, expected) in [
        (6, "off", "off"),
        (7, "high", "high"),
        (8, "none", "off"),
        (9, "on", "low"),
    ] {
        let switch_response = runtime
            .handle_incoming(
                connection_id,
                json!({
                    "id": request_id,
                    "method": "session/metadata/update",
                    "params": {
                        "sessionId": session_id.to_string(),
                        "expectedVersion": 0,
                        "model": { "provider": "openai", "model": "gpt-5.5" },
                        "settings": { "reasoningEffort": raw }
                    }
                }),
            )
            .await
            .context("model and reasoning switch response")?;
        let switch_result: devo_protocol::native::rpc_session::SessionMetadataUpdateResult =
            serde_json::from_value(switch_response["result"].clone()).with_context(|| {
                format!("model and reasoning switch response: {switch_response}")
            })?;
        assert!(switch_result.applied_to_active_turn);
        assert_eq!(switch_result.session.model.model, "openai/gpt-5.5");
        assert_eq!(
            switch_result.session.settings.reasoning_effort.as_deref(),
            Some(expected)
        );
    }

    // Releasing the stream lets the loop build the next request, which must
    // already use the switched model.
    release.notify_one();
    let second_request: Option<(String, Option<String>)> =
        timeout(Duration::from_secs(10), async {
            loop {
                let requests = provider
                    .stream_requests
                    .lock()
                    .expect("requests lock")
                    .len();
                if requests >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            provider
                .stream_requests
                .lock()
                .expect("requests lock")
                .get(1)
                .map(|request| (request.model.clone(), request.request_thinking.clone()))
        })
        .await
        .context("second model request should arrive")?;
    assert_eq!(
        second_request,
        Some(("openai/gpt-5.5".to_string(), Some("enabled".to_string()))),
        "the next model request must use the switched model"
    );

    let _ = runtime
        .handle_incoming(
            connection_id,
            json!({
                "id": 10,
                "method": "session/interrupt",
                "params": {
                    "scope": { "scope": "session", "sessionId": session_id }
                }
            }),
        )
        .await;
    Ok(())
}
