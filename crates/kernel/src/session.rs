//! Session-scoped REPL process and execute/interrupt/shutdown.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use futures::future::BoxFuture;
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, ChildStdout, Command};
use tokio::sync::{Mutex, Notify};
use uuid::Uuid;

use crate::protocol::{KernelEvent, KernelRequest, PROTOCOL_VERSION, ReadyEvent, ReplError};

/// Async host callback for kernel `host_request` events.
///
/// Arguments are `(request_id, data)`. Implementations must not call
/// [`KernelSession::execute`] on the same session (execute gate is held).
/// Host replies use a separate stdin lock so they never wait on the execute
/// FIFO (Prime deadlock rule).
pub type HostRequestHandler = Arc<dyn Fn(String, Value) -> BoxFuture<'static, Value> + Send + Sync>;

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
    /// Model tools = `[ipython, bash]` (+ MCP at turn build; hosted web_search
    /// on anthropic/responses wires).
    Rlm,
}

/// Configuration for spawning a kernel session.
#[derive(Debug, Clone)]
pub struct KernelSessionConfig {
    pub python: PathBuf,
    pub cwd: PathBuf,
    /// Directories prepended to `PYTHONPATH` (vendored `prime-agent-runtime/src`).
    pub python_path_entries: Vec<PathBuf>,
    /// Extra environment variables for the kernel process (harness paths, depth, …).
    pub extra_env: Vec<(String, String)>,
    /// OS fence request (design doc §5). `None` = unfenced by design; a fence
    /// that cannot be raised downgrades explicitly, never silently (§5.3).
    pub fence: Option<KernelFenceSpec>,
}

impl Default for KernelSessionConfig {
    fn default() -> Self {
        Self {
            python: PathBuf::from("python"),
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            python_path_entries: Vec::new(),
            extra_env: Vec::new(),
            fence: None,
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
    /// Diffs from `application/vnd.prime-agent.diff+json` display events.
    pub diffs: Vec<CellDiffDisplay>,
    /// Media attachments from `application/vnd.prime-agent.attachment+json`
    /// (e.g. attach-image skill).
    pub attachments: Vec<CellAttachment>,
}

/// One file edit emitted by the kernel edit skill (Prime MIME).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellDiffDisplay {
    pub path: String,
    pub old_str: String,
    pub new_str: String,
    pub start_line: Option<u32>,
}

/// One media attachment emitted by the attach-image skill (Prime MIME).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CellAttachment {
    pub mime_type: String,
    /// base64-encoded image bytes.
    pub data: String,
    pub path: Option<String>,
}

/// MIME used by Prime's edit skill when displaying a file diff from Python.
pub const DIFF_DISPLAY_MIME: &str = "application/vnd.prime-agent.diff+json";

/// MIME used by Prime's attach-image skill when loading an image into context.
pub const ATTACHMENT_DISPLAY_MIME: &str = "application/vnd.prime-agent.attachment+json";

/// Hard ceiling on a single attachment's base64 payload (defensive; skill caps lower).
pub const MAX_ATTACHMENT_DATA_CHARS: usize = 10_000_000;

/// Parse a Prime diff display payload from a kernel `display` event's `data`.
pub fn parse_diff_display(data: &Value) -> Option<CellDiffDisplay> {
    let payload = data
        .get(DIFF_DISPLAY_MIME)
        .or_else(|| data.get("application/vnd.prime-agent.diff+json"))?;
    let path = payload.get("path")?.as_str()?.to_string();
    let old_str = payload
        .get("old_str")
        .or_else(|| payload.get("oldStr"))?
        .as_str()?
        .to_string();
    let new_str = payload
        .get("new_str")
        .or_else(|| payload.get("newStr"))?
        .as_str()?
        .to_string();
    let start_line = payload
        .get("start_line")
        .or_else(|| payload.get("startLine"))
        .and_then(|v| v.as_u64())
        .map(|n| n as u32);
    Some(CellDiffDisplay {
        path,
        old_str,
        new_str,
        start_line,
    })
}

