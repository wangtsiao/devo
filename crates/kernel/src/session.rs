//! Session-scoped REPL process and execute/interrupt/shutdown.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::Mutex;
use uuid::Uuid;

use crate::protocol::{
    KernelEvent, KernelRequest, ReadyEvent, ReplError, PROTOCOL_VERSION,
};

/// Async host callback for kernel `host_request` events.
///
/// Arguments are `(request_id, data)`. Implementations must not call
/// [`KernelSession::execute`] on the same session (execute gate is held).
/// Host replies use a separate stdin lock so they never wait on the execute
/// FIFO (Prime deadlock rule).
pub type HostRequestHandler =
    Arc<dyn Fn(String, Value) -> BoxFuture<'static, Value> + Send + Sync>;

/// Default deny handler when no host table is installed.
pub fn deny_host_handler() -> HostRequestHandler {
    Arc::new(|_id, data| {
        Box::pin(async move {
            let action = data
                .get("type")
                .or_else(|| data.get("action"))
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            serde_json::json!({
                "status": "error",
                "error": format!("host_request action not implemented: {action}"),
            })
        })
    })
}

/// Extract `(action, params)` from a REPL `host_request` data object.
pub fn parse_host_request_data(data: &Value) -> (String, Value) {
    let action = data
        .get("action")
        .and_then(|v| v.as_str())
        .or_else(|| data.get("type").and_then(|v| v.as_str()))
        .unwrap_or("")
        .to_string();
    if let Some(params) = data.get("params").cloned() {
        return (action, params);
    }
    let mut obj = data.as_object().cloned().unwrap_or_default();
    obj.remove("action");
    obj.remove("type");
    (action, Value::Object(obj))
}

/// Spike/dev gate for discrete vs RLM tool surface. Must not ship as a product flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ExecutionSurface {
    /// Legacy discrete JSON tools (fallback when kernel/host table unavailable).
    Discrete,
    /// Model tools = `[ipython]` only.
    Rlm,
}

/// Configuration for spawning a kernel session.
#[derive(Debug, Clone)]
pub struct KernelSessionConfig {
    pub python: PathBuf,
    pub cwd: PathBuf,
    /// Directories prepended to `PYTHONPATH` (vendored `prime-agent-runtime/src`).
    pub python_path_entries: Vec<PathBuf>,
}

