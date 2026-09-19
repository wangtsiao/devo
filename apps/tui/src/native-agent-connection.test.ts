/**
 * Trace: L2-DES-APP-010
 * Verifies: NativeAgentConnection prompt/abort/snapshot main path over fake stdio.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import { SessionImportFileNotFoundError } from "@earendil-works/pi-coding-agent";
import {
  NativeAgentConnection,
  approvalScopeForOptionId,
  nativeImageUserInputs,
  reverseRpcToExtensionUiRequest,
} from "./native-agent-connection.js";
import { parseGoalSlash } from "./native-connection-ops.js";
import type { NativeTrafficLog, NativeTrafficRecord } from "./native-traffic-log.js";
import { resolveDevoHome } from "./host.js";

type RpcHandler = (
  method: string,
  params: Record<string, unknown>,
  respond: (result: unknown) => void,
  respondError: (message: string) => void,
) => boolean;

function createFakeServer(
  connRef: { current: NativeAgentConnection | null },
  extraHandlers: RpcHandler[] = [],
) {
  const lines: string[] = [];
  const requests: Array<{ method: string; params: Record<string, unknown> }> = [];
  const writeLine = (line: string) => {
    lines.push(line);
    let msg: Record<string, unknown>;
    try {
      msg = JSON.parse(line) as Record<string, unknown>;
    } catch {
      return;
    }
    const id = msg.id as number | undefined;
    const method = String(msg.method ?? "");
    if (id == null) return;
    const params =
      msg.params && typeof msg.params === "object"
        ? (msg.params as Record<string, unknown>)
        : {};
    requests.push({ method, params });

    const respond = (result: unknown) => {
      connRef.current?.pushChunk(`${JSON.stringify({ jsonrpc: "2.0", id, result })}\n`);
    };
    const respondError = (message: string) => {
      connRef.current?.pushChunk(
        `${JSON.stringify({
          jsonrpc: "2.0",
          id,
          error: { code: -32602, message },
        })}\n`,
      );
    };

    for (const handler of extraHandlers) {
      if (handler(method, params, respond, respondError)) return;
    }

    if (method === "initialize") {
      respond({ protocolVersion: 1, serverInfo: { name: "devo", version: "0" } });
      return;
    }
    if (method === "session/new") {
      respond({
        session: {
          id: "sess_test",
          title: "Test",
          cwd: process.cwd(),
          model: { provider: "openai", id: "gpt-test", input: ["text"] },
          version: 1,
        },
      });
      return;
    }
    if (method === "subscription/create") {
      const selectors = params.selectors;
      if (!Array.isArray(selectors) || selectors.length === 0) {
        respondError("subscription/create requires selectors");
        return;
      }
      for (const sel of selectors) {
        const s = sel as Record<string, unknown>;
        if (s.type != null && s.kind == null) {
          respondError("subscription selectors must use kind, not type");
          return;
        }
        if (s.kind !== "session" || !s.sessionId) {
          respondError("expected { kind: \"session\", sessionId }");
          return;
        }
      }
      respond({ subscriptionId: "sub_1", cursors: [] });
      return;
    }
    if (method === "subscription/delete") {
      respondError("use subscription/unsubscribe, not delete");
      return;
    }
    if (method === "subscription/unsubscribe") {
      respond({ ok: true });
      return;
    }
    if (method === "subscription/ack") {
      respond({ ok: true });
      return;
    }
    if (method === "session/items/list") {
      if ("items" in params) {
        respondError("session/items/list result uses Page.data, not items on params");
        return;
      }
      respond({ data: [], nextCursor: null });
      return;
    }
    if (method === "session/list") {
      respond({
        data: [
          {
            id: "sess_test",
            settings: { permissionProfile: "default" },
          },
        ],
        nextCursor: null,
      });
      return;
    }
    if (method === "session/turns/list") {
      respond({ data: [], nextCursor: null });
      return;
    }
    if (method === "session/fork") {
      respond({
        session: {
          id: "sess_forked",
          title: "Forked",
          cwd: process.cwd(),
          model: { provider: "openai", id: "gpt-test", input: ["text"] },
          version: 1,
        },
      });
      return;
    }
    if (method === "session/resume") {
      respond({
        session: {
          id: String(params.sessionId ?? "sess_resumed"),
          title: "Resumed",
          cwd: process.cwd(),
          model: { provider: "openai", id: "gpt-test", input: ["text"] },
          version: 1,
        },
      });
      return;
    }
    if (method === "session/tree/read") {
      respond({ tree: [], leafId: "item_leaf" });
      return;
    }
    if (method === "session/tree/navigate") {
      respond({
        leafId: params.entryId ?? null,
        cancelled: false,
        editorText: "draft from tree",
      });
      return;
    }
    if (method === "session/queue/list") {
      respond({ entries: [] });
      return;
    }
    if (method === "session/queue/push") {
      respond({
        outcome: "queued",
        entry: {
          queueItemId: "q_steer_1",
          position: 1,
          preview: String(
            (Array.isArray(params.input) &&
              (params.input[0] as { text?: string } | undefined)?.text) ||
              "queued",
          ),
          input: params.input ?? [],
        },
      });
      return;
    }
    if (method === "turn/steer") {
      if (!params.expectedTurnId || !Array.isArray(params.input)) {
        respondError("turn/steer requires expectedTurnId + input");
        return;
      }
      respond({ outcome: "injected", itemId: "item_steer_1" });
      return;
    }
    if (method === "session/queue/remove") {
      respond({});
      return;
    }
    if (method === "session/queue/update") {
      respond({
        entry: {
          queueItemId: params.queueItemId,
          position: params.position ?? 0,
          preview: "updated",
          input: params.input ?? [],
        },
      });
      return;
    }
    if (method === "context/usage/read") {
      respond({
        occupancy: { tokensUsed: 100, contextWindow: 1000 },
      });
      return;
    }
    if (method === "skill/list") {
      respond({
        skills: [
          {
            id: "s1",
            name: "demo",
            description: "Demo skill",
            path: "/skills/demo",
            enabled: true,
            source: "user",
            scope: "user",
          },
        ],
      });
      return;
    }
    if (method === "tool/list") {
      respond({ tools: [{ name: "ipython", source: "builtin" }] });
      return;
    }
    if (method === "mcp/list") {
      respond({ servers: [{ name: "time", enabled: true }] });
      return;
    }
    if (method === "workspace/changes/read") {
      respond({ views: [{ files: [{ path: "README.md" }] }] });
      return;
    }
    if (method === "provider/list") {
      respond({
        providers: [],
        connectedProviderIds: [],
        connectionModels: {},
      });
      return;
    }
    if (method === "credential/set") {
      respond({
        credential: {
          id: "cred_1",
          provider: params.provider,
          masked: "***",
          kind: params.kind,
        },
      });
      return;
    }
    if (method === "credential/list") {
      respond({ credentials: [] });
      return;
    }
    if (method === "credential/delete") {
      respond({});
      return;
    }
    if (method === "session/goal/read") {
      respond({ goal: null });
      return;
    }
    if (method === "session/goal/set") {
      if (typeof params.objective !== "string" || !params.ifExists || !params.idempotencyKey) {
        respondError("session/goal/set requires objective, ifExists, idempotencyKey");
        return;
      }
      respond({
        goal: {
          id: "goal_1",
          objective: params.objective,
          status: "active",
        },
      });
      return;
    }
    if (
      method === "session/goal/pause" ||
      method === "session/goal/resume" ||
      method === "session/goal/cancel" ||
      method === "session/goal/complete" ||
      method === "session/goal/clear"
    ) {
      if (!params.expectedGoalId) {
        respondError(`${method} requires expectedGoalId`);
        return;
      }
      respond(
        method === "session/goal/clear"
          ? {}
          : {
              goal: {
                id: params.expectedGoalId,
                status:
                  method === "session/goal/pause"
                    ? "paused"
                    : method === "session/goal/complete"
                      ? "completed"
                      : method === "session/goal/cancel"
                        ? "failed"
                        : "active",
              },
            },
      );
      return;
    }
    if (method === "turn/start") {
      respond({ turn: { id: "turn_1" } });
      queueMicrotask(() => {
        const c = connRef.current;
        if (!c) return;
        c.pushChunk(
          `${JSON.stringify({ jsonrpc: "2.0", method: "turn/started", params: { turn: { id: "turn_1" } } })}\n`,
        );
        c.pushChunk(
          `${JSON.stringify({
            jsonrpc: "2.0",
            method: "item/assistantMessage/delta",
            params: { itemId: "a1", delta: "Hi" },
          })}\n`,
        );
        c.pushChunk(
          `${JSON.stringify({
            jsonrpc: "2.0",
            method: "item/completed",
            params: {
              id: "a1",
              item: { type: "assistantMessage", text: "Hi" },
            },
          })}\n`,
        );
        c.pushChunk(
          `${JSON.stringify({ jsonrpc: "2.0", method: "turn/completed", params: { turn: { id: "turn_1" } } })}\n`,
        );
      });
      return;
    }
    if (method === "session/interrupt") {
      const scope = params.scope as Record<string, unknown> | undefined;
      if (!scope || scope.scope !== "session" || !scope.sessionId) {
        respondError('session/interrupt requires { scope: { scope: "session", sessionId } }');
        return;
      }
      respond({ interrupted: true });
      return;
    }
    if (method === "session/metadata/update") {
      if (params.patch != null) {
        respondError("session/metadata/update is flat; do not nest patch");
        return;
      }
      if (!params.sessionId || params.expectedVersion == null) {
        respondError("session/metadata/update requires sessionId and expectedVersion");
        return;
      }
      respond({
        session: {
          id: params.sessionId,
          version: Number(params.expectedVersion) + 1,
          title: params.title,
          model: params.model,
          settings: params.settings,
        },
      });
      return;
    }
    if (method === "model/list") {
      respond({
        models: [
          { provider: "openai", id: "gpt-test", input: ["text", "image"] },
          { provider: "openai", id: "gpt-other", input: ["text"] },
        ],
      });
      return;
    }
    if (method === "task/start") {
      if (params.kind !== "process" || !params.command || !params.sessionId) {
        respondError("task/start requires kind=process, sessionId, command");
        return;
      }
      respond({ itemId: "bash_1" });
      queueMicrotask(() => {
        const c = connRef.current;
        if (!c) return;
        c.pushChunk(
          `${JSON.stringify({
            jsonrpc: "2.0",
            method: "item/started",
            params: {
              item: { type: "process", id: "bash_1", command: params.command },
            },
          })}\n`,
        );
        c.pushChunk(
          `${JSON.stringify({
            jsonrpc: "2.0",
            method: "item/completed",
            params: {
              id: "bash_1",
              item: { type: "process", id: "bash_1", status: "completed", exitCode: 0 },
            },
          })}\n`,
        );
      });
      return;
    }
    if (method === "task/read") {
      respond({ outputTail: "ok\n", item: { status: "completed", exitCode: 0 } });
      return;
    }
    if (method === "session/schedule/list") {
      respond({ jobs: [] });
      return;
    }
    if (method === "session/heartbeat/command") {
      const args = String(params.args ?? "").trim();
      if (!params.sessionId) {
        respondError("session/heartbeat/command requires sessionId");
        return;
      }
      if (!args || args === "status") {
        respond({ action: "status", job: null });
        return;
      }
      if (args === "pause" || args === "resume" || args === "clear") {
        respond({
          action: args,
          job: {
            jobId: "job_1",
            kind: "heartbeat",
            status: args === "clear" ? "stopped" : args === "pause" ? "paused" : "active",
            sessionId: params.sessionId,
            schedule: "every 1h",
            instruction: "ping",
            deliveryMode: "steer",
            createdAt: new Date().toISOString(),
            updatedAt: new Date().toISOString(),
            runCount: 0,
          },
        });
        return;
      }
      // set: optional --follow-up, then schedule + instruction
      const tokens = args.split(/\s+/);
      let deliveryMode = "steer";
      let i = 0;
      if (tokens[0] === "--follow-up") {
        deliveryMode = "followUp";
        i = 1;
      }
      const schedule = tokens[i] ?? "every 1h";
      const instruction = tokens.slice(i + 1).join(" ") || "ping";
      respond({
        action: "set",
        job: {
          jobId: "job_1",
          kind: "heartbeat",
          status: "active",
          sessionId: params.sessionId,
          schedule,
          instruction,
          deliveryMode,
          createdAt: new Date().toISOString(),
          updatedAt: new Date().toISOString(),
          runCount: 0,
        },
      });
      return;
    }
    if (method === "session/schedule/upsert") {
      if (!params.kind || !params.sessionId) {
        respondError("session/schedule/upsert requires kind and sessionId");
        return;
      }
      respond({
        job: {
          jobId: "job_1",
          kind: params.kind,
          schedule: params.schedule,
          instruction: params.instruction ?? params.prompt,
          prompt: params.prompt ?? params.instruction,
          deliveryMode: params.deliveryMode ?? "steer",
          sessionId: params.sessionId,
          status: "active",
          createdAt: new Date().toISOString(),
          updatedAt: new Date().toISOString(),
          runCount: 0,
        },
      });
      return;
    }
    if (method === "session/schedule/update" || method === "session/schedule/delete") {
      respond({
        job: {
          jobId: params.jobId,
          kind: "heartbeat",
          status: method === "session/schedule/delete" ? "stopped" : "paused",
          sessionId: "sess_test",
          schedule: "every 1h",
          instruction: "ping",
          deliveryMode: "steer",
          createdAt: new Date().toISOString(),
          updatedAt: new Date().toISOString(),
          runCount: 0,
        },
      });
      return;
    }
    if (method === "session/export") {
      if (!params.sessionId || (params.format !== "jsonl" && params.format !== "html")) {
        respondError("session/export requires sessionId and format jsonl|html");
        return;
      }
      respond({ path: String(params.path ?? `export.${params.format}`) });
      return;
    }
    if (method === "session/import") {
      respond({ sessionId: "ses_imported" });
      return;
    }
    respond({});
  };

  return { lines, writeLine, requests };
}

async function bootConnection(
  extraHandlers: RpcHandler[] = [],
  trafficLog?: NativeTrafficLog,
) {
  const connRef: { current: NativeAgentConnection | null } = { current: null };
  const fake = createFakeServer(connRef, extraHandlers);
  const conn = NativeAgentConnection.create({
    writeLine: fake.writeLine,
    cwd: process.cwd(),
    trafficLog,
  });
  connRef.current = conn;
  await conn.initialize({ name: "test", version: "0" });
  await conn.newSession();
  return { conn, fake };
}

test("connect initializes, creates session, and returns InteractiveMode snapshot", async () => {
  const { conn, fake } = await bootConnection();

  const snap = await conn.getInitialSnapshot();
  assert.equal(snap.state.sessionId, "sess_test");
  assert.equal(snap.state.isStreaming, false);
  assert.ok(Array.isArray(snap.messages));

  const models = await conn.getAvailableModels();
  assert.equal(models[0]?.id, "gpt-test");
  assert.deepEqual(models[0]?.input, ["text", "image"]);

  const create = fake.requests.find((r) => r.method === "subscription/create");
  assert.ok(create);
  assert.equal((create!.params.selectors as Array<{ kind: string }>)[0]?.kind, "session");

  const usage = fake.requests.find((r) => r.method === "context/usage/read");
  assert.ok(usage, "subscription/resume path should call context/usage/read");

  await conn.dispose();
  const unsub = fake.requests.filter((r) => r.method === "subscription/unsubscribe");
  assert.ok(unsub.length >= 1);
  assert.equal(
    fake.requests.some((r) => r.method === "subscription/delete"),
    false,
  );
});

test("getModelCatalog marks connected and credentialed providers as configured", async () => {
  const { conn } = await bootConnection([
    (method, _params, respond) => {
      if (method === "provider/list") {
        respond({
          providers: [
            {
              id: "deepseek",
              name: "DeepSeek",
              models: {
                "deepseek-v4-flash": {
                  id: "deepseek-v4-flash",
                  name: "DeepSeek V4 Flash",
                  provider: "deepseek",
                  input: ["text"],
                },
              },
            },
            {
              id: "openai",
              name: "OpenAI",
              models: {
                "gpt-test": {
                  id: "gpt-test",
                  name: "GPT Test",
                  provider: "openai",
                  input: ["text"],
                },
              },
            },
          ],
          connectedProviderIds: ["deepseek"],
          connectionModels: {
            deepseek: {
              "deepseek-v4-flash": {
                id: "deepseek-v4-flash",
                name: "DeepSeek V4 Flash",
              },
            },
          },
        });
        return true;
      }
      if (method === "credential/list") {
        respond({
          credentials: [
            {
              id: "deepseek_api_key",
              provider: "deepseek",
              masked: "sk-…",
              kind: "api_key",
            },
          ],
        });
        return true;
      }
      return false;
    },
  ]);

  const catalog = await conn.getModelCatalog();
  assert.ok(catalog.configuredProviders.includes("deepseek"));
  assert.equal(catalog.configuredProviders.includes("openai"), false);
  assert.ok(catalog.models.some((model) => model.provider === "deepseek"));

  await conn.dispose();
});

test("nativeImageUserInputs maps ImageContent data to Native data-URI", () => {
  assert.deepEqual(
    nativeImageUserInputs([
      { type: "image", data: "abc123", mimeType: "image/png" },
      { type: "image", uri: "data:image/jpeg;base64,zzz", mimeType: "image/jpeg" },
      { path: "C:\\tmp\\x.png" },
      { type: "image" },
    ]),
    [
      {
        type: "image",
        uri: "data:image/png;base64,abc123",
        mimeType: "image/png",
      },
      {
        type: "image",
        uri: "data:image/jpeg;base64,zzz",
        mimeType: "image/jpeg",
      },
      {
        type: "localImage",
        path: "C:\\tmp\\x.png",
      },
    ],
  );
});

test("prompt with local image path sends Native localImage (no client encode)", async () => {
  const { conn, fake } = await bootConnection();
  await conn.prompt("describe this", {
    images: [{ path: "C:\\Users\\lenovo\\Desktop\\test_image.png" }],
  });
  const start = fake.requests.find((r) => r.method === "turn/start");
  assert.ok(start, "expected turn/start");
  assert.deepEqual(start.params.input, [
    { type: "text", text: "describe this" },
    {
      type: "localImage",
      path: "C:\\Users\\lenovo\\Desktop\\test_image.png",
    },
  ]);
  await conn.dispose();
});

test("prompt with pasted ImageContent sends Native image uri on turn/start", async () => {
  const { conn, fake } = await bootConnection();
  await conn.prompt("describe this", {
    images: [{ type: "image", data: "qqq", mimeType: "image/png" }],
  });
  const start = fake.requests.find((r) => r.method === "turn/start");
  assert.ok(start, "expected turn/start");
  assert.deepEqual(start.params.input, [
    { type: "text", text: "describe this" },
    {
      type: "image",
      uri: "data:image/png;base64,qqq",
      mimeType: "image/png",
    },
  ]);
  await conn.dispose();
});

test("prompt emits InteractiveMode session_events then idle", async () => {
  const { conn, fake } = await bootConnection();

  const events: string[] = [];
  conn.subscribe((ev) => {
    if (ev.type === "session_event") {
      events.push(String((ev.event as { type?: string }).type));
    }
  });

  await conn.prompt("hello");
  await conn.waitForIdle();

  assert.ok(events.includes("agent_start"));
  assert.ok(events.includes("message_start") || events.includes("message_update"));
  assert.ok(events.includes("agent_end"));

  await conn.abort();
  const interrupt = fake.requests.find((r) => r.method === "session/interrupt");
  assert.deepEqual(interrupt?.params, {
    scope: { scope: "session", sessionId: "sess_test" },
  });

  await conn.dispose();
});

test("abort emits agent_end so InteractiveMode clears isStreaming", async () => {
  const { conn, fake } = await bootConnection();
  const events: string[] = [];
  conn.subscribe((ev) => {
    if (ev.type === "session_event") {
      events.push(String((ev.event as { type?: string }).type));
    }
  });

  await conn.prompt("hello");
  // Simulate mid-turn without waiting for idle completion.
  events.length = 0;
  await conn.abort();
  const interrupt = fake.requests.find((r) => r.method === "session/interrupt");
  assert.deepEqual(interrupt?.params, {
    scope: { scope: "session", sessionId: "sess_test" },
  });
  assert.ok(events.includes("agent_end"), `events=${events.join(",")}`);
  assert.ok(events.includes("message_end"), `events=${events.join(",")}`);
  assert.equal((await conn.getState()).isStreaming, false);

  await conn.dispose();
});

test("parseGoalSlash covers slash forms", () => {
  assert.deepEqual(parseGoalSlash("/goal"), { kind: "read" });
  assert.deepEqual(parseGoalSlash("/goal pause"), {
    kind: "transition",
    method: "session/goal/pause",
  });
  assert.deepEqual(parseGoalSlash("/goal Ship it"), {
    kind: "set",
    objective: "Ship it",
  });
  assert.equal(parseGoalSlash("not a goal"), null);
});

test("/goal intercept uses session/goal RPCs and emits goal_update", async () => {
  let goalId: string | null = null;
  const { conn, fake } = await bootConnection([
    (method, params, respond) => {
      if (method === "session/goal/read") {
        respond({
          goal: goalId
            ? { id: goalId, objective: "Ship it", status: "active" }
            : null,
        });
        return true;
      }
      if (method === "session/goal/set") {
        goalId = "goal_1";
        respond({
          goal: { id: goalId, objective: params.objective, status: "active" },
        });
        return true;
      }
      if (method === "session/goal/pause") {
        respond({ goal: { id: params.expectedGoalId, status: "paused" } });
        return true;
      }
      return false;
    },
  ]);

  const goals: unknown[] = [];
  conn.subscribe((ev) => {
    if (ev.type === "session_event" && (ev.event as { type?: string }).type === "goal_update") {
      goals.push((ev.event as { goal?: unknown }).goal);
    }
  });

  await conn.prompt("/goal");
  assert.ok(goals.length >= 1);

  await conn.prompt("/goal Ship it");
  assert.ok(
    fake.requests.some(
      (r) => r.method === "session/goal/set" && r.params.objective === "Ship it",
    ),
  );
  assert.equal(
    fake.requests.some((r) => r.method === "turn/start"),
    false,
    "/goal must not call turn/start",
  );

  await conn.prompt("/goal pause");
  assert.ok(
    fake.requests.some(
      (r) =>
        r.method === "session/goal/pause" && r.params.expectedGoalId === "goal_1",
    ),
  );

  await conn.dispose();
});

test("idle prompt with streamingBehavior steer uses turn/start (resumeIfIdle)", async () => {
  const { conn, fake } = await bootConnection();
  await conn.prompt("hello", { streamingBehavior: "steer" });
  assert.ok(
    fake.requests.some((r) => r.method === "turn/start"),
    "idle steer must turn/start",
  );
  assert.equal(
    fake.requests.some((r) => r.method === "turn/steer"),
    false,
    "idle must not call turn/steer",
  );
  await conn.dispose();
});

test("busy steer uses turn/steer with raw input", async () => {
  const { conn, fake } = await bootConnection();
  conn.pushChunk(
    `${JSON.stringify({
      jsonrpc: "2.0",
      method: "turn/started",
      params: { turn: { id: "turn_busy" } },
    })}\n`,
  );
  await new Promise((r) => setTimeout(r, 10));
  await conn.prompt("nudge", { streamingBehavior: "steer" });
  const methods = fake.requests.map((r) => r.method);
  assert.ok(methods.includes("turn/steer"), "busy steer must call turn/steer");
  assert.equal(
    methods.includes("session/queue/steer"),
    false,
    "session/queue/steer is removed",
  );
  const steer = fake.requests.find((r) => r.method === "turn/steer");
  assert.equal(steer?.params.expectedTurnId, "turn_busy");
  assert.ok(Array.isArray(steer?.params.input));
  const queue = await conn.getQueue();
  assert.deepEqual(queue.steering, []);
  await conn.dispose();
});

test("turn/completed refreshes follow-up queue from session/queue/list", async () => {
  const { conn, fake } = await bootConnection();
  conn.pushChunk(
    `${JSON.stringify({
      jsonrpc: "2.0",
      method: "turn/started",
      params: { turn: { id: "turn_q" } },
    })}\n`,
  );
  await new Promise((r) => setTimeout(r, 10));
  await conn.followUp("stay queued");
  assert.deepEqual((await conn.getQueue()).followUp, ["stay queued"]);

  // Server queue is empty after drain — turn/completed must clear the banner.
  conn.pushChunk(
    `${JSON.stringify({
      jsonrpc: "2.0",
      method: "turn/completed",
      params: { turn: { id: "turn_q", status: "completed" } },
    })}\n`,
  );
  await new Promise((r) => setTimeout(r, 30));
  assert.ok(
    fake.requests.some((r) => r.method === "session/queue/list"),
    "must re-list queue after turn/completed",
  );
  assert.deepEqual((await conn.getQueue()).followUp, []);
  await conn.dispose();
});

test("executeBash uses task/start process kind", async () => {
  const { conn, fake } = await bootConnection();
  await conn.executeBash("echo hi");
  const start = fake.requests.find((r) => r.method === "task/start");
  assert.ok(start);
  assert.equal(start!.params.kind, "process");
  assert.equal(start!.params.command, "echo hi");
  assert.equal(start!.params.sessionId, "sess_test");
  await conn.dispose();
});

test("getResourceSnapshot maps skill/list + mcp/list", async () => {
  const { conn } = await bootConnection();
  const snap = await conn.getResourceSnapshot();
  assert.equal(snap.skills[0]?.name, "demo");
  assert.equal(snap.extensions[0]?.path, "time");
  assert.ok(snap.contextFiles.some((f) => f.path === "README.md"));
  await conn.dispose();
});

test("clearQueue lists then removes entries", async () => {
  const { conn, fake } = await bootConnection([
    (method, _params, respond) => {
      if (method === "session/queue/list") {
        respond({
          entries: [
            {
              queueItemId: "q1",
              position: 0,
              preview: "one",
              input: [{ type: "text", text: "one" }],
            },
          ],
        });
        return true;
      }
      return false;
    },
  ]);
  await conn.clearQueue();
  assert.ok(fake.requests.some((r) => r.method === "session/queue/list"));
  assert.ok(
    fake.requests.some(
      (r) => r.method === "session/queue/remove" && r.params.queueItemId === "q1",
    ),
  );
  await conn.dispose();
});

test("cycleModel advances through catalog and metadata/update", async () => {
  const { conn, fake } = await bootConnection();
  const result = await conn.cycleModel("forward");
  assert.ok(result?.model);
  const meta = fake.requests.find((r) => r.method === "session/metadata/update");
  assert.ok(meta);
  assert.equal(meta!.params.patch, undefined);
  assert.ok(meta!.params.model);
  await conn.dispose();
});

test("getSessionStats uses context/usage/read", async () => {
  const { conn } = await bootConnection();
  const stats = await conn.getSessionStats();
  assert.equal(stats.sessionId, "sess_test");
  assert.equal(stats.tokens.input, 100);
  assert.ok(stats.contextUsage);
  assert.equal(
    stats.sessionFile,
    path.join(resolveDevoHome(), "sessions", "sess_test.jsonl"),
  );
  assert.equal((await conn.getState()).sessionFile, stats.sessionFile);
  await conn.dispose();
});

test("getContextTree returns InteractiveMode-shaped ownUsage so /context can format", async () => {
  const { conn } = await bootConnection();
  await conn.getSessionStats(); // warm occupancy → contextUsage
  const tree = await conn.getContextTree();
  assert.equal(tree.id, "root");
  assert.equal(tree.status, "active");
  assert.equal(typeof tree.ownUsage.input, "number");
  assert.equal(typeof tree.ownUsage.cost.total, "number");
  assert.ok(Array.isArray(tree.children));
  assert.equal(tree.ownUsage.input, 100);
  await conn.dispose();
});

test("schedule heartbeat and export use Native session/heartbeat/command + session/export", async () => {
  const { conn, fake } = await bootConnection();
  const hb = await conn.setHeartbeat("every 1h", "ping status", "steer");
  assert.equal(hb.id, "job_1");
  assert.ok(
    fake.requests.some(
      (r) =>
        r.method === "session/heartbeat/command" &&
        r.params.sessionId === "sess_test" &&
        String(r.params.args).includes("every 1h") &&
        String(r.params.args).includes("ping status"),
    ),
  );

  const exported = await conn.exportToJsonl("out.jsonl");
  assert.equal(exported, path.resolve("out.jsonl"));
  const exportReq = fake.requests.find((r) => r.method === "session/export");
  assert.ok(exportReq);
  assert.equal(exportReq?.params.format, "jsonl");
  assert.equal(exportReq?.params.sessionId, "sess_test");
  assert.equal(exportReq?.params.path, path.resolve("out.jsonl"));

  const tempHtml = path.join(os.tmpdir(), "session.html");
  const shared = await conn.exportToHtml(tempHtml);
  assert.equal(shared, "export.html");
  const shareReq = fake.requests.filter((r) => r.method === "session/export").at(-1);
  assert.ok(shareReq);
  assert.equal(shareReq?.params.format, "html");
  assert.equal(shareReq?.params.path, undefined);
  await conn.dispose();
});

test("importFromJsonl resolves relative path and switches to imported session", async () => {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "devo-import-"));
  const relName = "import-fixture.jsonl";
  const absPath = path.join(tmp, relName);
  fs.writeFileSync(absPath, '{"type":"sessionMeta","id":"old"}\n{"type":"userMessage"}\n');
  const prevCwd = process.cwd();
  try {
    process.chdir(tmp);
    const { conn, fake } = await bootConnection();
    const result = await conn.importFromJsonl(relName);
    assert.equal(result.cancelled, false);
    const importReq = fake.requests.find((r) => r.method === "session/import");
    assert.ok(importReq);
    assert.equal(importReq?.params.path, absPath);
    assert.equal(importReq?.params.format, "jsonl");
    assert.equal((await conn.getState()).sessionId, "ses_imported");
    await conn.dispose();
  } finally {
    process.chdir(prevCwd);
    fs.rmSync(tmp, { recursive: true, force: true });
  }
});

test("importFromJsonl throws SessionImportFileNotFoundError for missing file", async () => {
  const { conn } = await bootConnection();
  await assert.rejects(
    () => conn.importFromJsonl(path.join(os.tmpdir(), "missing-devo-import.jsonl")),
    (error: unknown) => error instanceof SessionImportFileNotFoundError,
  );
  await conn.dispose();
});

test("compact on short session emits warning without calling session/compact/start", async () => {
  const { conn, fake } = await bootConnection();
  const events: Array<Record<string, unknown>> = [];
  conn.subscribe((ev) => {
    if (ev.type === "session_event") {
      events.push(ev.event as Record<string, unknown>);
    }
  });

  await assert.rejects(
    () => conn.compact(),
    /too short to compact/i,
  );

  assert.equal(
    fake.requests.some((r) => r.method === "session/compact/start"),
    false,
  );
  const start = events.find((e) => e.type === "compaction_start");
  const end = events.find((e) => e.type === "compaction_end");
  assert.ok(start);
  assert.equal(start?.reason, "manual");
  assert.equal(end?.errorSeverity, "warning");
  assert.match(String(end?.errorMessage ?? ""), /too short to compact/i);

  await conn.dispose();
});

test("fork at tree leaf omits atTurnId (tip fork) and switches session", async () => {
  const { conn, fake } = await bootConnection();
  const result = await conn.fork("item_leaf", { position: "at" });
  assert.equal(result.cancelled, false);
  const forkReq = fake.requests.find((r) => r.method === "session/fork");
  assert.ok(forkReq);
  assert.equal("atTurnId" in forkReq!.params, false);
  assert.equal(forkReq!.params.cut, "through");
  assert.equal(conn.buildState().sessionId, "sess_forked");
  await conn.dispose();
});

test("fork with turn_ id passes atTurnId through", async () => {
  const { conn, fake } = await bootConnection();
  await conn.fork("turn_abc", { position: "at" });
  const forkReq = fake.requests.find((r) => r.method === "session/fork");
  assert.equal(forkReq?.params.atTurnId, "turn_abc");
  await conn.dispose();
});

/** Captures recorded traffic and serves canned `/traces` state. */
function createCaptureTrafficLog() {
  const records: NativeTrafficRecord[] = [];
  const log: NativeTrafficLog = {
    getState: () => ({ enabled: true, path: "/tmp/traces/protocol-1.ndjsonl" }),
    record: (entry) => {
      records.push(entry);
    },
    enable: () => ({ enabled: true, path: "/tmp/traces/protocol-1.ndjsonl" }),
    disable: () => {},
    list: () => [{ path: "/tmp/traces/protocol-1.ndjsonl", bytes: 128, modifiedAtMs: 1 }],
    preview: () => [
      { direction: "tui-to-server", kind: "request", id: 1, method: "initialize", payload: {} },
    ],
  };
  return { log, records };
}

