# `devo-kernel`

CPython RLM REPL host (protocol v3). Spawns `python -m rlm.repl`.

## Runtime path

Set `DEVO_RLM_RUNTIME_SRC` to the directory that contains the `rlm` package,
or use the default `crates/kernel/rlm-runtime/src`.

## Host requests

`KernelSession` accepts an optional `HostRequestHandler`. During `execute`,
`host_request` events invoke the handler and write `host_reply` on a **separate
stdin lock** (Prime deadlock rule). Default deny when unset.

Server dispatch lives in `crates/server/src/runtime/kernel_host.rs`.

## Execution surface

When a session kernel is available, the turn uses `ExecutionSurface::Rlm`
(root tools: `ipython`, `bash`, plus MCP; hosted `web_search` on
anthropic/responses wires). Discrete remains the bootstrap default when no
kernel can spawn. Do not ship `execution_surface` as a product config flag.

Trace: `L2-DES-RLM-001`.
