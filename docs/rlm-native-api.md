# RLM Native API — living wire contract

**Audience:** Implementers (Rust server, InteractiveMode adapter, desktop bindings)  
**Authority:** Operational for the RLM host-reform train. If code and this doc disagree, fix both before the next phase slice. L1/L2 remain product/design authority once Approved.

**Last updated:** 2026-09-13

---

## 1. Protocol handshake

- Native `initialize` uses a **dated** `protocol_version` string.
- First-party clients must match exactly — **no negotiate-down**.
- Reject older clients with `UNSUPPORTED_PROTOCOL_VERSION`.
- ACP is not a business protocol this release.

See `crates/protocol/src/native/rpc_admin.rs` and `NATIVE_METHODS` in `methods.rs`.

---

## 2. Method catalog (RLM-relevant)

| Method | Role | Status |
|---|---|---|
| `initialize` | Handshake | done |
| `session/refine/run` | Schedule refine (mid-turn safe) | **P3 done** — schedule only; apply post-`MergeTurn` |
| `session/compact/start` | Host compaction | extend for agent-requested (turn-end schedule via `compact.run`) |
| `context/usage/read` | Context tokens/percent | reused by `compact.status` |
| `agent/message` | Agent family delivery | extend for kernel `agent_message.*` |
| Turn / session / Ask reverse-RPC | Existing Native | keep |

Idempotency and params: follow existing OpenRPC / method registry; changelog below when shapes change.

---

## 3. Item / event shapes

### `ipython` tool details (Prime-compatible target)

```json
{
  "stdout": "",
  "stderr": "",
  "result": null,
  "status": "ok",
  "errorName": null,
  "errorValue": null,
  "traceback": [],
  "durationMs": 0,
  "diffs": [],
  "sentAgentMessages": [],
  "backgroundOutput": null
}
```

### `Item::Refinement`

Native item → InteractiveMode `refinement_outcome` with `edits[]` via adapter projection.

### Received notices

| Kind | Projection |
|---|---|
| Agent message received | `agent_message` / queue preview |
| Background command finished | `async_bash_completion` |

### Waiting phases

`kernel` | `host_request` | `refine`

---

## 4. `host_request` action table

| Action | Host behavior | Ask? | Status |
|---|---|---|---|
| `rlm.run` | Spawn child session | no | stub (handle metadata) |
| `rlm.find_models` / `list_subagents` / `delete_subagent` | Roster | no | stub (empty) |
| `rlm.create_session` | **Deny** (daemon OOS) | — | deny |
| `bash` / `bash.start` | Ask preflight, then in-kernel runner | yes | pending (structured) |
| `bash.completed` / `bash.consumed` | Inject / withdraw wake notice | no | in-memory queue |
| `goal.*` | Map to `session/goal/*` | no | stub |
| `compact.status` / `compact.run` | Façade over `context/usage/read` + pending flag; `run` schedules turn-end only (never mid-ipython) | no | done (schedule + status) |
| `refine.run` / `refine.status` | Schedule only mid-cell | no | done (schedule) |
| `mcp.call` / list | Rust MCP under Ask | yes | pending |
| `write` / `edit` | Mutate under Ask + diff MIME | yes | pending |
| `web.search` / `web.fetch` | Existing adapters | yes | pending |
| `model.info` | Vision / model caps | no | stub |
| `question` | Native userInput | yes | pending |
| `agent_message.send` | Family steer/queue | no | stub |
| `agent_observe.*` | Roster / recent | no | stub (empty) |
| `rlm_heartbeat.*` | Deny | — | deny |

`host_reply` **must bypass** any execute FIFO (Prime deadlock rule). Wired in `devo-kernel` (separate stdin lock + `HostRequestHandler`); server table in `runtime/kernel_host.rs`. When a kernel is live, the turn uses the Rlm (ipython-only) registry plan.

---

## 5. Kernel session lifecycle

| Step | Behavior |
|---|---|
| Spawn | `python -m rlm.repl`, protocol v3; sandbox wrap at spawn only |
| Bootstrap | Import `rlm` / skills; **no** `rlm.create_session` assert |
| Snapshot | `kernel-state.dill` + `kernel-state.json` under session artifact dir |
| Restore | Missing dill → empty namespace (Prime semantics) |
| Interrupt | REPL interrupt + turn cancel |
| Shutdown | `drainHostRequests` then shutdown; orphan bash reaper |
| Ownership | Session-scoped `Arc<KernelSession>`; turn task I/O only |

Env: `DEVO_RLM_RUNTIME_SRC` → vendored `vendor/prime-agent-runtime/src`.

---

## 6. Harness

