/**
 * Resume projection must restore tools + file ops, not only text.
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  applyFailedTurnsToAgentMessages,
  occupancyToContextUsage,
  projectNativeItemsToAgentMessages,
} from "./project-native-items.js";

test("occupancyToContextUsage maps Native totalTokens/contextWindowTokens", () => {
  const usage = occupancyToContextUsage({
    totalTokens: 12000,
    contextWindowTokens: 262144,
    categories: [],
  });
  assert.deepEqual(usage, {
    tokens: 12000,
    contextWindow: 262144,
    percent: 4.6,
  });
});

test("restore projects toolCall + toolResult pairs", () => {
  const messages = projectNativeItemsToAgentMessages([
    {
      id: "1",
      createdAt: "2026-01-01T00:00:00Z",
      item: { type: "userMessage", content: [{ type: "text", text: "run" }] },
    },
    {
      id: "2",
      createdAt: "2026-01-01T00:00:01Z",
      item: {
        type: "toolCall",
        callId: "c1",
        toolName: "ipython",
        arguments: { code: "print(1)" },
      },
    },
    {
      id: "3",
      createdAt: "2026-01-01T00:00:02Z",
      item: {
        type: "toolResult",
        callId: "c1",
        toolName: "ipython",
        isError: false,
        output: {
          content: [{ type: "text", text: "1" }],
          details: { status: "ok", stdout: "1\n", durationMs: 1 },
        },
      },
    },
  ]);
  assert.equal(messages[0]?.role, "user");
  assert.equal(messages[1]?.role, "assistant");
  const call = (messages[1]?.content as Array<{ type?: string; name?: string; id?: string }>)?.[0];
  assert.equal(call?.type, "toolCall");
  assert.equal(call?.name, "ipython");
  assert.equal(call?.id, "c1");
  assert.equal(messages[2]?.role, "toolResult");
  assert.equal(messages[2]?.toolCallId, "c1");
  assert.equal((messages[2] as { details?: { status?: string } }).details?.status, "ok");
});

test("restore FileChange emits edit toolCall + toolResult with details.diff", () => {
  const messages = projectNativeItemsToAgentMessages([
    {
      id: "1",
      item: {
        type: "fileChange",
        callId: "c-edit",
        changes: [
          {
            path: "tmp.txt",
            change: { type: "update", unifiedDiff: "--- a\n+++ b\n@@\n-old\n+new\n" },
          },
        ],
      },
    },
  ]);
  assert.equal(messages.length, 2);
  assert.equal(messages[0]?.role, "assistant");
  assert.equal(
    (messages[0]?.content as Array<{ name?: string }>)[0]?.name,
    "edit",
  );
  assert.equal(messages[1]?.role, "toolResult");
  assert.match(String((messages[1] as { details?: { diff?: string } }).details?.diff), /\+new/);
});

test("restore commandExecution emits bash call + result", () => {
  const messages = projectNativeItemsToAgentMessages([
    {
      id: "1",
      item: {
        type: "commandExecution",
        callId: "c-bash",
        command: "echo hi",
        output: "hi\n",
        exitCode: 0,
      },
    },
  ]);
  assert.equal(messages[0]?.role, "assistant");
  assert.equal((messages[0]?.content as Array<{ name?: string }>)[0]?.name, "bash");
  assert.equal(messages[1]?.role, "toolResult");
  assert.equal(messages[1]?.isError, false);
});

test("restore synthesizes aborted toolResult for unpaired toolCall", () => {
  const messages = projectNativeItemsToAgentMessages([
    {
      id: "2",
      state: "interrupted",
      createdAt: "2026-01-01T00:00:01Z",
      item: {
        type: "toolCall",
        callId: "c-orphan",
        toolName: "ipython",
        arguments: { code: "import time; time.sleep(99)" },
      },
    },
  ]);
  assert.equal(messages.length, 2);
  assert.equal(messages[0]?.role, "assistant");
  assert.equal(messages[1]?.role, "toolResult");
  assert.equal(messages[1]?.toolCallId, "c-orphan");
  assert.equal(messages[1]?.isError, true);
  assert.equal((messages[1] as { details?: { status?: string } }).details?.status, "aborted");
});

test("restore projects Warning as assistant error (failed turn)", () => {
  const messages = projectNativeItemsToAgentMessages([
    {
      id: "w1",
      createdAt: "2026-01-01T00:00:03Z",
      item: {
        type: "warning",
        code: "PROVIDER",
        message: "rate limit exceeded",
        retryable: false,
      },
    },
  ]);
  assert.equal(messages.length, 1);
  assert.equal(messages[0]?.role, "assistant");
  assert.equal(messages[0]?.stopReason, "error");
  assert.equal(messages[0]?.errorMessage, "rate limit exceeded");
  const text = (messages[0]?.content as Array<{ text?: string }>)[0]?.text ?? "x";
  assert.equal(text, "");
});

test("restore projects shell BackgroundTask as bashExecution", () => {
  const messages = projectNativeItemsToAgentMessages([
    {
      id: "bt1",
      createdAt: "2026-01-01T00:00:04Z",
      item: {
        type: "backgroundTask",
        taskKind: "shell",
        state: "completed",
        command: "echo hi",
        output: "hi\n",
        exitCode: 0,
      },
    },
  ]);
  assert.equal(messages.length, 1);
  assert.equal(messages[0]?.role, "bashExecution");
  assert.equal(messages[0]?.command, "echo hi");
  assert.equal(messages[0]?.output, "hi\n");
  assert.equal(messages[0]?.exitCode, 0);
});

test("applyFailedTurnsToAgentMessages annotates assistant on failed turns", () => {
  const messages = applyFailedTurnsToAgentMessages(
    [
      {
        role: "assistant",
        content: [{ type: "text", text: "partial" }],
        stopReason: null,
        errorMessage: null,
      },
    ],
    [{ status: "failed", error: { message: "boom" } }],
  );
  assert.equal(messages[0]?.stopReason, "error");
  assert.equal(messages[0]?.errorMessage, "boom");
});
