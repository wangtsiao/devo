/** Offline explicit-approval prototype. No auth store, network or disk writes.
 * Production caller must supply authoritative config/catalog/auth-kind snapshots,
 * serialize the final read/commit with every configuration writer, then atomically
 * persist the single returned owner document before publishing a new router.
 */
import { createHash, timingSafeEqual } from 'node:crypto';

function canonical(value) {
  if (Array.isArray(value)) return value.map(canonical);
  if (value && typeof value === 'object') return Object.fromEntries(
    Object.keys(value).sort().map(key => [key, canonical(value[key])]),
  );
  return value;
}
function digest(value) {
  return createHash('sha256').update(JSON.stringify(canonical(value))).digest('hex');
}
function inspect(snapshot) {
  const { providerId, modelId, authKind, route, connections, custom } = snapshot;
  if (authKind !== 'api_key' || providerId === 'openai-codex') throw Error('Explicit API-key connection required; OAuth is outside this prototype');
  if (!providerId || !modelId || !route) throw Error('Specific provider, model and route required');
  const owners = [connections, custom].map(doc => doc.provider?.[providerId]);
  if (!owners.some(Boolean)) throw Error('Existing credential-bound connection required');
  const owner = owners[1] ? 'custom' : 'connections';
  // Inspect both authored files, not only the effective overlay. Hidden partial
  // user routes must never become permission to redirect an existing credential.
  for (const entry of owners.filter(Boolean)) {
    if (entry.enabled === false) throw Error('Disabled connection requires explicit repair');
    for (const authored of [entry, entry.models?.[modelId], entry.model_overrides?.[modelId]].filter(Boolean)) {
      if (['base_url', 'wire_api', 'request_model'].some(key => Object.hasOwn(authored, key))) {
        throw Error('Existing complete or partial user transport must be preserved');
      }
    }
  }
  const credentials = owners.filter(Boolean).map(entry => entry.credential).filter(Boolean);
  if (!credentials.length || new Set(credentials).size !== 1) throw Error('Missing or ambiguous credential binding requires explicit repair');
  const { endpoint, wireApi, requestModel } = route;
  if (!['anthropic_messages', 'openai_responses', 'openai_chat_completions'].includes(wireApi)) throw Error('Unsupported route');
  if (typeof requestModel !== 'string' || !requestModel.trim()) throw Error('Request model required');
  const url = new URL(endpoint);
  if (!['https:', 'http:'].includes(url.protocol) || url.username || url.password || url.search || url.hash || endpoint.includes('${')) throw Error('Unrepresentable endpoint');
  if (wireApi !== 'anthropic_messages' && !url.pathname.replaceAll('/', '')) throw Error('OpenAI root base changes target under server normalization');
  // Route is already normalized by reviewed catalog conversion. Show the exact
  // resulting request target as well as the persisted adapter base.
  const base = endpoint.replace(/\/+$/, '');
  if (wireApi === 'anthropic_messages' && base.endsWith('/v1')) throw Error('Messages route must be normalized before approval');
  const suffix = { anthropic_messages: '/v1/messages', openai_responses: '/responses', openai_chat_completions: '/chat/completions' }[wireApi];
  return { owner, view: { providerId, modelId, wireApi, endpoint: base, requestModel, requestUrl: base + suffix } };
}

/** User sees only view and approvalDigest, never config or credential identifiers. */
export function prepare(snapshot) {
  const { owner, view } = inspect(snapshot);
  return { view, approvalDigest: digest({ snapshot, owner, view }) };
}

/** Explicit accept only: returns an unwritten candidate, not a persistence claim. */
export function accept(currentSnapshot, approvalDigest) {
  const proposal = prepare(currentSnapshot);
  const supplied = Buffer.from(approvalDigest ?? '', 'utf8');
  const expected = Buffer.from(proposal.approvalDigest, 'utf8');
  if (supplied.length !== expected.length || !timingSafeEqual(supplied, expected)) throw Error('Configuration or route changed; show a fresh proposal');
  const { owner, view } = inspect(currentSnapshot);
  const document = structuredClone(currentSnapshot[owner]);
  const provider = document.provider[currentSnapshot.providerId];
  provider.models ??= {};
  provider.models[currentSnapshot.modelId] = {
    ...provider.models[currentSnapshot.modelId],
    base_url: view.endpoint,
    wire_api: view.wireApi,
    request_model: view.requestModel,
  };
  return { owner, document };
}
