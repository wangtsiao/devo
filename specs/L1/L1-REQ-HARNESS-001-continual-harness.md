---
artifact_id: L1-REQ-HARNESS-001
revision: 1
status: Draft
active_baseline: no
supersedes:
superseded_by:
owner: Assistant
last_updated: 2026-09-13
---

# L1-REQ-HARNESS-001 — Continual Harness

## Purpose

Define Continual Harness as a product capability: durable supplemental agent state \(H\) that can be refined from evidence in the recent trajectory, without training weights and without replacing the SKILL.md catalog or the immutable base system prompt.

## Why This Matters

Online adaptation of harness state (prompt notes, memories, skill stubs, subagent specs) improves reuse across turns. Users need a clear trigger (`/refine` and agent-initiated refine), a visible outcome, and rollback — without turning memory into a client-managed database.

## Background / Context

Continual Harness follows [arXiv:2605.09998](https://arxiv.org/abs/2605.09998). Durable state is \(H = (\rho, G, K, M)\):

- \(\rho\) — supplemental prompt notes (not the base system prompt)
- \(G\) — subagent specifications (not Ralph `session/goal`)
- \(K\) — refine-created skill *descriptions* (not the SKILL.md package catalog)
- \(M\) — memories (session-local by default; optional global under user data)

Refine proposes the smallest evidence-backed CRUD edit to \(H\). Skills packages (`L1-REQ-APP-009`) remain first-class. Persistent memory (`L1-REQ-MEM-001`) remains core-maintained; clients do not get a memory browser — only refine trigger and an outcome row.

## User / Business Requirement

The program must support Continual Harness so the agent (or the user via `/refine`) can update supplemental harness state from recent work, with rollback, without replacing skills packages or requiring clients to manage memory entries.

## Real User Scenarios

- A user runs `/refine` after a long debugging session; the agent stores a short memory and shows an outcome row; the next turn sees a harness digest.
- The root session auto-refines after a configured number of user-visible turns; the user can disable auto-refine in settings.
- A user rolls back a bad refinement from the outcome row; workspace file rollback remains a separate action.
- A child `/btw` chat does not auto-refine and does not persist harness files.

## Functional Requirements

- The program must maintain session-local harness state \(H\) for RLM sessions, with optional global harness under user data (not the legacy `~/.devo/memories/` git workspace).
- Users must be able to trigger refine (`/refine` or equivalent Native RPC). The agent must be able to request refine (`refine.run`).
- Root sessions must support interval auto-refine (default on, user-disableable). Goal-continuation turns must not increment the interval counter. Child sessions must not auto-refine.
- Refine outcomes must appear as a user-visible transcript/outcome row (not a memory-browser protocol).
- Users must be able to roll back a refinement’s harness edits without implying workspace file rollback.
- Harness \(K\) must not replace the SKILL.md catalog; harness \(G\) must not replace Ralph goals.
- The immutable base system prompt must not be rewritten by refine.
- Refine-created \(K\)/\(G\) must not widen permissions, sandbox, or MCP grants.
- New-session memory writes go through harness \(M\); Phase 1/2 memory extractors are not a second write path for new sessions.

## Non-Functional Requirements

- Refine planning is an auxiliary model call that must not block the session actor mailbox; apply happens at a turn boundary.
- Auto-refine must not starve the user turn under rate limits (skip auto-refine on 429 / low remaining quota; never skip the user turn).
- Corrupt harness files must fail closed (no silent wipe-to-empty).
- Secrets must be redacted from harness writes that become model-visible.

## Acceptance Criteria

- Given a root RLM session, when the user runs `/refine`, then an evidence-backed harness edit can apply and an outcome row is visible in TUI and desktop.
- Given auto-refine is enabled, when N user-visible assistant turns complete, then a refine may be scheduled; when a Ralph continuation turn completes, then the interval counter does not increment.
- Given a refinement is applied, when the user requests harness rollback for that event, then \(H\) restores that event’s before/after without rewinding workspace files or the REPL namespace.
- Given a skill package exists in the catalog, when refine creates a \(K\) entry, then the catalog skill remains authoritative for package discovery.
- Given Plan Mode is active, when autonomous refine would apply, then apply is suppressed along with goal continuation.

## Out of Scope

- Prime heartbeats, cron, autonomous quality-review gates.
- Client APIs to list/edit/delete individual memory entries.
- Training or fine-tuning.
- Exact JSON schema and file layout (L2).

## Open Questions

- None for product intent; interval default and cooldown values are L2.

## Traceability

| Relationship | Target ID | Target Revision | Target Path | Rationale |
|---|---|---:|---|---|
| refined-by | L2-DES-HARNESS-001 | 1 | specs/L2/harness/L2-DES-HARNESS-001-continual-harness.md | Harness files, refine pipeline, Native events. |
| related-to | L1-REQ-RLM-001 | 1 | specs/L1/L1-REQ-RLM-001-programmatic-runtime.md | Harness requires the RLM kernel/host. |
| related-to | L1-REQ-MEM-001 | 2 | specs/L1/L1-REQ-MEM-001-persistent-memory.md | Outcome row + refine trigger; no memory browser. |
| related-to | L1-REQ-APP-009 | 2 | specs/L1/L1-REQ-APP-009-skills.md | \(K\) does not replace SKILL.md. |
| related-to | L1-REQ-GOAL-001 | 1 | specs/L1/L1-REQ-GOAL-001-ralph-loop.md | \(G\) ≠ Ralph goals. |

## Revision Notes

| Revision | Date | Author | Change Type | Notes |
|---:|---|---|---|---|
| 1 | 2026-09-13 | Assistant | Initial | Draft for RLM host reform. **Pending human approval before implementation authority.** |