function collectNotices(conn: NativeAgentConnection): string[] {
  const notices: string[] = [];
  conn.subscribe((ev) => {
    if (ev.type !== "extension_ui_request") return;
    const message = (ev.request as { payload?: { message?: string } }).payload?.message;
    if (message) notices.push(message);
  });
  return notices;
}

test("/traces records protocol traffic and reports status without turn/start", async () => {
  const capture = createCaptureTrafficLog();
  const { conn, fake } = await bootConnection([], capture.log);
  const notices = collectNotices(conn);

  await conn.prompt("/traces status");
  assert.ok(notices.some((message) => message.includes("Traces: on")));
  assert.ok(notices.some((message) => message.includes("Trace files: 1")));
  assert.equal(
    fake.requests.some((request) => request.method === "turn/start"),
    false,
    "/traces must not call turn/start",
  );

  notices.length = 0;
  await conn.prompt("/traces preview 5");
  assert.ok(notices.some((message) => message.includes("Traces preview (1 records)")));

  notices.length = 0;
  await conn.prompt("/traces off");
  assert.ok(notices.some((message) => message.includes("Traces disabled")));

  await assert.rejects(() => conn.prompt("/traces bounce"), /Usage: \/traces/);

  assert.ok(
    capture.records.some((record) => record.direction === "tui-to-server"),
    "outbound frames must be traced",
  );
  assert.ok(
    capture.records.some((record) => record.direction === "server-to-tui"),
    "inbound frames must be traced",
  );

  await conn.dispose();
});

