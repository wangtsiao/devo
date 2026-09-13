//! CPython RLM REPL kernel host (REPL protocol v3).
//!
//! Spawns `python -m rlm.repl`, speaks newline-delimited UTF-8 JSON, and keeps
//! a session-scoped namespace across turns. See `L2-DES-RLM-001`.

mod protocol;
mod session;

pub use protocol::{
    KernelEvent, KernelRequest, PROTOCOL_VERSION, ReadyEvent, ReplError,
};
pub use session::{
    default_runtime_pythonpath, deny_host_handler, parse_host_request_data, CellOutput,
    ExecutionSurface, HostRequestHandler, KernelSession, KernelSessionConfig, SpawnError,
};