/// Parse a Prime attachment display payload from a kernel `display` event's `data`.
///
/// Oversized or malformed payloads are dropped (returns `None`).
pub fn parse_attachment_display(data: &Value) -> Option<CellAttachment> {
    let payload = data
        .get(ATTACHMENT_DISPLAY_MIME)
        .or_else(|| data.get("application/vnd.prime-agent.attachment+json"))?;
    let mime_type = payload
        .get("mime_type")
        .or_else(|| payload.get("mimeType"))?
        .as_str()?
        .to_string();
    if !mime_type.starts_with("image/") {
        return None;
    }
    let data_b64 = payload.get("data")?.as_str()?.to_string();
    if data_b64.is_empty() || data_b64.len() > MAX_ATTACHMENT_DATA_CHARS {
        return None;
    }
    let path = payload
        .get("path")
        .and_then(|v| v.as_str())
        .map(str::to_string);
    Some(CellAttachment {
        mime_type,
        data: data_b64,
        path,
    })
}

struct KernelIo {
    /// Writable independently of the execute gate (host_reply + interrupt).
    stdin: Mutex<ChildStdin>,
    stdout: Mutex<BufReader<ChildStdout>>,
}

/// Shared collector state for a cell started via [`KernelSession::begin_execute`].
struct InflightCollectorState {
    output: CellOutput,
    /// Set when the collector finishes (`Ok` = got `done`; `Err` = I/O/protocol message).
    terminal: Option<Result<(), String>>,
}

/// Outcome of a budgeted wait on an in-flight Python cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CellWaitOutcome {
    /// Cell reached `done` within the budget.
    Done(CellOutput),
    /// Budget expired; cell is still running. `snapshot` is stdout/stderr so far.
    TimedOut { snapshot: CellOutput },
}

/// A cell that has been submitted to the kernel and is still being collected.
///
/// The execute gate is held by the background collector until `done` (stdout is
/// exclusive). Parking releases the *caller* so the turn can continue; later
/// cells still serialize behind this gate until the parked cell finishes.
pub struct InFlightCell {
    session: Arc<KernelSession>,
    cell_id: String,
    state: Arc<Mutex<InflightCollectorState>>,
    done: Arc<Notify>,
}

impl InFlightCell {
    pub fn cell_id(&self) -> &str {
        &self.cell_id
    }

    /// Best-effort interrupt of this cell (does not wait for `done`).
    pub async fn interrupt(&self) -> Result<(), ReplError> {
        self.session.interrupt(Some(self.cell_id.clone())).await
    }

    /// Latest stdout/stderr/result snapshot (for wait-policy prompts).
    pub async fn snapshot(&self) -> CellOutput {
        self.state.lock().await.output.clone()
    }

    /// Wait up to `budget` for the cell to finish.
    pub async fn wait_for(&self, budget: Duration) -> Result<CellWaitOutcome, ReplError> {
        if let Some(outcome) = self.try_take_done().await? {
            return Ok(CellWaitOutcome::Done(outcome));
        }
        if budget.is_zero() {
            return Ok(CellWaitOutcome::TimedOut {
                snapshot: self.snapshot().await,
            });
        }
        tokio::select! {
            () = self.done.notified() => {}
            () = tokio::time::sleep(budget) => {
                if let Some(outcome) = self.try_take_done().await? {
                    return Ok(CellWaitOutcome::Done(outcome));
                }
                return Ok(CellWaitOutcome::TimedOut {
                    snapshot: self.snapshot().await,
                });
            }
        }
        if let Some(outcome) = self.try_take_done().await? {
            Ok(CellWaitOutcome::Done(outcome))
        } else {
            Ok(CellWaitOutcome::TimedOut {
                snapshot: self.snapshot().await,
            })
        }
    }

    /// Wait until the cell reaches `done` (or collector error).
    pub async fn wait_until_done(self) -> Result<CellOutput, ReplError> {
        loop {
            if let Some(outcome) = self.try_take_done().await? {
                return Ok(outcome);
            }
            self.done.notified().await;
        }
    }

    /// Keep collecting in the background and invoke `on_complete` when finished.
    pub fn park<F>(self, on_complete: F)
    where
        F: FnOnce(String, Result<CellOutput, ReplError>) + Send + 'static,
    {
        tokio::spawn(async move {
            let cell_id = self.cell_id.clone();
            let result = self.wait_until_done().await;
            on_complete(cell_id, result);
        });
    }