impl Default for KernelSessionConfig {
    fn default() -> Self {
        Self {
            python: PathBuf::from("python"),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            python_path_entries: Vec::new(),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum SpawnError {
    #[error(transparent)]
    Repl(#[from] ReplError),
    #[error("failed to spawn kernel: {0}")]
    Spawn(#[source] std::io::Error),
}

/// Collected output from one kernel request that waits for `done`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CellOutput {
    pub stdout: String,
    pub stderr: String,
    pub result: Option<String>,
    pub status: String,
    pub error_name: Option<String>,
    pub error_value: Option<String>,
    pub traceback: Vec<String>,
    /// Populated by `list_names` (and any `done` that carries `names`).
    pub names: Option<Vec<String>>,
}

struct KernelIo {
    /// Writable independently of the execute gate (host_reply + interrupt).
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
}

/// One durable CPython REPL for a session.
pub struct KernelSession {
    child: Child,
    io: KernelIo,
    /// Serializes cells; held across host_request awaits (Ask).
    execute_gate: Mutex<()>,
    ready: ReadyEvent,
    cell_seq: AtomicU64,
    host_handler: Mutex<Option<HostRequestHandler>>,
}

impl KernelSession {
    /// Spawn `python -m rlm.repl` and complete the protocol handshake.
    pub async fn spawn(config: KernelSessionConfig) -> Result<Arc<Self>, SpawnError> {
        let mut cmd = Command::new(&config.python);
        cmd.arg("-m")
            .arg("rlm.repl")
            .current_dir(&config.cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1");

        if !config.python_path_entries.is_empty() {
            let mut paths: Vec<PathBuf> = config.python_path_entries.clone();
            if let Some(existing) = std::env::var_os("PYTHONPATH") {
                paths.extend(std::env::split_paths(&existing));
            }
            let joined = std::env::join_paths(&paths)
                .map_err(|e| ReplError::Other(format!("PYTHONPATH: {e}")))?;
            cmd.env("PYTHONPATH", joined);
        }

        let mut child = cmd.spawn().map_err(SpawnError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ReplError::Other("missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ReplError::Other("missing stdout".into()))?;

        let mut stdout = BufReader::new(stdout);
        let ready = read_ready(&mut stdout).await?;
        if ready.protocol != PROTOCOL_VERSION {
            return Err(ReplError::ProtocolMismatch {
                got: ready.protocol,
            }
            .into());
        }

        Ok(Arc::new(Self {
            child,
            io: KernelIo {
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(stdout),
            },
            execute_gate: Mutex::new(()),
            ready,
            cell_seq: AtomicU64::new(0),
            host_handler: Mutex::new(None),
        }))
    }

    /// Spawn a custom command that speaks REPL protocol v3 (tests / fakes).
    pub async fn spawn_command(
        mut cmd: Command,
        host_handler: Option<HostRequestHandler>,
    ) -> Result<Arc<Self>, SpawnError> {
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .env("PYTHONUTF8", "1")
            .env("PYTHONIOENCODING", "utf-8")
            .env("PYTHONUNBUFFERED", "1");
        let mut child = cmd.spawn().map_err(SpawnError::Spawn)?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| ReplError::Other("missing stdin".into()))?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| ReplError::Other("missing stdout".into()))?;
        let mut stdout = BufReader::new(stdout);
        let ready = read_ready(&mut stdout).await?;
        if ready.protocol != PROTOCOL_VERSION {
            return Err(ReplError::ProtocolMismatch {
                got: ready.protocol,
            }
            .into());
        }
        Ok(Arc::new(Self {
            child,
            io: KernelIo {
                stdin: Mutex::new(stdin),
                stdout: Mutex::new(stdout),
            },
            execute_gate: Mutex::new(()),
            ready,
            cell_seq: AtomicU64::new(0),
            host_handler: Mutex::new(host_handler),
        }))
    }

    pub fn ready(&self) -> &ReadyEvent {
        &self.ready
    }

    /// Install or clear the host_request callback (typically once per turn).
    pub async fn set_host_handler(&self, handler: Option<HostRequestHandler>) {
        *self.host_handler.lock().await = handler;
    }

    /// Execute a code cell and wait until its `done` event.
    pub async fn execute(&self, code: impl Into<String>) -> Result<CellOutput, ReplError> {
        let _gate = self.execute_gate.lock().await;
        let id = format!(
            "c{}",
            self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let req = KernelRequest::Execute {
            id: id.clone(),
            code: code.into(),
        };
        {
            let mut stdin = self.io.stdin.lock().await;
            write_request(&mut stdin, &req).await?;
        }
        collect_until_done(self, &id).await
    }

    /// Best-effort interrupt of the running (or next) cell.
    ///
    /// Uses the stdin lock only — does not wait for the execute gate — so an
    /// interrupt can land while a host_request Ask is outstanding.
    pub async fn interrupt(&self, id: Option<String>) -> Result<(), ReplError> {
        let mut stdin = self.io.stdin.lock().await;
        write_request(
            &mut stdin,
            &KernelRequest::Interrupt { id },
        )
        .await
    }

    /// Snapshot the kernel namespace to `path` (dill) with a JSON `manifest_path`.
    pub async fn snapshot(
        &self,
        path: impl AsRef<Path>,
        manifest_path: impl AsRef<Path>,
    ) -> Result<CellOutput, ReplError> {
        let _gate = self.execute_gate.lock().await;
        let id = format!(
            "s{}",
            self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let req = KernelRequest::Snapshot {
            id: id.clone(),
            path: path.as_ref().to_string_lossy().into_owned(),
            manifest_path: manifest_path.as_ref().to_string_lossy().into_owned(),
            max_bytes: None,
            max_variable_bytes: None,
            prune_oversized: None,
        };
        {
            let mut stdin = self.io.stdin.lock().await;
            write_request(&mut stdin, &req).await?;
        }
        collect_until_done(self, &id).await
    }

    /// Restore the kernel namespace from a dill at `path`.
    pub async fn restore(&self, path: impl AsRef<Path>) -> Result<CellOutput, ReplError> {
        let _gate = self.execute_gate.lock().await;
        let id = format!(
            "r{}",
            self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let req = KernelRequest::Restore {
            id: id.clone(),
            path: path.as_ref().to_string_lossy().into_owned(),
        };
        {
            let mut stdin = self.io.stdin.lock().await;
            write_request(&mut stdin, &req).await?;
        }
        collect_until_done(self, &id).await
    }

    /// List names currently defined in the kernel namespace.
    pub async fn list_names(&self) -> Result<Vec<String>, ReplError> {
        let _gate = self.execute_gate.lock().await;
        let id = format!(
            "n{}",
            self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1
        );
        let req = KernelRequest::ListNames { id: id.clone() };
        {
            let mut stdin = self.io.stdin.lock().await;
            write_request(&mut stdin, &req).await?;
        }
        let out = collect_until_done(self, &id).await?;
        Ok(out.names.unwrap_or_default())
    }

    /// Ask the kernel to shut down and wait for the child to exit.
    pub async fn shutdown(mut self) -> Result<(), ReplError> {
        {
            let _gate = self.execute_gate.lock().await;
            let id = Uuid::new_v4().to_string();
            {
                let mut stdin = self.io.stdin.lock().await;
                write_request(
                    &mut stdin,
                    &KernelRequest::Shutdown { id: Some(id.clone()) },
                )
                .await?;
            }
            let _ = collect_until_done(&self, &id).await;
        }
        let _ = self.child.wait().await;
        Ok(())
    }
}

async fn write_request(stdin: &mut ChildStdin, req: &KernelRequest) -> Result<(), ReplError> {
    let mut line = serde_json::to_string(req)?;
    line.push('\n');
    stdin.write_all(line.as_bytes()).await?;
    stdin.flush().await?;
    Ok(())
}

async fn read_line_event(stdout: &mut BufReader<ChildStdout>) -> Result<KernelEvent, ReplError> {
    let mut buf = String::new();
    let n = stdout.read_line(&mut buf).await?;
    if n == 0 {
        return Err(ReplError::Closed);
    }
    let trimmed = buf.trim_end_matches(['\r', '\n']);
    if trimmed.is_empty() {
        return Err(ReplError::Other("empty protocol line".into()));
    }
    Ok(serde_json::from_str(trimmed)?)
}

async fn read_ready(stdout: &mut BufReader<ChildStdout>) -> Result<ReadyEvent, ReplError> {
    match read_line_event(stdout).await? {
        KernelEvent::Ready { protocol, python } => Ok(ReadyEvent { protocol, python }),
        other => Err(ReplError::Handshake(format!(
            "expected ready, got {other:?}"
        ))),
    }
}

async fn collect_until_done(session: &KernelSession, id: &str) -> Result<CellOutput, ReplError> {
    let mut out = CellOutput::default();
    loop {
        let event = {
            let mut stdout = session.io.stdout.lock().await;
            read_line_event(&mut stdout).await?
        };
        match event {
            KernelEvent::Stdout {
                id: event_id,
                text,
            } if event_id.as_deref() == Some(id) || event_id.is_none() => {
                out.stdout.push_str(&text);
            }
            KernelEvent::Stderr {
                id: event_id,
                text,
            } if event_id.as_deref() == Some(id) || event_id.is_none() => {
                out.stderr.push_str(&text);
            }
            KernelEvent::Result {
                id: event_id,
                text,
            } if event_id == id => {
                out.result = Some(text);
            }
            KernelEvent::Error {
                id: event_id,
                ename,
                evalue,
                traceback,
            } if event_id.as_deref() == Some(id) || event_id.is_none() => {
                out.error_name = Some(ename);
                out.error_value = Some(evalue);
                out.traceback = traceback;
            }
            KernelEvent::Done {
                id: event_id,
                status,
                names,
                reason,
                ..
            } if event_id == id => {
                out.status = status;
                out.names = names;
                if let Some(reason) = reason {
                    if out.error_value.is_none() {
                        out.error_value = Some(reason);
                    }
                }
                return Ok(out);
            }
            KernelEvent::HostRequest {
                id: req_id,
                data,
            } => {
                let handler = session
                    .host_handler
                    .lock()
                    .await
                    .clone()
                    .unwrap_or_else(deny_host_handler);
                // Await without holding stdout/stdin so Ask / interrupt can progress.
                let reply_data = handler(req_id.clone(), data).await;
                let reply = KernelRequest::HostReply {
                    id: req_id,
                    data: reply_data,
                };
                let mut stdin = session.io.stdin.lock().await;
                write_request(&mut stdin, &reply).await?;
            }
            KernelEvent::Display { .. }
            | KernelEvent::Stdout { .. }
            | KernelEvent::Stderr { .. }
            | KernelEvent::Result { .. }
            | KernelEvent::Error { .. }
            | KernelEvent::Done { .. }
            | KernelEvent::Ready { .. } => {
                // Events for other cells or unattributed noise — ignore for now.
            }
        }
    }
}

/// Resolve the vendored runtime `src` dir under a Devo repo root.
///
/// Prefer `vendor/prime-agent-runtime/src`. Callers that need an override should
/// set `DEVO_RLM_RUNTIME_SRC` (handled by `ensure_kernel` / spawn config).
pub fn default_runtime_pythonpath(devo_root: &Path) -> Option<PathBuf> {
    let candidate = devo_root.join("vendor/prime-agent-runtime/src");
    candidate.join("rlm").is_dir().then_some(candidate)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;

    fn live_config() -> Option<KernelSessionConfig> {
        let runtime = std::env::var_os("DEVO_RLM_RUNTIME_SRC")
            .map(PathBuf::from)
            .filter(|p| p.join("rlm").is_dir())
            .or_else(|| default_runtime_pythonpath(Path::new(".")))
            .or_else(|| default_runtime_pythonpath(Path::new("../..")))
            .or_else(|| {
                // Workspace root when running from crates/kernel
                let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
                default_runtime_pythonpath(manifest.parent()?.parent()?)
                    .or_else(|| default_runtime_pythonpath(&manifest))
            })?;
        Some(KernelSessionConfig {
            python: PathBuf::from("python"),
            cwd: std::env::temp_dir(),
            python_path_entries: vec![runtime],
        })
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: handshake accepts protocol 3 from a live CPython REPL.
    #[tokio::test]
    async fn live_handshake_protocol_v3() {
        let Some(config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: could not spawn kernel: {e}");
                return;
            }
        };
        assert_eq!(session.ready().protocol, PROTOCOL_VERSION);
        // Drop the session; kill_on_drop terminates the child.
        drop(session);
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: namespace survives across two execute calls (cross-turn).
    #[tokio::test]
    async fn live_namespace_survives_two_executes() {
        let Some(config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: could not spawn kernel: {e}");
                return;
            }
        };
        let first = session.execute("x = 1").await.expect("turn1");
        assert_eq!(first.status, "ok", "stderr={}", first.stderr);
        let second = session.execute("print(x)").await.expect("turn2");
        assert_eq!(second.status, "ok", "stderr={}", second.stderr);
        assert!(
            second.stdout.contains('1'),
            "expected print(x) -> 1, got stdout={:?} result={:?}",
            second.stdout,
            second.result
        );
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: interrupt request is accepted without tearing down the session.
    #[tokio::test]
    async fn live_interrupt_keeps_session_alive() {
        let Some(config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: could not spawn kernel: {e}");
                return;
            }
        };
        session
            .interrupt(None)
            .await
            .expect("interrupt while idle");
        let after = session
            .execute("print('ok')")
            .await
            .expect("execute after interrupt");
        assert_eq!(after.status, "ok", "stderr={}", after.stderr);
        assert!(
            after.stdout.contains("ok"),
            "expected ok after interrupt, stdout={:?}",
            after.stdout
        );
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: Windows spawn uses `python` on PATH (RestrictedToken wrap is later).
    #[cfg(windows)]
    #[tokio::test]
    async fn live_windows_python_spawn() {
        let Some(config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        assert!(
            config.python.as_os_str() == "python"
                || config.python.extension().is_some_and(|e| e == "exe"),
            "windows kernel must use native python.exe, got {:?}",
            config.python
        );
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: could not spawn kernel: {e}");
                return;
            }
        };
        assert_eq!(session.ready().protocol, PROTOCOL_VERSION);
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: Unix spawn handshake succeeds when runtime is present.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_unix_python_spawn() {
        let Some(config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: could not spawn kernel: {e}");
                return;
            }
        };
        assert_eq!(session.ready().protocol, PROTOCOL_VERSION);
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: snapshot then restore round-trips a binding when runtime is present.
    #[tokio::test]
    async fn live_snapshot_restore_roundtrip() {
        let Some(config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: could not spawn kernel: {e}");
                return;
            }
        };
        let dir = tempfile::tempdir().expect("tempdir");
        let dill = dir.path().join("kernel-state.dill");
        let manifest = dir.path().join("kernel-state.json");
        let first = session.execute("snap_x = 99").await.expect("bind");
        assert_eq!(first.status, "ok", "stderr={}", first.stderr);
        let snap = session
            .snapshot(&dill, &manifest)
            .await
            .expect("snapshot");
        assert_eq!(snap.status, "ok", "stderr={}", snap.stderr);
        let clear = session
            .execute("del snap_x")
            .await
            .expect("clear");
        assert_eq!(clear.status, "ok", "stderr={}", clear.stderr);
        let restored = session.restore(&dill).await.expect("restore");
        assert_eq!(restored.status, "ok", "stderr={}", restored.stderr);
        let check = session
            .execute("print(snap_x)")
            .await
            .expect("check");
        assert_eq!(check.status, "ok", "stderr={}", check.stderr);
        assert!(
            check.stdout.contains("99"),
            "expected snap_x=99 after restore, stdout={:?} result={:?}",
            check.stdout,
            check.result
        );
        let names = session.list_names().await.expect("list_names");
        assert!(
            names.iter().any(|n| n == "snap_x"),
            "expected snap_x in names={names:?}"
        );
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: host_request during execute gets a host_reply (fake REPL).
    #[tokio::test]
    async fn host_request_receives_reply_via_fake_repl() {
        let script = r#"
import json, sys
print(json.dumps({"event":"ready","protocol":3,"python":"fake"}), flush=True)
for line in sys.stdin:
    line = line.strip()
    if not line:
        continue
    req = json.loads(line)
    if req.get("type") == "execute":
        print(json.dumps({
            "event": "host_request",
            "id": "h1",
            "data": {"type": "rlm.create_session", "params": {}}
        }), flush=True)
        reply = json.loads(sys.stdin.readline())
        assert reply.get("type") == "host_reply", reply
        assert reply.get("id") == "h1", reply
        print(json.dumps({"event":"stdout","id":req["id"],"text":str(reply["data"].get("status",""))+"\n"}), flush=True)
        print(json.dumps({"event":"done","id":req["id"],"status":"ok"}), flush=True)
    elif req.get("type") == "shutdown":
        print(json.dumps({"event":"done","id":req.get("id") or "","status":"ok"}), flush=True)
        break
"#;
        let dir = tempfile::tempdir().expect("tempdir");
        let script_path = dir.path().join("fake_repl.py");
        std::fs::write(&script_path, script).expect("write fake repl");

        let mut cmd = Command::new("python");
        cmd.arg(&script_path);

        let seen = Arc::new(Mutex::new(None::<String>));
        let seen_cb = Arc::clone(&seen);
        let handler: HostRequestHandler = Arc::new(move |_id, data| {
            let seen_cb = Arc::clone(&seen_cb);
            Box::pin(async move {
                let action = data
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                *seen_cb.lock().await = Some(action.clone());
                serde_json::json!({
                    "status": "error",
                    "error": format!("denied: {action}"),
                })
            })
        });

        let session = match KernelSession::spawn_command(cmd, Some(handler)).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: could not spawn fake repl: {e}");
                return;
            }
        };
        let out = session.execute("pass").await.expect("execute");
        assert_eq!(out.status, "ok", "stderr={}", out.stderr);
        assert_eq!(
            seen.lock().await.as_deref(),
            Some("rlm.create_session")
        );
        assert!(
            out.stdout.contains("error"),
            "expected reply status echoed on stdout, got {:?}",
            out.stdout
        );
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: parse_host_request_data reads action + params.
    #[test]
    fn parse_host_request_action_and_params() {
        let (action, params) = parse_host_request_data(&serde_json::json!({
            "action": "compact.status",
            "params": {"x": 1}
        }));
        assert_eq!(action, "compact.status");
        assert_eq!(params, serde_json::json!({"x": 1}));
    }
}
