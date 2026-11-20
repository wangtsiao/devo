// Controlled Messages wire frames for the real-terminal acceptance smoke.
// No synthetic Native events, credentials, or provider requests are generated here.
const zero = { input_tokens: 0, output_tokens: 0,
  cache_creation_input_tokens: 0, cache_read_input_tokens: 0 };
const frame = (type, fields = {}) =>
  `event: ${type}\ndata: ${JSON.stringify({ type, ...fields })}\n\n`;

export function messagesUsageFixture({ model, id, marker, scenario }) {
  const cases = {
    seed: [{ ...zero, input_tokens: 120 }, { ...zero, input_tokens: 120 }],
    provisional: [zero, undefined],
    cached: [zero, { ...zero, input_tokens: 100, cache_read_input_tokens: 40 }],
    "explicit-zero": [{ ...zero, input_tokens: 120 }, zero],
  };
  if (!Object.hasOwn(cases, scenario)) throw new Error(`Unknown scenario: ${scenario}`);
  const [initial, final] = cases[scenario];
  return {
    // Hold here until the terminal renders the unique marker and a capture has
    // been saved; the marker precedes every terminal event.
    progress: [
      frame("message_start", { message: { type: "message", id, model, role: "assistant",
        content: [], stop_reason: null, stop_sequence: null, usage: initial } }),
      frame("content_block_start", { index: 0, content_block: { type: "text", text: "" } }),
      frame("content_block_delta", { index: 0, delta: { type: "text_delta", text: marker } }),
    ].join(""),
    finish: [
      frame("content_block_stop", { index: 0 }),
      frame("message_delta", { delta: { stop_reason: "end_turn", stop_sequence: null },
        ...(final === undefined ? {} : { usage: final }) }),
      frame("message_stop"),
    ].join(""),
  };
}