| Item | Value | Status |
|---|---|---|
| State file | Session-dir `harness/harness_state.json` | done |
| Digest inject | `HarnessDigestInjector` → `QueryOptions.harness_digest` (after skills, before goal) | **P3 done** |
| Refine RPC | `session/refine/run` schedules; apply at turn boundary via `apply_proposal_re_read` | **P3 done** (planning still placeholder) |
| `refine.run` host_request | Schedule only mid-cell; never apply inside ipython | **P3 done** (schedule path) |
| Persist | `Item::Refinement` + `refinements.jsonl` on successful apply | **P3 done** |
| Post-turn order | **compact → refine → continue → queue → goal → title** | **P3 wired**; `take_pending_compact` consumed at boundary; **agent-requested compact execution** (invoke `run_session_compaction` + post-compact continue) still **P4** |
| Auto-interval | Root default on, N≈25; children off; Plan Mode defers autonomous apply | **P3 done** (`is_goal_continuation` still hard-coded false until TurnInputMode is threaded) |

---

## 7. Prompt assembly order

1. Immutable RLM base (root vs child blocks)
2. Subagent / spawn guidance (root)
3. MCP section (if enabled)
4. Project context (AGENTS.md, etc.)
5. Catalog skills XML
6. Harness digest (conversation layer)
7. Goal

---

## 8. Built-in skill → host_request map

### Bootstrap import list (kernel pre-import)

Always inject globals (`rlm`, `bash`, `mcp`, `rlm.harness` / `get_harness_state`) and pre-import these Python packages from `crates/core/rlm_skills/` (see `RLM_BOOTSTRAP_SKILL_IMPORTS`):

| Import | Package | Status |
|---|---|---|
| `compact` | `crates/core/rlm_skills/compact/` | **stub** — raises `NotImplementedError("host_request stub until P2")` |
| `refine` | `crates/core/rlm_skills/refine/` | **stub** — raises `NotImplementedError("host_request stub until P2")` |
| `goal` | `crates/core/rlm_skills/goal/` | **stub** — raises `NotImplementedError("host_request stub until P2")` |

Do **not** bootstrap or teach `rlm.create_session` (daemon path denied).

### Catalog → host_request

| Skill | Status |
|---|---|
| compact | helpers (`compact.status` / `compact.run` schedule; host execute at turn end) |
| refine | stub (P1b package; P2 schedule; P3 apply) |
| goal | stub (P1b package; P2 → `goal.*` → `session/goal/*`) |
| edit | todo (host_request rewrite) |
| websearch | todo (adapt, no Serper) |
| agent-message / agent-observe | todo |
| rlm-heartbeat / prime-intellect | skip |

Prompt helpers: `devo_core::build_rlm_base_prompt` (root vs child immutable doctrine) and `devo_core::format_catalog_skills_xml` (progressive disclosure). Markdown `render_available_skills_body` remains for non-RLM surfaces.

---

## 9. Verification

1. `cargo build` (workspace)
2. End-to-end: server + kernel + **new InteractiveMode** via tmux (`devo-tui-test`)
3. Never point tmux at legacy `crates/tui`

InteractiveMode tmux suite (new TUI only):

```bash
cd clients/interactive-mode && npm run test:tmux
```

See `clients/interactive-mode/scripts/tmux-tui-smoke.sh` and `clients/interactive-mode/README.md` (T0–T10 matrix; T0–T3 required early P5 via adapter smoke).

---

## 10. Changelog

| Date | Change |
|---|---|
| 2026-09-13 | Doc created; baseline spike + host_request table (mostly todo). |
| 2026-09-13 | **P1:** Session-scoped `Arc<KernelSession>` on `SessionActorState`; wired in `query.rs` / `approval_resume.rs` via `ensure_kernel`; `snapshot`/`restore`/`list_names` on kernel; `durationMs` on ipython details; Desktop-hardcoded PYTHONPATH removed (use `vendor/…` or `DEVO_RLM_RUNTIME_SRC`). |
| 2026-09-13 | **P1b:** `rlm_prompts` + `crates/core/rlm_skills` stubs (compact/refine/goal). |
| 2026-09-13 | **P2:** `HostRequestHandler` on `KernelSession` (separate stdin lock for `host_reply`; `set_host_handler`); `runtime/kernel_host.rs` dispatch table; compact/refine schedule; Ask actions return structured pending; bash notices in-memory; live kernel → Rlm registry plan. |
| 2026-09-13 | **P3:** Continual Harness host wiring — `HarnessDigestInjector` + `QueryOptions.harness_digest`; post-`MergeTurn` order compact→refine→queue→goal→title; `apply_proposal_re_read` + `Item::Refinement` / `refinements.jsonl`; root `should_auto_refine` interval; abort clears pending compact+refine. |
| 2026-09-13 | **P5:** InteractiveMode adapter T2/T3 smoke (`--smoke-prompt-cell` / `--smoke-interrupt`) + `projectRefinementOutcome`; tmux suite runs T0–T3. |
| 2026-09-13 | **P4:** `compact_host` status/run schedule; harness digest strip/reattach helpers. |
| 2026-09-13 | **P4:** `compact.status` / `compact.run` host helpers (`runtime/compact_host.rs`); harness digest strip/reattach; turn-end order locked compact-before-refine. |
| 2026-09-13 | **P1b:** `build_rlm_base_prompt` + catalog skills XML; `crates/core/rlm_skills/{compact,refine,goal}` Python stubs; section 8 bootstrap import list marked stub until P2. |