test("/traces reports unavailable when the host attaches no traffic log", async () => {
  const { conn } = await bootConnection();
  await assert.rejects(() => conn.prompt("/traces"), /tracing is unavailable/);
  await conn.dispose();
});

test("subscription replay of turn/started without snapshot activeTurn leaves idle", async () => {
  const { conn } = await bootConnection([
    (method, _params, respond) => {
      if (method !== "subscription/create") return false;
      respond({
        subscriptionId: "sub_orphan",
        cursors: [],
        snapshots: [
          {
            data: {
              session: {
                id: "sess_test",
                title: "Test",
                cwd: process.cwd(),
                status: "idle",
                activeTurnId: null,
                model: { provider: "openai", id: "gpt-test", input: ["text"] },
                version: 1,
              },
              activeTurn: null,
              queue: [],
            },
          },
        ],
        replay: [
          {
            notification: {
              method: "turn/started",
              params: {
                turn: {
                  id: "turn_orphan",
                  sessionId: "sess_test",
                  status: "inProgress",
                },
              },
            },
          },
        ],
      });
      return true;
    },
  ]);

  assert.equal((await conn.getState()).isStreaming, false);
  await conn.dispose();
});

test("switchSession with recovery keeps isStreaming false after subscribe", async () => {
  let resumeCalls = 0;
  const { conn } = await bootConnection([
    (method, params, respond) => {
      if (method === "session/resume") {
        resumeCalls += 1;
        respond({
          session: {
            id: String(params.sessionId ?? "sess_resumed"),
            title: "Resumed",
            cwd: process.cwd(),
            status: "idle",
            activeTurnId: null,
            model: { provider: "openai", id: "gpt-test", input: ["text"] },
            version: 1,
          },
          recovery: {
            turnId: "turn_orphan",
            revision: 1,
            attempt: 0,
            reason: "Execution ended unexpectedly.",
          },
        });
        return true;
      }
      if (method === "subscription/create") {
        const sessionId = String(
          (Array.isArray(params.selectors) &&
            (params.selectors[0] as { sessionId?: string } | undefined)?.sessionId) ||
            "sess_resumed",
        );
        respond({
          subscriptionId: "sub_resume",
          cursors: [],
          snapshots: [
            {
              data: {
                session: {
                  id: sessionId,
                  title: "Resumed",
                  cwd: process.cwd(),
                  status: "idle",
                  activeTurnId: null,
                  model: { provider: "openai", id: "gpt-test", input: ["text"] },
                  version: 1,
                },
                activeTurn: null,
                queue: [],
              },
            },
          ],
          replay: [
            {
              notification: {
                method: "turn/started",
                params: {
                  turn: { id: "turn_orphan", sessionId, status: "inProgress" },
                },
              },
            },
          ],
        });
        return true;
      }
      return false;
    },
  ]);

  const events: Array<Record<string, unknown>> = [];
  conn.subscribe((event) => {
    if (event.type === "session_event") {
      events.push(event.event as Record<string, unknown>);
    }
  });

  const result = await conn.switchSession("sess_resumed");
  assert.equal(result.cancelled, false);
  assert.ok(resumeCalls >= 1);
  assert.equal((await conn.getState()).isStreaming, false);
  assert.ok(
    events.some(
      (event) =>
        event.type === "session_lifecycle" &&
        event.action === "turn_recovery" &&
        (event.recovery as { turnId?: string } | undefined)?.turnId === "turn_orphan",
    ),
    "resume recovery should emit turn_recovery lifecycle",
  );
  await conn.dispose();
});