    async fn try_take_done(&self) -> Result<Option<CellOutput>, ReplError> {
        let state = self.state.lock().await;
        match &state.terminal {
            Some(Ok(())) => Ok(Some(state.output.clone())),
            Some(Err(err)) => Err(ReplError::Other(err.clone())),
            None => Ok(None),
        }
    }
}

/// One durable CPython REPL for a session.
pub struct KernelSession {
    child: Child,
    io: KernelIo,
    /// Serializes cells; held across host_request awaits (Ask).
    /// `Arc` so owned guards can move into background collectors.
    execute_gate: Arc<Mutex<()>>,
    ready: ReadyEvent,
    cell_seq: AtomicU64,
    host_handler: Mutex<Option<HostRequestHandler>>,
    /// Fence outcome for this kernel (P0): callers read it to emit rollout
    /// fence events and UI downgrade warnings (design doc §5.3).
    fence_state: crate::fence::FenceState,
    /// Unix credential delivery channel (§9): grants over SCM_RIGHTS. The
    /// peer descriptor is inherited by the kernel process; its number rides
    /// the `DEVO_GRANT_FD` env var.
    #[cfg(unix)]
    grant_channel: Option<crate::credential_unix::GrantChannel>,
    /// Per-session credential authority (P2, Windows fenced kernels only):
    /// grants deliver ACEs for the SID carried in the kernel's restricted
    /// token — the kernel is never restarted (§9).
    #[cfg(windows)]
    fence_credentials: Option<devo_windows_sandbox::SessionCredentialAuthority>,
}

/// Environment variable name segments that mark a variable as a credential.
///
/// The kernel runs model-generated code and never needs provider or cloud
/// credentials; the server holds those. Dropping credential-shaped variables
/// from the inherited environment prevents silent exfiltration through env
/// inspection. Anything the kernel legitimately needs (PATH, TEMP, locale,
/// PYTHON*, RLM_*) does not match these segments.
const CREDENTIAL_ENV_SEGMENTS: &[&str] = &[
    "KEY",
    "TOKEN",
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "CREDENTIAL",
    "CREDENTIALS",
    "AUTH",
];

fn is_credential_env_var(name: &str) -> bool {
    name.to_ascii_uppercase()
        .split('_')
        .any(|seg| CREDENTIAL_ENV_SEGMENTS.contains(&seg))
}

#[cfg(test)]
mod credential_env_tests {
    use super::is_credential_env_var;

    #[test]
    fn matches_credential_shaped_names() {
        for name in [
            "OPENAI_API_KEY",
            "ANTHROPIC_API_KEY",
            "AWS_SECRET_ACCESS_KEY",
            "AWS_SESSION_TOKEN",
            "GITHUB_TOKEN",
            "AZURE_CLIENT_SECRET",
            "NPM_TOKEN",
            "SSH_AUTH_SOCK",
            "DEPLOY_PASSWORD",
            "openai_api_key",
        ] {
            assert!(is_credential_env_var(name), "{name} should be scrubbed");
        }
    }

    #[test]
    fn keeps_non_credential_names() {
        for name in [
            "PATH",
            "TEMP",
            "USERPROFILE",
            "LANG",
            "PYTHONUTF8",
            "RLM_SESSION_DIR",
            "DATABASE_URL",
            "JAVA_HOME",
        ] {
            assert!(!is_credential_env_var(name), "{name} must be kept");
        }
    }
}

/// OS fence request for the kernel process (design doc §5.1/§5.2). `None`
/// spawns the kernel unfenced by design; a fence that cannot be raised is an
/// explicit downgrade, never a silent one (§5.3).
#[derive(Debug, Clone)]
pub struct KernelFenceSpec {
    /// Read-only roots beyond the platform defaults. **Empty = full-disk
    /// read** (workspace-profile semantic); the Windows legacy sandbox
    /// backend refuses restricted-read profiles.
    pub readable_roots: Vec<PathBuf>,
    /// Read-write roots (workspace first).
    pub writable_roots: Vec<PathBuf>,
    /// Paths the kernel must not read (deny roots).
    pub deny_read: Vec<PathBuf>,
    /// Whether the kernel's network must be restricted (default true for RLM).
    pub restrict_network: bool,
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

