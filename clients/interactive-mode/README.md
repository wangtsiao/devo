# Devo InteractiveMode client

Product TUI per `L2-DES-RLM-001` DD-7 / DD-12:

- Launch: `devo` exec/replaces onto Node/bun, **inherits TTY** (no piped sidecar).
- Protocol: Native dated `initialize` via [`NativeAgentConnection`](src/native-agent-connection.js) (not ACP, not Prime daemon).
- Config: `~/.devo` / `auth.json` — never `~/.prime`.
- Strip Prime-only commands (`STRIP_PRIME_COMMANDS`); map Devo `/refine` → `session/refine/run`, plus `/btw`, `/goal`, etc.
- Pin **Prime git revision** under `vendor/` (MIT NOTICE); do not publish `@earendil-works`.
- Agents View disabled (`returnToAgentsView: false`).

## Status

| Piece | Status |
|---|---|
| `NativeAgentConnection` P5 MVP (subscribe/snapshot/state/messages, prompt/abort/queue/steer, compact/refine, heartbeats stub, session new/switch/fork, ipython helper) | Implemented (`src/native-agent-connection.js`) |
| Vendored InteractiveMode UI | Pending pin under `vendor/` |
| Rust CLI launcher exec onto this package | Pending cutover (then delete `crates/tui`) |

## Tests

Unit tests (any OS):

```bash
cd clients/interactive-mode
npm test
```

### tmux scenario suite (new InteractiveMode TUI only)

**Product under test:** this package (`node src/index.js`) + Native adapter.

**Not under test:** legacy Rust ratatui in `crates/tui`. Do not point tmux at the old TUI binary.

Requires **tmux** (Unix / Linux CI). On Windows without tmux the script exits 0 with a skip message.

```bash
cd clients/interactive-mode
npm run test:tmux
# or a single scenario (Unix with tmux + bash):
npm run test:tmux -- T0
npm run test:tmux -- T1
# equivalent:
bash scripts/tmux-tui-smoke.sh T0
```

Safety:

- Session name only: `DEVO_TUI_TEST_SESSION` (default `devo-tui-test`)
- Creates/kills **that session only** — never `tmux kill-server`
- Trap cleanup on EXIT

Scenario matrix (see comments in `scripts/tmux-tui-smoke.sh`):

| ID | Status |
|---|---|
| T0 Launch under TTY | Implemented |
| T1 Idle / composer markers (`--smoke-adapter`) | Implemented (until IM vendored) |
| T2 Prompt → cell (`--smoke-prompt-cell`) | Implemented (adapter smoke; UI pending) |
| T3 Interrupt → idle (`--smoke-interrupt`) | Implemented (adapter smoke; UI pending) |
| T4–T10 | `skip` until InteractiveMode is vendored |