test("approval reverse-rpc with options renders a scope selector", () => {
  const request = reverseRpcToExtensionUiRequest("r1", "approval/fileChange/request", {
    summary: "Write file C:/Temp/x.txt",
    options: [
      { option_id: "allow_once", name: "Yes, proceed" },
      { option_id: "allow_path_prefix", name: "Yes, and don't ask again for files under `C:/Temp`" },
      { option_id: "reject_once", name: "No, continue without running it" },
    ],
  });
  assert.equal(request.method, "select");
  const payload = request.payload as { title: string; options: string[] };
  assert.match(payload.title, /Approve fileChange/);
  assert.deepEqual(payload.options, [
    "Yes, proceed",
    "Yes, and don't ask again for files under `C:/Temp`",
    "No, continue without running it",
  ]);
});

test("approval reverse-rpc without options falls back to binary confirm", () => {
  const request = reverseRpcToExtensionUiRequest("r2", "approval/command/request", {
    command: "npm install",
  });
  assert.equal(request.method, "confirm");
});

test("approval scope option ids map to native wire scopes", () => {
  assert.equal(approvalScopeForOptionId("allow_once"), "once");
  assert.equal(approvalScopeForOptionId("allow_session"), "session");
  assert.equal(approvalScopeForOptionId("allow_prefix_rule"), "commandPrefixPersist");
  assert.equal(approvalScopeForOptionId("allow_path_prefix"), "pathPrefix");
  assert.equal(approvalScopeForOptionId("allow_host"), "host");
  assert.equal(approvalScopeForOptionId("reject_once"), "once");
  assert.equal(approvalScopeForOptionId("unknown"), "once");
});
