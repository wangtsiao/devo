import assert from "node:assert/strict";
import test from "node:test";
import { messagesUsageFixture } from "./messages-usage-fixture.mjs";

const make = scenario => messagesUsageFixture({ model: "deepseek-v4-pro",
  id: "msg_acceptance", marker: "observed-before-finish", scenario });
const decode = stream => stream.trimEnd().split("\n\n").map(frame => {
  const [event, data] = frame.split("\n");
  const payload = JSON.parse(data.slice("data: ".length));
  assert.equal(event, `event: ${payload.type}`);
  return payload;
});
test("gated progress contains an observable text marker but no terminal event", () => {
  const { progress, finish } = make("provisional");
  const [start, , delta] = decode(progress);
  assert.equal(start.message.model, "deepseek-v4-pro");
  assert.equal(start.message.usage.input_tokens, 0);
  assert.equal(delta.delta.text, "observed-before-finish");
  assert.deepEqual(decode(progress).map(x => x.type),
    ["message_start", "content_block_start", "content_block_delta"]);
  const [, end, stop] = decode(finish);
  assert.equal(end.type, "message_delta");
  assert.equal(Object.hasOwn(end, "usage"), false);
  assert.equal(stop.type, "message_stop");
});
test("seed, terminal cache buckets and explicit zero remain distinct on the wire", () => {
  const terminal = scenario => decode(make(scenario).finish)[1].usage;
  assert.equal(terminal("seed").input_tokens, 120);
  assert.deepEqual(terminal("cached"), { input_tokens: 100, output_tokens: 0,
    cache_creation_input_tokens: 0, cache_read_input_tokens: 40 });
  assert.deepEqual(terminal("explicit-zero"), { input_tokens: 0, output_tokens: 0,
    cache_creation_input_tokens: 0, cache_read_input_tokens: 0 });
});
