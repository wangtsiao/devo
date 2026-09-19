# DEVO_PATCHES — divergence from upstream `windows-sandbox-rs`

This crate is a vendored fork of OpenAI codex's `codex-rs/windows-sandbox-rs`
(Apache-2.0). Baseline snapshot: v0.150.1 (2026-09 comparison). Per the project
goal, every hunk that differs from upstream after a sync must be listed here.

Sync procedure:

```sh
for f in $(ls <codex>/codex-rs/windows-sandbox-rs/src/*.rs | xargs -n1 basename); do
  diff --strip-trailing-cr <codex>/codex-rs/windows-sandbox-rs/src/$f src/$f
done
```

## Known intentional deltas (devo ahead — keep when syncing)

- `cap.rs`: `normalize_verbatim_prefix` strips `\\?\` / `\\?\UNC\` before
  `starts_with`/`components()` in `workspace_write_root_contains_path` /
  `specificity` (upstream compares raw canonicalized paths and can mis-match
  verbatim-prefixed roots).
- `allow.rs`: `path_exists_lenient`.
- `setup.rs` + `bin/setup_main/win.rs`: `#[serde(alias = "codex_home")]` and
  `CODEX_HOME` fallback for marker/payload compatibility.
- `windows_impl.rs`: WAIT_FAILED-safe wait loop; kill-on-close job.
- devo-added integration files (no upstream counterpart): `launch.rs`,
  `windows_impl.rs`, `request_adapter.rs`, `path_util.rs`, `string_util.rs`,
  `capture_stub.rs`, `otel_stub.rs`, `protocol/`, `unified_exec/`, `conpty/`,
  `pty/`, `elevated/`, and `credential_delivery.rs`.
- `credential_delivery.rs` is the one devo-owned *security* module (goal rule
  5): codex's per-exec model never delivers credentials to a long-lived
  process. Per-session capability SID + inheritable allow-ACE grant/revoke
  (write and read masks) + crash-recovery journal (`credential_journal.json`) +
  orphan sweep. Upstream cannot host it by design.

## Credential-delivery plumbing (devo-owned hunks in upstream files, P2)

All of these exist only to carry the per-session credential SID to the
sandboxed long-lived kernel; codex has no equivalent need:

- `wrapper.rs`: `--session-credential-sid` flag (builder param, wrapper-request
  field, parse arm).
- `token.rs`: `create_readonly_token_with_caps_user_and_additional_restrictions_from`
  and `create_workspace_write_token_with_caps_and_additional_restrictions_from`
  (identity markers; deliberately excluded from the default DACL) +
  `additional_restricting_tests`.
- `WindowsSandboxSessionRequest` / `WindowsSandboxRequest`: `session_credential_sid`
  field, threaded through `unified_exec/mod.rs` → `backends/legacy.rs` →
  `spawn_prep.rs` (`prepare_legacy_session_security` param).
- `launch.rs`: `prepare_direct_argv_launch` + `prepare_windows_sandbox_launch_for_argv`
  (direct-argv launcher for non-shell callers).
- Callers pass `None` everywhere except the RLM kernel fence path
  (`crates/kernel/src/fence.rs`).

## Ports applied 2026-09-19 (step 1, from upstream v0.150.1)

- `audit.rs`: deny-ACE application failures now aggregate into an `Err`
  (previously logged and swallowed → world-writable dirs stayed world-writable
  with preflight green). Injected-apply-fn split + regression test, ported from
  upstream `audit.rs`.
- `setup.rs`: `setup_refresh_deny_read_paths` — non-elevated setup refresh now
  re-resolves deny-read paths (exact + glob) from the permission profile and
  passes them as `SetupRootOverrides.deny_read_paths` in
  `run_setup_refresh` / `run_setup_refresh_with_extra_read_roots` (previously
  `None` → deny-read ACEs never refreshed). Divergence note: devo's protocol
  has no skip-missing entry behavior, so the upstream
  `remove_skip_missing_path_entries()` call is omitted.

## Known upstream-behind gaps (to close on next sync — see design doc)

- Proxy port wire (`DEVO_WINDOWS_SANDBOX_PROXY_PORTS` env + marker drift +
  provisioning settings) — fixes ephemeral-port-vs-static-firewall mismatch.
- `acl.rs`: `INHERITED_ACE`-aware refresh scope (upstream `AceScope::Explicit`)
  — devo's effective-scope check causes perpetual write-ACE refresh churn.
- `deny_read_resolver.rs`: pre-validate glob scan plans (root-anchored globs
  without depth bound currently scan unbounded from the drive root).
- `helper_materialization.rs`: junction-safe helper lookup retry.
- `setup.rs`: `SEE_MASK_NOASYNC` + null stdin on ShellExecuteEx; no-reparse
  HANDLE for ProvisionOnly DACL mutation; symbolic-root read gating.
