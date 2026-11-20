# DeepSeek Messages + restored-session Enter acceptance handoff

## Baseline evidence, not a new regression claim

GitHub #339 is merged at e1bae5a7dbefb5169b8528a9fee4fb3e46219d72;
#341 is merged at 2678ea953d2f58ea7733d37c1e5a5e8de900b5d3. PR descriptions
report controlled HTTP/SSE and runtime tests, not the user's private endpoint.
The retained release source contains the corresponding mechanisms:

- `crates/provider/src/anthropic/stream_usage.rs`: message_start is progress;
  message_delta usage gets terminal provenance; zero-only initial counters do
  not become reported final usage if final usage is absent. Fields update when
  present, so omitted counters are not an instruction to reset previous values.
- The same module and `anthropic/messages/response_metadata.rs`: input is
  uncached input + cache creation + cache read, using saturating addition.
- `crates/provider/tests/usage_presence.rs:663+`: all three protocol families
  distinguish initial zero/missing final, explicit terminal zero, later errors.
- `crates/server/src/usage_ledger/completion_tests.rs`: completion observer and
  failed-terminal zero accounting tests; `usage_ledger.rs:488` filters provisional
  all-zero stream samples, not every sample whose uncached input is zero.
- `apps/tui/src/native-agent-connection.test.ts:2389`: positive occupancy survives
  placeholder query usage, while authoritative context/usageUpdated zero clears it.

Thus a new report of Enter resetting restored usage is a reproduction target,
not proof these fixes are absent or that DeepSeek is responsible. Need actual
provider identity, protocol, lifecycle and usage provenance to classify it.

## Required controlled end-to-end scenario (NOT run here)

Use real Devo Rust server + product TUI in tmux, isolated disposable home, and a
loopback HTTP/SSE fixture without real keys. Explicitly adopt the selected model
Messages route through the final production connection path once implemented;
verify request path `/anthropic/v1/messages` and exact request model, not just UI
provider name. Do not test against a live credentialed endpoint.

Restore a session with measured context 120 / 1000. Capture that stable state,
enter a new prompt and press Enter. Gate the mock after message_start so the
intermediate state is observable, not inferred from a fast final screen.

1. **Provisional zero:** message_start usage carries input/output/cache fields all
   zero. Before releasing any final event, restored occupancy must remain 120,
   not flash to zero. End with message_delta stop_reason but no usage, then
   message_stop: no invented measured-zero call. The call/outcome still exists;
   usage coverage remains unknown/unreported, previous measured occupancy is not
   replaced merely by this placeholder.
2. **Disjoint cache counters:** start with input_tokens=11,
   cache_creation_input_tokens=3, cache_read_input_tokens=5; final output_tokens=7.
   Assert normalized adapter input=19, output=7 and accounting total=26. Native
   displays cache read/write as subsets of input; never produce 34 by adding
   cache again. Context occupancy and cumulative spend are separate projections,
   so do not compare restored occupancy directly to cumulative total=26.
3. **Cache-only positive:** uncached input=0, cache read=80, cache creation=0,
   output=0. This is a meaningful 80-token measurement, not provisional zero.
4. **Explicit terminal zero:** after positive progress, send message_delta usage
   explicitly setting input/output/cache creation/cache read to zero, then stop.
   Assert final measured zero replaces progress, retains a metered call, and an
   authoritative zero occupancy event clears the TUI. Repeat with a subsequent
   malformed event: terminal measurement must not be discarded merely due to error.
5. **Provenance limit:** arbitrary phase-less compatibility usage cannot always be
   classified as startup versus final. Do not invent provenance or trust a JSON
   terminal_sample boolean: the marker is internal, not an incoming wire field.

Also gate a stale restored-session context read until after the new authoritative
update; its late result must not overwrite the newer measurement. Repeat session
switch and next Enter to check event ownership. Screenshots/TTY capture, Native
notification sequence and normalized ledger assertions should be saved separately.

## What ran in this pass

`node --test experiments/explicit-route-adoption/usage-boundary.test.mjs`
passed 4/4 using Node 24's built-in TypeScript stripping and the **actual** retained
`apps/tui/src/native-to-pi/usage.ts`, not a reimplementation. It verifies the last
Native-to-TUI accounting boundary for 19+7=26, cache-only 80, unreported zero and
measured zero. No dependencies installed, product edits, authenticated calls,
Rust compilation, real SSE runtime, renderer, Enter action or TTY execution.

The raw wire-to-normalized transformation and restored-session occupancy scenarios
above remain explicit acceptance gates. These passing boundary tests must not be
reported as the whole restored-session Enter regression passing.

## Porting the released tmux harness (2026-11-20)

Reuse lifecycle, trace reading and capture/timeout from
`apps/tui/scripts/python-code-stream-smoke.mjs`; reuse event framing from
`apps/tui/scripts/smoke-mock-provider.mjs`. The local
`messages-usage-fixture.mjs` supplies split progress and terminal buffers for
the seed, missing final usage, cached terminal sample and explicit zero, not a
new terminal harness. Its tests only check frame construction.

1. Run a real Native session seeded by a completed measured turn, recover the
   session ID from the trace, and resume it before sending the scenario marker.
   Validate the actual context window from Native; use 1000 only if explicitly
   configured. Assert the pre-Enter snapshot and wait for a matching resume
   response rather than assuming a screen repaint means restore is finished.
2. Match the foreground POST by path, model and a unique user prompt; handle
   title requests separately. Send the progress buffer then wait until its
   unique assistant marker renders before capturing the in-progress meter.
   This proves bytes were consumed before the assertion. Keep the terminal
   buffer gated until that capture. Do not use an arbitrary sleep or rely
   solely on a request counter that a background turn can increment.
3. Release the terminal buffer and capture the completed turn. Use exact
   occupancy in Native, UI-formatted occupancy in tmux, and call-level billing
   evidence separately. Keep a short capture sequence while gated to catch a
   transient reset; a single post-marker pane does not rule out a brief flash.
4. A baseline with an explicitly pinned loopback Messages route can verify
   the Enter regression independently now. It cannot verify catalog adoption,
   explicit login or live DeepSeek. Repeat after production route adoption is
   available and keep those results separate.
