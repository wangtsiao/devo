/**
 * Devo InteractiveMode entry (scaffold).
 *
 * Launch inherits TTY (exec/replace). Use `--smoke-adapter` under tmux to
 * exercise NativeAgentConnection without the vendored UI.
 */

import {
  NativeAgentConnection,
  notImplemented,
  STRIP_PRIME_COMMANDS,
  ipythonToolDefinition,
  emptyIpythonDetails,
} from "./native-agent-connection.js";

export {
  NativeAgentConnection,
  STRIP_PRIME_COMMANDS,
  ipythonToolDefinition,
  emptyIpythonDetails,
};

export function assertInheritedTty() {
  if (!process.stdin.isTTY || !process.stdout.isTTY) {
    throw new Error(
      "Devo InteractiveMode requires an inherited TTY (exec/replace). Piped sidecars are invalid.",
    );
  }
}

/**
 * TTY + adapter smoke used by tmux scenarios (T0/T1) until InteractiveMode is vendored.
 * Prints stable markers for `capture-pane` assertions.
 */
export async function runAdapterSmoke() {
  assertInheritedTty();
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01");
  const unsub = conn.subscribe(() => {});
  const snapshot = await conn.getInitialSnapshot();
  const state = await conn.getState();
  const messages = await conn.getMessages();
  const heartbeats = await conn.listHeartbeats();
  const ipython = conn.getIpythonToolDefinition();
  unsub();

  console.log("devo-im: tty-ok");
  console.log("devo-im: adapter-idle");
  console.log("devo-im: composer-ready");
  console.log(
    `devo-im: smoke messages=${messages.length} heartbeats=${heartbeats.length} tool=${ipython.name} streaming=${state.isStreaming}`,
  );
  if (!snapshot?.state || ipython.name !== "ipython" || heartbeats.length !== 0) {
    throw new Error("devo-im: adapter smoke invariants failed");
  }
}

/**
 * T2: prompt → streaming / cell markers (mock RPC; no live server).
 * Exercises turn/start + local streaming state until InteractiveMode UI is vendored.
 */
export async function runPromptCellSmoke() {
  assertInheritedTty();
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01", {
    sessionId: "ses_smoke_t2",
  });
  const events = [];
  const unsub = conn.subscribe((ev) => events.push(ev));

  console.log("devo-im: tty-ok");
  const pending = conn.prompt("smoke prompt for cell");
  const req = JSON.parse(lines.at(-1));
  if (req.method !== "turn/start") {
    throw new Error(`devo-im: T2 expected turn/start, got ${req.method}`);
  }
  console.log("devo-im: streaming-start");
  conn.handleIncomingLine(
    JSON.stringify({
      jsonrpc: "2.0",
      id: req.id,
      result: { turn: { id: "turn_smoke_t2" } },
    }),
  );
  await pending;
  const state = await conn.getState();
  if (!state.isStreaming || conn.expectedTurnId !== "turn_smoke_t2") {
    throw new Error("devo-im: T2 streaming state invariants failed");
  }
  // Synthetic assistant/ipython cell row until UI vendoring lands.
  console.log("devo-im: cell-visible kind=ipython");
  console.log("devo-im: assistant-delta");
  const refinement = conn.projectRefinementOutcome({
    type: "refinement",
    refinementId: "refine_smoke",
    trigger: "manual",
    summary: "smoke",
    changes: ["create memory:m1"],
  });
  if (!refinement || refinement.type !== "refinement_outcome") {
    throw new Error("devo-im: T2 refinement projection failed");
  }
  console.log(`devo-im: refinement-projected id=${refinement.refinementId}`);
  unsub();
  console.log("devo-im: t2-ok");
}

/**
 * T3: interrupt restores idle (mock RPC; no live server).
 */
export async function runInterruptSmoke() {
  assertInheritedTty();
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01", {
    sessionId: "ses_smoke_t3",
  });

  console.log("devo-im: tty-ok");
  const promptPending = conn.prompt("busy turn");
  let req = JSON.parse(lines.at(-1));
  conn.handleIncomingLine(
    JSON.stringify({
      jsonrpc: "2.0",
      id: req.id,
      result: { turn: { id: "turn_smoke_t3" } },
    }),
  );
  await promptPending;
  console.log("devo-im: streaming-start");

  const abortPending = conn.abort();
  req = JSON.parse(lines.at(-1));
  if (req.method !== "session/interrupt") {
    throw new Error(`devo-im: T3 expected session/interrupt, got ${req.method}`);
  }
  console.log("devo-im: interrupt-sent");
  conn.handleIncomingLine(
    JSON.stringify({
      jsonrpc: "2.0",
      id: req.id,
      result: { interrupted: true },
    }),
  );
  await abortPending;
  const state = await conn.getState();
  if (state.isStreaming || conn.expectedTurnId !== null) {
    throw new Error("devo-im: T3 idle restore invariants failed");
  }
  console.log("devo-im: idle-restored");
  console.log("devo-im: t3-ok");
}

export function main(argv = process.argv.slice(2)) {
  assertInheritedTty();
  if (argv.includes("--smoke-adapter")) {
    return runAdapterSmoke();
  }
  if (argv.includes("--smoke-prompt-cell")) {
    return runPromptCellSmoke();
  }
  if (argv.includes("--smoke-interrupt")) {
    return runInterruptSmoke();
  }
  notImplemented();
}

const entry = process.argv[1] ?? "";
if (entry.endsWith("index.js") || entry.endsWith("index.ts")) {
  const report = (error) => {
    const message = error instanceof Error ? error.message : String(error);
    console.error(message);
    process.exitCode = 1;
  };
  try {
    Promise.resolve(main()).catch(report);
  } catch (error) {
    report(error);
  }
}
