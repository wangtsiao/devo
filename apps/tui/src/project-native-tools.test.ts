/**
 * Trace: tool lifecycle projection (pending → args → start → end).
 */

import { test } from "node:test";
import assert from "node:assert/strict";
import {
  createToolProjectionState,
  displayToolName,
  editResultFromFileChange,
  fileChangeSummariesFromWorkspaceViews,
  normalizeToolArgs,
  projectToolCallInputDelta,
  projectToolItemStarted,
  projectToolStatusUpdated,
  shapeToolResultOutput,
  summarizeWebSearchOutput,
} from "./project-native-tools.js";

test("displayToolName maps exec_command to bash", () => {
  assert.equal(displayToolName("exec_command"), "bash");
  assert.equal(displayToolName("apply_patch"), "edit");
  assert.equal(displayToolName("ipython"), "ipython");
});

test("shapeToolResultOutput summarizes hosted web_search hit arrays", () => {
  const hits = [
    { title: "Devo docs", url: "https://example.com/a" },
    { title: "Coding agent", url: "https://example.com/b" },
    { title: "Extra", url: "https://example.com/c" },
  ];
  const shaped = shapeToolResultOutput(hits, "status: completed");
  assert.equal(shaped.content.length, 1);
  assert.match(shaped.content[0]?.text ?? "", /3 results: Devo docs; Coding agent; Extra/);
  assert.equal(shaped.details?.resultKind, "web_search");
  assert.equal(shaped.details?.hitCount, 3);
  assert.ok(!shaped.content[0]?.text?.includes("https://example.com"));
  assert.equal(summarizeWebSearchOutput(hits).startsWith("3 results:"), true);
});

test("shapeToolResultOutput summarizes JSON-string web_search dumps", () => {
  const hits = [
    { title: "One", url: "https://example.com/1" },
    { title: "Two", url: "https://example.com/2" },
  ];
  const shaped = shapeToolResultOutput(JSON.stringify(hits));
  assert.match(shaped.content[0]?.text ?? "", /2 results: One; Two/);
  assert.equal(shaped.details?.resultKind, "web_search");
});

test("shapeToolResultOutput truncates huge plain strings", () => {
  const shaped = shapeToolResultOutput("z".repeat(5000));
  assert.ok((shaped.content[0]?.text?.length ?? 0) <= 400);
  assert.equal(shaped.details?.resultKind, "truncated_output");
});

test("normalizeToolArgs promotes cmd to command for bash", () => {
  assert.deepEqual(normalizeToolArgs("exec_command", { cmd: "ls" }), {
    command: "ls",
    cmd: "ls",
  });
});

test("inputDelta accumulates partial_json by itemId", () => {
  const state = createToolProjectionState();
  projectToolItemStarted(
    { id: "item_1" },
    { type: "toolCall", callId: "c1", toolName: "ipython", arguments: {} },
    state,
  );
  const first = projectToolCallInputDelta(
    {
      itemId: "item_1",
      delta: JSON.stringify({ tool_use_id: "c1", partial_json: '{"code":"ab' }),
    },
    state,
  );
  assert.equal(first[0]?.type, "tool_args_update");
  assert.equal((first[0]?.args as { code?: string }).code, "ab");

  const second = projectToolCallInputDelta(
    {
      itemId: "item_1",
      delta: JSON.stringify({ tool_use_id: "c1", partial_json: 'c"}' }),
    },
    state,
  );
  assert.equal((second[0]?.args as { code?: string }).code, "abc");
});

test("status in_progress emits args_complete then execution_start", () => {
  const state = createToolProjectionState();
  projectToolItemStarted(
    { id: "item_1" },
    { type: "toolCall", callId: "c1", toolName: "ipython", arguments: { code: "1" } },
    state,
  );
  const events = projectToolStatusUpdated({ toolCallId: "c1", status: "in_progress" }, state);
  assert.deepEqual(
    events.map((e) => e.type),
    ["tool_args_complete", "tool_execution_start"],
  );
});

test("editResultFromFileChange maps unifiedDiff for edit renderer", () => {
  const shaped = editResultFromFileChange({
    type: "fileChange",
    callId: "c1",
    changes: [
      {
        path: "src/a.ts",
        change: { type: "update", unifiedDiff: "--- a\n+++ b\n@@\n-old\n+new\n" },
      },
    ],
  });
  assert.equal(shaped.details.path, "src/a.ts");
  assert.match(String(shaped.details.diff), /\+new/);
});

test("fileChangeSummariesFromWorkspaceViews maps additions/deletions", () => {
  const changes = fileChangeSummariesFromWorkspaceViews({
    views: [
      {
        files: [
          { path: "a.ts", additions: 2, deletions: 1, status: "modified" },
          { path: "b.ts", status: "added" },
        ],
      },
    ],
  });
  assert.deepEqual(changes, [
    { path: "a.ts", added: 2, removed: 1 },
    { path: "b.ts", added: 1, removed: 0 },
  ]);
});
