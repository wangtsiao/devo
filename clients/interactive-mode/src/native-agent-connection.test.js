import assert from "node:assert/strict";
import test from "node:test";

import {
  DEVO_SLASH_COMMANDS,
  NativeAgentConnection,
  STRIP_PRIME_COMMANDS,
  emptyIpythonDetails,
  ipythonToolDefinition,
} from "./native-agent-connection.js";

test("strip list includes Prime-only commands", () => {
  assert.ok(STRIP_PRIME_COMMANDS.includes("/heartbeat"));
  assert.ok(STRIP_PRIME_COMMANDS.includes("/share"));
  assert.ok(!STRIP_PRIME_COMMANDS.includes("/refine"));
});

test("Devo slash catalog includes /refine and /btw", () => {
  assert.ok(DEVO_SLASH_COMMANDS.includes("/refine"));
  assert.ok(DEVO_SLASH_COMMANDS.includes("/btw"));
});

test("NativeAgentConnection strips Prime commands", () => {
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01");
  assert.equal(conn.isPrimeCommandStripped("/heartbeat"), true);
  assert.equal(conn.isPrimeCommandStripped("/refine"), false);
});

test("ipython tool definition helper", () => {
  const def = ipythonToolDefinition();
  assert.equal(def.name, "ipython");
  assert.deepEqual(def.parameters.required, ["code"]);
  const details = emptyIpythonDetails({ durationMs: 12 });
  assert.equal(details.durationMs, 12);
  assert.deepEqual(details.diffs, []);
  assert.equal(details.backgroundOutput, null);
});

test("subscribe / snapshot / state / messages / listHeartbeats", async () => {
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01");
  const events = [];
  const unsub = conn.subscribe((ev) => events.push(ev));
  const snap = await conn.getInitialSnapshot();
  const state = await conn.getState();
  const messages = await conn.getMessages();
  const heartbeats = await conn.listHeartbeats();
  assert.equal(state.isStreaming, false);
  assert.deepEqual(messages, []);
  assert.deepEqual(heartbeats, []);
  assert.equal(snap.state.isStreaming, false);
  assert.equal(await conn.getToolDefinition("ipython").then((d) => d?.name), "ipython");
  unsub();
});

test("refineRun emits session/refine/run", async () => {
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01");
  const pending = conn.refineRun({ sessionId: "ses_1", instructions: "note" });
  assert.equal(lines.length, 1);
  const req = JSON.parse(lines[0]);
  assert.equal(req.method, "session/refine/run");
  assert.equal(req.params.sessionId, "ses_1");
  conn.handleIncomingLine(
    JSON.stringify({ jsonrpc: "2.0", id: req.id, result: { scheduled: true } }),
  );
  const result = await pending;
  assert.equal(result.scheduled, true);
});

test("prompt / abort / queue helpers emit Native methods", async () => {
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01", {
    sessionId: "ses_1",
  });

  const promptPending = conn.prompt("hello");
  let req = JSON.parse(lines.at(-1));
  assert.equal(req.method, "turn/start");
  assert.equal(req.params.sessionId, "ses_1");
  assert.deepEqual(req.params.input, [{ type: "text", text: "hello" }]);
  conn.handleIncomingLine(
    JSON.stringify({
      jsonrpc: "2.0",
      id: req.id,
      result: { turn: { id: "turn_1" } },
    }),
  );
  await promptPending;
  assert.equal(conn.expectedTurnId, "turn_1");

  const followPending = conn.followUp("next");
  req = JSON.parse(lines.at(-1));
  assert.equal(req.method, "session/queue/push");
  conn.handleIncomingLine(
    JSON.stringify({ jsonrpc: "2.0", id: req.id, result: { outcome: "queued" } }),
  );
  await followPending;

  const abortPending = conn.abort();
  req = JSON.parse(lines.at(-1));
  assert.equal(req.method, "session/interrupt");
  conn.handleIncomingLine(
    JSON.stringify({ jsonrpc: "2.0", id: req.id, result: { interrupted: true } }),
  );
  await abortPending;
});

test("compact emits session/compact/start", async () => {
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01", {
    sessionId: "ses_1",
  });
  const pending = conn.compact();
  const req = JSON.parse(lines[0]);
  assert.equal(req.method, "session/compact/start");
  conn.handleIncomingLine(
    JSON.stringify({ jsonrpc: "2.0", id: req.id, result: { turn: { id: "t" } } }),
  );
  await pending;
});

test("projectRefinementOutcome maps Native refinement item", () => {
  const conn = new NativeAgentConnection(() => {}, "2026-09-01");
  const outcome = conn.projectRefinementOutcome({
    type: "refinement",
    refinementId: "r1",
    trigger: "manual",
    summary: "added memory",
    changes: ["create memory:m1"],
    evidence: "saw repeat",
  });
  assert.equal(outcome.type, "refinement_outcome");
  assert.equal(outcome.refinementId, "r1");
  assert.deepEqual(outcome.edits, [{ description: "create memory:m1" }]);
  assert.equal(conn.projectRefinementOutcome({ type: "message" }), null);
});

test("session new/switch/fork return structured errors or shapes", async () => {
  const lines = [];
  const conn = new NativeAgentConnection((line) => lines.push(line), "2026-09-01");

  const pathSwitch = await conn.switchSession("C:/tmp/session.jsonl");
  assert.equal(pathSwitch.cancelled, true);
  assert.match(pathSwitch.error, /filesystem session paths/i);

  const forkNoSession = await conn.fork("entry_1");
  assert.equal(forkNoSession.cancelled, true);
  assert.match(forkNoSession.error, /no session bound/i);

  const newPending = conn.newSession({ cwd: "/tmp/ws", idempotencyKey: "k1" });
  const req = JSON.parse(lines[0]);
  assert.equal(req.method, "session/new");
  conn.handleIncomingLine(
    JSON.stringify({
      jsonrpc: "2.0",
      id: req.id,
      result: { session: { id: "ses_new" } },
    }),
  );
  const created = await newPending;
  assert.equal(created.cancelled, false);
  assert.equal(conn.sessionId, "ses_new");
});
