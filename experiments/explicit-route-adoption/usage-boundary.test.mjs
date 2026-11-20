// Executes the actual retained TUI normalization module via Node 24 TS stripping.
// Does NOT execute the Rust Messages adapter, runtime, renderer, Enter or a TTY.
import test from 'node:test';
import assert from 'node:assert/strict';
import { nativeCumulativeUsage } from '../../apps/tui/src/native-to-pi/usage.ts';

test('normalized Messages input includes disjoint cache buckets exactly once', () => {
  // Provider wire fixture: uncached 11 + cache creation 3 + cache read 5.
  // Rust adapter must normalize this to 19 before this Native boundary.
  const actual = nativeCumulativeUsage({ inputTokens: 19, outputTokens: 7,
    cacheCreationInputTokens: 3, cacheReadInputTokens: 5, totalTokens: 26,
    callCount: 1, meteredCallCount: 1, unreportedFields: [] });
  assert.deepEqual(actual.tokens, { input: 19, output: 7, cacheRead: 5, cacheWrite: 3, total: 26 });
  assert.equal(actual.coverage, 'complete');
});

test('cache-only positive usage is not a placeholder zero', () => {
  const actual = nativeCumulativeUsage({ inputTokens: 80, outputTokens: 0,
    cacheReadInputTokens: 80, cacheCreationInputTokens: 0, totalTokens: 80,
    callCount: 1, meteredCallCount: 1, unreportedFields: [] });
  assert.deepEqual(actual.tokens, { input: 80, output: 0, cacheRead: 80, cacheWrite: 0, total: 80 });
});

test('unmeasured final after provisional zero stays unknown at Native boundary', () => {
  const actual = nativeCumulativeUsage({ inputTokens: 0, outputTokens: 0,
    cacheReadInputTokens: 0, cacheCreationInputTokens: 0, totalTokens: 0,
    callCount: 1, meteredCallCount: 0 });
  assert.deepEqual(actual.tokens, { input: null, output: null, cacheRead: null, cacheWrite: null, total: null });
  assert.equal(actual.coverage, 'unreported');
});

test('explicit measured final zero remains zero, not unknown', () => {
  const actual = nativeCumulativeUsage({ inputTokens: 0, outputTokens: 0,
    cacheReadInputTokens: 0, cacheCreationInputTokens: 0, totalTokens: 0,
    callCount: 1, meteredCallCount: 1, unreportedFields: [] });
  assert.deepEqual(actual.tokens, { input: 0, output: 0, cacheRead: 0, cacheWrite: 0, total: 0 });
  assert.equal(actual.coverage, 'complete');
});
