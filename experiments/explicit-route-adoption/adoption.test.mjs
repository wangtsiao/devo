import test from 'node:test';
import assert from 'node:assert/strict';
import { prepare, accept } from './adoption.mjs';

function fixture() {
  return {
    providerId: 'deepseek', modelId: 'deepseek-v4-pro', authKind: 'api_key',
    route: { endpoint: 'https://api.deepseek.com/anthropic', wireApi: 'anthropic_messages', requestModel: 'deepseek-v4-pro' },
    connections: { model: 'other/default', provider: { deepseek: { credential: 'retained-reference', models: { untouched: { base_url: 'https://manual.example/v1', wire_api: 'openai_responses' } } } } },
    custom: { provider: {} },
  };
}

test('explicit approval installs only selected tuple, preserving owner, bindings and defaults', () => {
  const snapshot = fixture();
  const before = structuredClone(snapshot);
  const proposal = prepare(snapshot);
  assert.equal(proposal.view.requestUrl, 'https://api.deepseek.com/anthropic/v1/messages');
  assert.ok(!JSON.stringify(proposal).includes('retained-reference'));
  const result = accept(snapshot, proposal.approvalDigest);
  const expected = structuredClone(snapshot.connections);
  expected.provider.deepseek.models['deepseek-v4-pro'] = {
    base_url: 'https://api.deepseek.com/anthropic', wire_api: 'anthropic_messages', request_model: 'deepseek-v4-pro',
  };
  assert.deepEqual(result, { owner: 'connections', document: expected });
  assert.deepEqual(snapshot, before);
});

test('custom owner retained and unrelated overlay not rewritten', () => {
  const snapshot = fixture();
  snapshot.custom.provider.deepseek = { credential: 'retained-reference', models: { 'deepseek-v4-pro': { context_window: 1000 } } };
  const approved = prepare(snapshot);
  const result = accept(snapshot, approved.approvalDigest);
  assert.equal(result.owner, 'custom');
  assert.equal(result.document.provider.deepseek.models['deepseek-v4-pro'].context_window, 1000);
  assert.deepEqual(snapshot.connections, fixture().connections);
});

test('all stale route and config approvals rejected', () => {
  for (const change of [
    s => { s.route.endpoint = 'https://new.example/anthropic'; },
    s => { s.route.requestModel = 'new-wire-id'; },
    s => { s.connections.provider.deepseek.credential = 'different-binding'; },
    s => { s.connections.model = 'new/default'; },
    s => { s.custom.provider.other = { enabled: true }; },
  ]) {
    const snapshot = fixture();
    const approved = prepare(snapshot);
    change(snapshot);
    assert.throws(() => accept(snapshot, approved.approvalDigest));
  }
});

test('complete, partial, blank and hidden manual routes remain untouched', () => {
  for (const doc of ['connections', 'custom']) {
    for (const location of ['provider', 'models', 'model_overrides']) {
      for (const route of [{ base_url: '' }, { wire_api: 'openai_responses' }, { request_model: 'alias' }, { base_url: 'https://user.example', wire_api: 'openai_responses' }]) {
        const snapshot = fixture();
        snapshot[doc].provider.deepseek ??= {};
        const provider = snapshot[doc].provider.deepseek;
        if (location === 'provider') Object.assign(provider, route);
        else { provider[location] ??= {}; provider[location][snapshot.modelId] = route; }
        const before = structuredClone(snapshot);
        assert.throws(() => prepare(snapshot), /transport must be preserved/);
        assert.deepEqual(snapshot, before);
      }
    }
  }
});

test('OAuth, ambiguous binding, disable and missing connection are outside activation', () => {
  for (const change of [s => { s.authKind = 'oauth'; }, s => { s.providerId = 'openai-codex'; },
    s => { s.custom.provider.deepseek = { credential: 'another' }; },
    s => { s.connections.provider.deepseek.enabled = false; },
    s => { delete s.connections.provider.deepseek; }]) {
    const snapshot = fixture(); change(snapshot);
    assert.throws(() => prepare(snapshot));
  }
});

test('approval required and endpoint target cannot be silently shifted', () => {
  const snapshot = fixture();
  assert.throws(() => accept(snapshot, ''));
  snapshot.route = { endpoint: 'https://gateway.example', wireApi: 'openai_responses', requestModel: 'model' };
  assert.throws(() => prepare(snapshot));
  snapshot.route.endpoint = 'https://gateway.example/v1';
  assert.equal(prepare(snapshot).view.requestUrl, 'https://gateway.example/v1/responses');
});
