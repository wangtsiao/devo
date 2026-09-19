//! OS fence wiring for the RLM kernel (design doc `docs/design/rlm-permissions.md`
//! §5.2/§5.3, P0).
//!
//! Windows: the kernel argv is wrapped in the restricted-token sandbox launcher
//! (`devo.exe --run-as-windows-sandbox ... python -m rlm.repl`). Linux/macOS
//! wiring (bwrap strict profile / Seatbelt) is P0 follow-up; until then a fence
//! request on those platforms is an explicit downgrade, never a silent one.
//!
//! Downgrade semantics (§5.3): if the sandbox is not provisioned — or the
//! wrapped launch fails — the kernel still spawns unfenced, but the downgrade is
//! loud (`tracing::warn`) and the caller is expected to surface it in the UI and
//! record a `fence-off` event. The one hard exception stays with the approval
//! pipeline: profiles with deny-read paths never drop their sandbox.

use crate::session::KernelFenceSpec;
use std::process::Stdio;
use tokio::process::Command;

/// Outcome of the fence request for a spawned kernel — recorded on the
/// session so callers (core/server) can emit rollout events and UI warnings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FenceState {
    /// No fence requested; spawn bare by design.
    NotRequested,
    /// Sandbox provisioned; kernel argv wrapped in the sandbox launcher.
    Fenced,
    /// Fence requested but unavailable; kernel runs unfenced with an explicit,
    /// logged downgrade (never silent).
    DowngradedUnfenced,
}

/// Pure decision for the fence path — testable without touching the machine's
/// provisioning state.
pub(crate) fn decide_fence(
    requested: Option<&KernelFenceSpec>,
    sandbox_ready: bool,
) -> FenceState {
    match requested {
        None => FenceState::NotRequested,
        Some(_) if sandbox_ready => FenceState::Fenced,
        Some(_) => FenceState::DowngradedUnfenced,
    }
}

/// Result of the Windows fence application: the launch command, the decision,
/// and — when fenced — the per-session credential authority whose SID traveled
/// in the kernel's restricted token. Later `grant_write_root` calls on it
/// deliver write access to the living kernel without a restart (§9, P2).
#[cfg(windows)]
pub(crate) struct FenceOutcome {
    pub command: Command,
    pub state: FenceState,
    pub credentials: Option<devo_windows_sandbox::SessionCredentialAuthority>,
}

#[cfg(windows)]
pub(super) fn wrap_or_bare(
    config: &crate::session::KernelSessionConfig,
    bare: Command,
) -> FenceOutcome {
    let Some(spec) = config.fence.as_ref() else {
        return FenceOutcome {
            command: bare,
            state: FenceState::NotRequested,
            credentials: None,
        };
    };
    let devo_home = match devo_util_paths::find_devo_home() {
        Ok(home) => home,
        Err(err) => {
            tracing::warn!(
                error = %err,
                "RLM kernel fence requested but devo home is unavailable; kernel runs UNFENCED \
                 with full user permissions (explicit downgrade, design doc §5.3)"
            );
            return FenceOutcome {
                command: bare,
                state: FenceState::DowngradedUnfenced,
                credentials: None,
            };
        }
    };
    let ready = devo_windows_sandbox::sandbox_setup_is_complete(&devo_home);
    match decide_fence(config.fence.as_ref(), ready) {
        FenceState::NotRequested => FenceOutcome {
            command: bare,
            state: FenceState::NotRequested,
            credentials: None,
        },
        FenceState::DowngradedUnfenced => {
            tracing::warn!(
                "RLM kernel fence requested but the Windows sandbox is not provisioned; \
                 kernel runs UNFENCED with full user permissions (explicit downgrade, \
                 design doc §5.3). Run the Windows sandbox setup to enable the fence."
            );
            FenceOutcome {
                command: bare,
                state: FenceState::DowngradedUnfenced,
                credentials: None,
            }
        }
        FenceState::Fenced => {
            // P2: mint the per-session credential authority. Its SID travels in
            // the kernel's restricted token (identity marker, no access by
            // itself); the retained authority delivers later grants as ACEs.
            let session_id = format!("kernel-{}", uuid::Uuid::new_v4());
            let credentials =
                match devo_windows_sandbox::SessionCredentialAuthority::new(&session_id, &devo_home)
                {
                    Ok(authority) => authority,
                    Err(err) => {
                        tracing::warn!(
                            error = %err,
                            "RLM kernel session credential authority could not be minted; \
                             kernel runs UNFENCED (explicit downgrade, design doc §5.3)"
                        );
                        return FenceOutcome {
                            command: bare,
                            state: FenceState::DowngradedUnfenced,
                            credentials: None,
                        };
                    }
                };
            let request = devo_windows_sandbox::WindowsSandboxRequest {
                // Shell-shaped fields are ignored by the direct-argv launcher.
                command: String::new(),
                shell_program: String::new(),
                shell_args: Vec::new(),
                cwd: config.cwd.clone(),
                readable_roots: spec.readable_roots.clone(),
                writable_roots: spec.writable_roots.clone(),
                deny_read: spec.deny_read.clone(),
                restrict_network: spec.restrict_network,
                session_credential_sid: Some(credentials.sid().to_string()),
            };
            let argv = vec![
                config.python.to_string_lossy().into_owned(),
                "-m".to_string(),
                "rlm.repl".to_string(),
            ];
            match devo_windows_sandbox::prepare_windows_sandbox_launch_for_argv(&request, argv) {
                Ok(launch) => {
                    let mut cmd = Command::new(launch.program);
                    cmd.args(launch.args)
                        .current_dir(&config.cwd)
                        .stdin(Stdio::piped())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::piped())
                        .kill_on_drop(true);
                    for (key, value) in &launch.env {
                        cmd.env(key, value);
                    }
                    FenceOutcome {
                        command: cmd,
                        state: FenceState::Fenced,
                        credentials: Some(credentials),
                    }
                }
                Err(err) => {
                    tracing::warn!(
                        error = %err,
                        "RLM kernel fence launch failed; kernel runs UNFENCED with full user \
                         permissions (explicit downgrade, design doc §5.3)"
                    );
                    FenceOutcome {
                        command: bare,
                        state: FenceState::DowngradedUnfenced,
                        credentials: None,
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::decide_fence;
    use super::FenceState;
    use crate::session::KernelFenceSpec;
    use std::path::PathBuf;

    fn spec() -> KernelFenceSpec {
        KernelFenceSpec {
            readable_roots: vec![PathBuf::from(r"C:\proj")],
            writable_roots: vec![PathBuf::from(r"C:\proj")],
            deny_read: Vec::new(),
            restrict_network: true,
        }
    }

    #[test]
    fn no_request_spawns_bare_by_design() {
        assert_eq!(decide_fence(None, false), FenceState::NotRequested);
        assert_eq!(decide_fence(None, true), FenceState::NotRequested);
    }

    #[test]
    fn provisioned_sandbox_fences() {
        assert_eq!(
            decide_fence(Some(&spec()), true),
            FenceState::Fenced
        );
    }

    #[test]
    fn unprovisioned_sandbox_is_an_explicit_downgrade_never_silent() {
        assert_eq!(
            decide_fence(Some(&spec()), false),
            FenceState::DowngradedUnfenced
        );
    }
}
