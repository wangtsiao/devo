import assert from "node:assert/strict";
import { test } from "node:test";
import { formatCollapsedToolPreview, summarizeToolArgs } from "./tool-execution.js";

test("summarizeToolArgs shows web_search query", () => {
	assert.equal(summarizeToolArgs("web_search", { query: "Rust async docs" }), "Rust async docs");
	assert.equal(summarizeToolArgs("websearch", { q: "short" }), "short");
});

test("formatCollapsedToolPreview never keeps huge dumps", () => {
	const huge = Array.from({ length: 200 }, (_, i) => `hit ${i}: ${"x".repeat(80)}`).join("\n");
	const preview = formatCollapsedToolPreview(huge);
	assert.ok(preview.length < 320);
	assert.match(preview, /more lines/);
});
