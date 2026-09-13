---
artifact_id: L2-DES-HARNESS-001
revision: 1
status: Draft
active_baseline: no
supersedes:
superseded_by:
owner: Assistant
last_updated: 2026-09-13
---

# L2-DES-HARNESS-001 — Continual Harness

## Purpose

Define Continual Harness persistence, kernel CRUD, host `refine.run` / auto-interval, Native events, and relationship to MEM-001 / skills / goals. Refines `L1-REQ-HARNESS-001`.

## Source Requirements

- `L1-REQ-HARNESS-001`
- `L1-REQ-MEM-001` Rev 2
- `L1-REQ-APP-009` Rev 2
- `L1-REQ-RLM-001` / `L2-DES-RLM-001`
- `L2-DES-CONV-002` — auto-refine settings persist-first + DD-6
- `L2-DES-SERVER-002` — refine planning off the actor mailbox

## Status

**Draft.** Pending human approval. Kernel spike omits `/refine`. Implementation lands after `host_request` exists, in the same breaking major.

## Design Decisions

### DD-1: State \(H = (\rho, G, K, M)\)

| Symbol | Kind | Meaning |
|---|---|---|
| \(\rho\) | `prompt` | Supplemental notes — never the immutable base system prompt id |
| \(G\) | `subagent` | Specs for `rlm.spawn` — **not** Ralph `session/goal` |
| \(K\) | `skill` | Refine-created skill descriptions — **not** SKILL.md catalog |
| \(M\) | `memory` | Memories — session-local default; optional global under `~/.devo` |

Global harness path is **not** `~/.devo/memories/` (legacy Phase 1/2 git workspace). Breaking release: disable Phase 1/2 extractors and memory tools for new sessions; \(M\) is the write path. Redaction applies to refine writes.

### DD-2: Shared file ownership

Session files under session dir: `harness/harness_state.json`, `refinements.jsonl`. Optional global under `~/.devo/harness/`.

**Shared file** like Prime: kernel `rlm.harness` CRUD hits the file; host re-reads immediately before apply and rejects per-entry drift (`entry changed during refinement planning`). Atomic replace; no flock beyond that. Env: `RLM_HARNESS_STATE_DIR`, `RLM_GLOBAL_HARNESS_STATE_DIR`.

**Corrupt JSON: fail closed** (backup/refuse). Timestamped copies (keep N); doctor can restore last good; schema bump copies first. One schema and one id generator (host); Python loader must match Rust (no Prime TS-vs-Python divergence).

Children: local copy of \(H\) then isolate; must not write parent’s local file; **default deny child writes to global**.

### DD-3: Triggers

1. User `/refine` → Native `session/refine/run`
2. Kernel `host_request("refine.run")` / `await refine.run()`
3. **Root auto-interval** — default **on**, user setting to disable; every N successful **user-visible** assistant turns (default 25). **Goal-continuation turns do not count.** No reviewer/quality gate. Compact may offer `reason: "compact"` auto-refine under monotonic cooldown (~20 min).

Children: no auto-refine, no `refine.run` host RPC (Prime). Abort drops pending `refine.run`; steer does not. Idle `/refine` while streaming is queued (`skipAbort`). Second `refine.run` in same turn: last-write-wins with field merge. Apply never mid-turn; interactive planning may overlap user work; apply at turn boundary / idle.

Auto-refine enable/interval are `SessionSettingsPatch` fields with CONV-002 DD-6 rows.

### DD-4: Host planning and apply

Rust owns planning via `crates/provider` (session model, not `small_model`). Spawned task off the session actor (title-polish pattern). Separate `in_flight` from title polish; cancel on delete/fork.

Apply at turn boundary (post-`MergeTurn` step 1 per `L2-DES-RLM-001` DD-15). Persist-first. Project Native `Item::Refinement`. Adapter maps to InteractiveMode outcome row.

Refine tokens → `UsagePurpose::Refine`; count against goal budget. Plan Mode: no autonomous apply.

On 429/`Retry-After` / low remaining-%: skip auto-refine, never skip user turn.

### DD-5: Context digest

Inject harness digest into cold boundaries (resume/compact/new-turn), after skills, before hidden goal. Include when to call `await refine.run()`. Mid-session apply → `[refinement]` notice; do not rebuild immutable prefix.

### DD-6: Rollback

`refinements.jsonl` undo log. Restore that event’s before/after only (not stack replay). Independent of workspace `/rollback`. Desktop palette shows both undo logs; H-rollback only from Refinement row.

### DD-7: Safety

Refine-created \(K\)/\(G\) are routing metadata — cannot add tools, MCP, or sandbox grants. Block editing immutable base-prompt id. Security Mode (`agent_mode`) extra \(\rho\) is session metadata, not harness-editable. Prompt notes can change behavior via digest — treat as model-visible user data; redact secrets.

### DD-8: Kernel module

Vendor/adapt Prime `harness.py` as `rlm.harness`. Host `/refine` writes must be visible on kernel re-read (mtime reload).

### DD-9: Out of scope

Prime heartbeats/cron, auto-reviewer, extension hooks, ACP `_meta`, memory-browser client API, AUTO-001 automations (interval auto-refine is **not** AUTO-001).

## Traceability

| Relationship | Target ID | Target Revision | Target Path | Rationale |
|---|---|---:|---|---|
| refines | L1-REQ-HARNESS-001 | 1 | specs/L1/L1-REQ-HARNESS-001-continual-harness.md | Continual harness design. |
| related-to | L2-DES-MEM-001 | 2 | specs/L2/memory/L2-DES-MEM-001-persistent-memory-architecture.rev2-draft.md | \(M\) absorbs new-session writes. |
| related-to | L2-DES-RLM-001 | 1 | specs/L2/rlm/L2-DES-RLM-001-kernel-host.md | Kernel + host_request prerequisite. |

## Revision Notes

| Revision | Date | Author | Change Type | Notes |
|---:|---|---|---|---|
| 1 | 2026-09-13 | Assistant | Initial | Draft from reform plan. **Pending human approval.** |
