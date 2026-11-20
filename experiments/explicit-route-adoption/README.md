# Explicit route adoption: offline contract prototype, NOT a product feature

This small, zero-dependency Node experiment exercises the approval/data contract
for the missing activation step. It is not an authentication tool, RPC server,
config file format, Rust implementation, or UI. The snapshots use existing
`providers.json` / `custom-providers.json` shapes with synthetic credential
references only. No secrets, network calls, disk writes, provider validation,
credential creation, login, or implicit activation occur.

Run: `node --test experiments/explicit-route-adoption/adoption.test.mjs`

`prepare(snapshot)` returns the exact provider/model/endpoint/protocol/request
model and resulting request URL for explicit review. Its digest binds the
proposal to both config snapshots, route and auth kind. `accept(current, digest)`
requires a matching approval and returns one **unwritten candidate document**.
The digest is stale-state detection, NOT proof that a human authorized anything.
A real client/server must establish explicit user intent independently; do not
accept an untrusted client claiming `authKind: api_key` or supplying its own
catalog route. The same-state request is deterministic. A subsequent applied
manual pin is protected and requires a fresh explicit repair workflow, not a
blind replay. There is no new persisted approval-token format.

## Production integration contract

1. Keep the reviewed catalog patch isolated. Do not weaken
   `core/model_catalog.rs::merge_user_connections`. Obtain selectable route
   proposals from public discovery separately from effective execution routes.
2. Add explicit route adoption to the existing Native provider/model connection
   action and generated schema, not to ordinary session model selection. Display
   the exact proposed request target before approval. `rpc_admin.rs`, TUI
   `host.ts::completeHostProviderAuthentication` and the relevant provider editor
   are integration points; this experiment does not modify them.
3. Server verifies actual existing credential kind and binding (no secret copied
   into the proposal), exact catalog identity/tuple and unchanged user config.
   Re-read both authored files and selected model overrides; reject any explicit
   complete/partial transport in either file, including shadowed and blank fields.
   OAuth adapters are not candidates. Preserve unrelated model pins and defaults.
4. Reuse or narrowly extend `config/provider_connection.rs::pin_provider_model`,
   not whole-provider upsert. The current primitive permits replacement and has no
   CAS: it needs validated expected-state input plus a serialization boundary
   shared with other writers. Preserve its owning-file selection. Do not copy
   remote metadata or change credential ownership, names, capability overrides,
   session/global model defaults. The desired mutation is exactly the selected
   model's `base_url`, `wire_api`, `request_model`.
5. `write_provider_catalog_config` can atomically replace one document. The
   comparison and write must be under the same writer gate; the pure prototype
   does NOT prevent external file changes between read and write. External writers
   require an explicit lock/version contract, not a claim of transactional CAS.
   Existing credential/set and provider/upsert write auth/config separately:
   this prototype intentionally operates only on an existing credential binding
   and does not claim to solve cross-file login atomicity.
6. Reload routers only after route persistence succeeds; then enable the selected
   model. On failure leave activation incomplete and send no model request. Do not
   use provider/validate with real keys to probe protocol support. Preserve logout
   generation fences before/after asynchronous client work.

## #405 relationship and residual gates

#405 and its local recovery are untouched. Its API-key explicit-login pin uses
provider-level transport; it cannot simply elevate this selected model route to
the entire provider. Decide the final selected-model Native contract before
integrating the two changes. A complete existing route is not silently upgraded
by reconnect, startup, refresh, retry, or normal model selection.

Six Node tests pass here: exact tuple/display, owner/binding/default preservation,
stale approvals, complete/partial/shadowed pins, OAuth/ambiguous/missing binding,
and explicit approval/path representability. These are contract tests only.
Production Rust config tests, generated Native protocol checks, independent review,
real explicit UI interaction and final CI remain required. Rust tooling is not
available in the current environment. End-user activation is NOT implemented.