        // OS fence (P0): on Windows with a provisioned sandbox this replaces
        // OS fence (design doc §5): Windows wraps with the restricted-token
        // launcher; Unix reuses the platform sandbox wrapper (bwrap/Seatbelt)
        // that shell sandboxing uses, carrying the fence roots as overlay.
        // Every unimplementable fence is an explicit downgrade, never silent.
        #[cfg(any(windows, unix))]
        let mut fence_outcome = crate::fence::wrap_or_bare(&config, cmd);
        // Unix: duplicate the channel's peer fd (no CLOEXEC) for the child;
        // its number rides DEVO_GRANT_FD and the descriptor survives exec.
        #[cfg(unix)]
        let mut config = config;
        #[cfg(unix)]
        if let Some(channel) = fence_outcome.grant_channel.as_ref() {
            let peer = channel.peer_fd();
            if let Ok(dup) = crate::credential_unix::dup_no_cloexec(peer) {
                config
                    .extra_env
                    .push(("DEVO_GRANT_FD".to_string(), dup.to_string()));
            }
        }
        #[cfg(any(windows, unix))]
        let mut cmd = fence_outcome.command;
        #[cfg(any(windows, unix))]
        let fence_state = fence_outcome.state;
        #[cfg(windows)]
        let fence_credentials = fence_outcome.credentials;
        // Unix pipe mode: apply the parent-resolved Landlock/seccomp plan in
        // the child after fork (same pattern as the shell sandbox path).
        #[cfg(unix)]
        if let Some(plan) = fence_outcome.child_plan {
            unsafe {
                cmd.pre_exec(move || {
                    devo_util_process::sandbox::apply_resolved_in_child(Some(&plan))
                });
            }
        }

        // Never inherit credential-shaped variables into the kernel: it runs
        // model-generated code and must not see provider or cloud credentials.
        // Env names are case-insensitive on Windows, so match that way on all
        // platforms. Explicit `extra_env` entries below still take effect.
        for (key, _) in std::env::vars() {
            if is_credential_env_var(&key) {
                cmd.env_remove(&key);
            }
        }

        for (key, value) in kernel_env_overrides(&config) {
            cmd.env(key, value);
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
        let ready = match read_ready(&mut stdout).await {
            Ok(ready) => ready,
            Err(error) => {
                // Handshake died: surface the child's stderr tail instead of a
                // bare "closed unexpectedly" (fail loudly, never silently).
                let mut stderr_tail = String::new();
                if let Some(mut stderr) = child.stderr.take() {
                    use tokio::io::AsyncReadExt;
                    let _ = tokio::time::timeout(
                        Duration::from_secs(2),
                        stderr.read_to_string(&mut stderr_tail),
                    )
                    .await;
                }
                let stderr_tail: String = stderr_tail.chars().take(400).collect();
                return Err(ReplError::Other(format!(
                    "kernel handshake failed: {error}; stderr tail: {stderr_tail:?}"
                ))
                .into());
            }
        };
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
            execute_gate: Arc::new(Mutex::new(())),
            ready,
            cell_seq: AtomicU64::new(0),
            host_handler: Mutex::new(None),
            fence_state,
            #[cfg(unix)]
            grant_channel: fence_outcome.grant_channel.take(),
            #[cfg(windows)]
            fence_credentials,
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
            execute_gate: Arc::new(Mutex::new(())),
            ready,
            cell_seq: AtomicU64::new(0),
            host_handler: Mutex::new(host_handler),
            fence_state: crate::fence::FenceState::NotRequested,
            #[cfg(unix)]
            grant_channel: None,
            #[cfg(windows)]
            fence_credentials: None,
        }))
    }

    pub fn ready(&self) -> &ReadyEvent {
        &self.ready
    }

    /// Fence outcome recorded at spawn (P0): `Fenced`, `DowngradedUnfenced`
    /// (loud downgrade — callers must surface it), or `NotRequested`.
    pub fn fence_state(&self) -> crate::fence::FenceState {
        self.fence_state
    }

    /// Per-session credential authority for a fenced kernel (P2, Windows):
    /// callers wire approval callbacks to `grant_write_root`/`revoke_*` so
    /// permission changes are credential deliveries, never restarts (§9).
    #[cfg(windows)]
    pub fn fence_credentials(
        &self,
    ) -> Option<&devo_windows_sandbox::SessionCredentialAuthority> {
        self.fence_credentials.as_ref()
    }

    /// Unix credential delivery channel (§9): approvals call `grant`/`revoke`
    /// on it; the kernel facade turns received descriptors into native
    /// dirfd-backed rlm.read/rlm.write. `None` for unfenced kernels.
    #[cfg(unix)]
    pub fn grant_channel(&self) -> Option<&crate::credential_unix::GrantChannel> {
        self.grant_channel.as_ref()
    }

    /// Install or clear the host_request callback (typically once per turn).
    pub async fn set_host_handler(&self, handler: Option<HostRequestHandler>) {
        *self.host_handler.lock().await = handler;
    }

    /// Execute a code cell and wait until its `done` event.
    pub async fn execute(&self, code: impl Into<String>) -> Result<CellOutput, ReplError> {
        let _gate = self.execute_gate.lock().await;
        let id = format!("c{}", self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1);
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

    /// Start a code cell and return an [`InFlightCell`] for budgeted waits / park.
    ///
    /// The execute gate is held by a background collector until the cell reaches
    /// `done`. Callers may detach via [`InFlightCell::park`] without interrupting.
    pub async fn begin_execute(
        self: &Arc<Self>,
        code: impl Into<String>,
    ) -> Result<InFlightCell, ReplError> {
        let gate = Arc::clone(&self.execute_gate);
        let guard = gate.lock_owned().await;
        let id = format!("c{}", self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1);
        let req = KernelRequest::Execute {
            id: id.clone(),
            code: code.into(),
        };
        {
            let mut stdin = self.io.stdin.lock().await;
            write_request(&mut stdin, &req).await?;
        }

        let state = Arc::new(Mutex::new(InflightCollectorState {
            output: CellOutput::default(),
            terminal: None,
        }));
        let done = Arc::new(Notify::new());
        let session = Arc::clone(self);
        let state_c = Arc::clone(&state);
        let done_c = Arc::clone(&done);
        let id_c = id.clone();
        tokio::spawn(async move {
            let _gate = guard;
            let collect_result = collect_into_shared(&session, &id_c, &state_c).await;
            {
                let mut state = state_c.lock().await;
                state.terminal = Some(collect_result.map_err(|e| e.to_string()));
            }
            done_c.notify_waiters();
        });

        Ok(InFlightCell {
            session: Arc::clone(self),
            cell_id: id,
            state,
            done,
        })
    }

    /// Best-effort interrupt of the running (or next) cell.
    ///
    /// Uses the stdin lock only — does not wait for the execute gate — so an
    /// interrupt can land while a host_request Ask is outstanding.
    pub async fn interrupt(&self, id: Option<String>) -> Result<(), ReplError> {
        let mut stdin = self.io.stdin.lock().await;
        write_request(&mut stdin, &KernelRequest::Interrupt { id }).await
    }

    /// Snapshot the kernel namespace to `path` (dill) with a JSON `manifest_path`.
    pub async fn snapshot(
        &self,
        path: impl AsRef<Path>,
        manifest_path: impl AsRef<Path>,
    ) -> Result<CellOutput, ReplError> {
        let _gate = self.execute_gate.lock().await;
        let id = format!("s{}", self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1);
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
        let id = format!("r{}", self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1);
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
        let id = format!("n{}", self.cell_seq.fetch_add(1, Ordering::Relaxed) + 1);
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
                    &KernelRequest::Shutdown {
                        id: Some(id.clone()),
                    },
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

/// Environment overrides the kernel child needs regardless of how it is
/// launched (bare or sandbox-wrapped): harness env plus the runtime
/// PYTHONPATH. For the sandbox wrapper these must ride the request itself —
/// the wrapper builds the child's env from its argv snapshot, so `cmd.env()`
/// set after wrapping would be lost.
pub(crate) fn kernel_env_overrides(config: &KernelSessionConfig) -> Vec<(String, String)> {
    let mut overrides: Vec<(String, String)> = config.extra_env.clone();
    if !config.python_path_entries.is_empty() {
        let mut paths: Vec<PathBuf> = config.python_path_entries.clone();
        if let Some(existing) = std::env::var_os("PYTHONPATH") {
            paths.extend(std::env::split_paths(&existing));
        }
        if let Ok(joined) = std::env::join_paths(&paths) {
            overrides.push(("PYTHONPATH".to_string(), joined.to_string_lossy().into_owned()));
        }
    }
    overrides
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
    let state = Arc::new(Mutex::new(InflightCollectorState {
        output: CellOutput::default(),
        terminal: None,
    }));
    collect_into_shared(session, id, &state).await?;
    Ok(state.lock().await.output.clone())
}

async fn collect_into_shared(
    session: &KernelSession,
    id: &str,
    state: &Arc<Mutex<InflightCollectorState>>,
) -> Result<(), ReplError> {
    loop {
        let event = {
            let mut stdout = session.io.stdout.lock().await;
            read_line_event(&mut stdout).await?
        };
        match event {
            KernelEvent::Stdout { id: event_id, text }
                if event_id.as_deref() == Some(id) || event_id.is_none() =>
            {
                state.lock().await.output.stdout.push_str(&text);
            }
            KernelEvent::Stderr { id: event_id, text }
                if event_id.as_deref() == Some(id) || event_id.is_none() =>
            {
                state.lock().await.output.stderr.push_str(&text);
            }
            KernelEvent::Result { id: event_id, text } if event_id == id => {
                state.lock().await.output.result = Some(text);
            }
            KernelEvent::Error {
                id: event_id,
                ename,
                evalue,
                traceback,
            } if event_id.as_deref() == Some(id) || event_id.is_none() => {
                let mut out = state.lock().await;
                out.output.error_name = Some(ename);
                out.output.error_value = Some(evalue);
                out.output.traceback = traceback;
            }
            KernelEvent::Done {
                id: event_id,
                status,
                names,
                reason,
                ..
            } if event_id == id => {
                let mut out = state.lock().await;
                out.output.status = status;
                out.output.names = names;
                if let Some(reason) = reason
                    && out.output.error_value.is_none()
                {
                    out.output.error_value = Some(reason);
                }
                return Ok(());
            }
            KernelEvent::HostRequest { id: req_id, data } => {
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
            KernelEvent::Display {
                id: event_id,
                data,
            } if event_id.as_deref() == Some(id) || event_id.is_none() => {
                let mut out = state.lock().await;
                if let Some(diff) = parse_diff_display(&data) {
                    out.output.diffs.push(diff);
                }
                if let Some(attachment) = parse_attachment_display(&data) {
                    out.output.attachments.push(attachment);
                }
            }
            KernelEvent::Stdout { .. }
            | KernelEvent::Stderr { .. }
            | KernelEvent::Result { .. }
            | KernelEvent::Error { .. }
            | KernelEvent::Done { .. }
            | KernelEvent::Ready { .. }
            | KernelEvent::Display { .. } => {
                // Events for other cells or unattributed noise — ignore for now.
            }
        }
    }
}

/// Resolve the RLM runtime `src` dir (the folder that contains the `rlm` package).
///
/// Walks up from `start` looking for `crates/kernel/rlm-runtime/src` or `rlm-runtime/src`.
/// Override with `DEVO_RLM_RUNTIME_SRC`.
pub fn default_runtime_pythonpath(start: &Path) -> Option<PathBuf> {
    let mut dir = start.to_path_buf();
    for _ in 0..8 {
        let candidates = [
            dir.join("crates/kernel/rlm-runtime/src"),
            dir.join("rlm-runtime/src"),
        ];
        if let Some(candidate) = candidates.into_iter().find(|path| path.join("rlm").is_dir()) {
            // Always absolute: the kernel child's cwd differs from this
            // process's cwd, so a relative PYTHONPATH entry would silently
            // resolve to nothing inside the child (No module named 'rlm').
            return Some(dunce::canonicalize(&candidate).unwrap_or(candidate));
        }
        match dir.parent() {
            Some(parent) => dir = parent.to_path_buf(),
            None => break,
        }
    }
    None
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
            extra_env: Vec::new(),
            fence: None,
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

    /// Trace: design doc rlm-permissions.md §5-§9 (Windows provisioned path)
    /// Verifies: with the sandbox provisioned, the kernel spawns inside the
    /// restricted-token fence; writes inside the workspace succeed, writes
    /// outside are refused by the OS, and state survives hitting the wall.
    #[cfg(windows)]
    #[tokio::test]
    async fn live_fenced_kernel_enforces_os_boundary_windows() {
        // The fence wrapper re-execs `current_exe()` with the sandbox sentinel
        // argv; only the real `devo.exe` early-dispatch understands it, so the
        // in-cargo-test binary cannot self-host the wrapper. Verify via the
        // product binary (TUI e2e) instead.
        let exe = std::env::current_exe().unwrap_or_default();
        let exe_name = exe
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        // The wrapper re-execs current_exe() with the sandbox sentinel argv;
        // only the product binary's early dispatch understands it. The cargo
        // test harness (`devo_kernel-<hash>.exe`) cannot self-host it — this
        // path is verified on the product binary (TUI e2e: fenced spawn,
        // OS-refused out-of-fence writes, PID-stable credential delivery).
        if exe_name != "devo.exe" && exe_name != "devo" {
            eprintln!("skip: fence wrapper needs the devo.exe host binary, not {exe_name}");
            return;
        }
        let Some(mut config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let workspace = tempfile::tempdir().expect("workspace");
        config.cwd = workspace.path().to_path_buf();
        config.fence = Some(KernelFenceSpec {
            readable_roots: Vec::new(), // empty = full-disk read (workspace semantic)
            writable_roots: vec![workspace.path().to_path_buf()],
            deny_read: Vec::new(),
            restrict_network: true,
        });
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                panic!("fenced kernel spawn failed (provisioned?): {e}");
            }
        };
        assert_eq!(session.fence_state(), crate::fence::FenceState::Fenced);

        let inside = workspace.path().join("inside.txt");
        let ok = session
            .execute(format!("open({inside:?}, 'w').write('ok')"))
            .await
            .expect("inside write");
        assert_eq!(ok.status, "ok", "stderr={}", ok.stderr);
        let marker = session.execute("fenced_state = 42").await.expect("state");
        assert_eq!(marker.status, "ok", "stderr={}", marker.stderr);

        // Outside the fence: the restricted token refuses the write.
        let outside = session
            .execute("open(r'C:/Windows/devo-fence-probe-x7', 'w')")
            .await
            .expect("outside write cell");
        assert_ne!(
            outside.status, "ok",
            "write outside the fence must fail; stdout={:?} stderr={:?}",
            outside.stdout, outside.stderr
        );

        let state = session
            .execute("print(fenced_state)")
            .await
            .expect("state after wall");
        assert_eq!(state.status, "ok", "stderr={}", state.stderr);
        assert!(state.stdout.contains("42"), "stdout={:?}", state.stdout);
    }

    /// Trace: L2-DES-RLM-001 / design doc rlm-permissions.md §5-§9
    /// Verifies: on Unix the fenced kernel enforces the OS boundary — writes
    /// inside the granted workspace succeed, writes outside are refused by
    /// the OS (not by a filter), and the kernel keeps its state afterwards.
    #[cfg(unix)]
    #[tokio::test]
    async fn live_fenced_kernel_enforces_os_boundary() {
        let Some(mut config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let workspace = tempfile::tempdir().expect("workspace");
        config.cwd = workspace.path().to_path_buf();
        config.fence = Some(KernelFenceSpec {
            readable_roots: Vec::new(), // empty = full-disk read (workspace semantic)
            writable_roots: vec![workspace.path().to_path_buf()],
            deny_read: Vec::new(),
            restrict_network: true,
        });
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                panic!("fenced kernel spawn failed (bwrap available?): {e}");
            }
        };
        assert_eq!(session.fence_state(), crate::fence::FenceState::Fenced);

        // Inside the fence: write + read back works, and state persists.
        let inside = workspace.path().join("inside.txt");
        let ok = session
            .execute(format!("open({inside:?}, 'w').write('ok')"))
            .await
            .expect("inside write");
        assert_eq!(ok.status, "ok", "stderr={}", ok.stderr);
        let marker = session.execute("fenced_state = 42").await.expect("state");
        assert_eq!(marker.status, "ok", "stderr={}", marker.stderr);

        // Outside the fence: the OS itself refuses the write.
        let outside = session
            .execute("open('/usr/local/devo-fence-probe-x7', 'w')")
            .await
            .expect("outside write cell");
        assert_ne!(
            outside.status, "ok",
            "write outside the fence must fail; stdout={:?} stderr={:?}",
            outside.stdout, outside.stderr
        );

        // After hitting the wall the kernel is the same living process with
        // its state intact (never restarted, §9).
        let state = session.execute("print(fenced_state)").await.expect("state after wall");
        assert_eq!(state.status, "ok", "stderr={}", state.stderr);
        assert!(state.stdout.contains("42"), "stdout={:?}", state.stdout);
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
        session.interrupt(None).await.expect("interrupt while idle");
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
        let dill = dir.path().join("kernel.dill");
        let manifest = dir.path().join("kernel.json");
        let first = session.execute("snap_x = 99").await.expect("bind");
        assert_eq!(first.status, "ok", "stderr={}", first.stderr);
        let snap = session.snapshot(&dill, &manifest).await.expect("snapshot");
        if snap.status != "ok" {
            eprintln!(
                "skip: snapshot not supported by runtime (status={} stderr={})",
                snap.status, snap.stderr
            );
            return;
        }
        let clear = session.execute("del snap_x").await.expect("clear");
        assert_eq!(clear.status, "ok", "stderr={}", clear.stderr);
        let restored = session.restore(&dill).await.expect("restore");
        assert_eq!(restored.status, "ok", "stderr={}", restored.stderr);
        let check = session.execute("print(snap_x)").await.expect("check");
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
        assert_eq!(seen.lock().await.as_deref(), Some("rlm.create_session"));
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

    #[test]
    fn parse_diff_display_reads_prime_mime_payload() {
        let data = serde_json::json!({
            DIFF_DISPLAY_MIME: {
                "path": "src/a.py",
                "old_str": "x = 1\n",
                "new_str": "x = 2\n",
                "start_line": 3,
            }
        });
        let diff = parse_diff_display(&data).expect("diff");
        assert_eq!(diff.path, "src/a.py");
        assert_eq!(diff.old_str, "x = 1\n");
        assert_eq!(diff.new_str, "x = 2\n");
        assert_eq!(diff.start_line, Some(3));
    }

    #[test]
    fn parse_diff_display_ignores_unrelated_display() {
        let data = serde_json::json!({ "text/plain": "hello" });
        assert!(parse_diff_display(&data).is_none());
    }

    #[test]
    fn parse_attachment_display_reads_prime_mime_payload() {
        let data = serde_json::json!({
            "application/vnd.prime-agent.attachment+json": {
                "mime_type": "image/png",
                "data": "YWJj",
                "path": "C:/tmp/a.png"
            }
        });
        let attachment = parse_attachment_display(&data).expect("attachment");
        assert_eq!(
            attachment,
            CellAttachment {
                mime_type: "image/png".into(),
                data: "YWJj".into(),
                path: Some("C:/tmp/a.png".into()),
            }
        );
    }

    #[test]
    fn parse_attachment_display_rejects_non_image_and_oversized() {
        let non_image = serde_json::json!({
            "application/vnd.prime-agent.attachment+json": {
                "mime_type": "application/pdf",
                "data": "YWJj"
            }
        });
        assert!(parse_attachment_display(&non_image).is_none());

        let oversized = serde_json::json!({
            "application/vnd.prime-agent.attachment+json": {
                "mime_type": "image/png",
                "data": "x".repeat(MAX_ATTACHMENT_DATA_CHARS + 1)
            }
        });
        assert!(parse_attachment_display(&oversized).is_none());
    }

    /// Trace: L2-DES-RLM-001
    /// Verifies: begin_execute can time out without interrupting; wait_until_done finishes later.
    #[tokio::test]
    async fn begin_execute_budgeted_wait_times_out_then_completes() {
        let Some(config) = live_config() else {
            eprintln!("skip: prime-agent-runtime not found");
            return;
        };
        let session = match KernelSession::spawn(config).await {
            Ok(s) => s,
            Err(e) => {
                eprintln!("skip: no kernel ({e})");
                return;
            }
        };
        let inflight = session
            .begin_execute("import time; time.sleep(0.4); print('late')")
            .await
            .expect("begin");
        let outcome = inflight
            .wait_for(Duration::from_millis(50))
            .await
            .expect("wait");
        assert!(
            matches!(outcome, CellWaitOutcome::TimedOut { .. }),
            "expected TimedOut, got {outcome:?}"
        );
        let done = inflight.wait_until_done().await.expect("done");
        assert_eq!(done.status, "ok");
        assert!(
            done.stdout.contains("late"),
            "stdout={:?}",
            done.stdout
        );
    }
}
