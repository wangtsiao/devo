/**
 * Native AgentConnection adapter for Devo InteractiveMode.
 *
 * Product rules (L2-DES-RLM-001):
 * - Talk Native dated `initialize` only (no ACP, no Prime daemon).
 * - Config/auth under `~/.devo` — never `~/.prime`.
 * - Strip Prime-only slash commands; map Devo `/refine`, `/btw`, `/goal`, etc.
 * - Launch inherits TTY (exec/replace); no piped sidecar.
 *
 * P5 MVP: methods match Prime AgentConnection shapes so InteractiveMode can
 * wire later. RPCs are emitted when a session is bound; otherwise structured
 * stubs / clear errors.
 */

export const STRIP_PRIME_COMMANDS = [
  "/login",
  "/logout",
  "/update",
  "/heartbeat",
  "/heartbeats",
  "/autonomous",
  "/share",
  "/system-prompt",
  "/traces",
  "/rlm-max-depth",
  "/fast",
  "/import",
  "/export",
];

export const DEVO_SLASH_COMMANDS = [
  "/refine",
  "/btw",
  "/goal",
  "/permissions",
  "/diff",
  "/delete",
  "/show-reasoning",
  "/skills",
  "/model",
];

/** Prime-compatible empty `ipython` tool details (docs/rlm-native-api.md). */
export function emptyIpythonDetails(overrides = {}) {
  return {
    stdout: "",
    stderr: "",
    result: null,
    status: "ok",
    errorName: null,
    errorValue: null,
    traceback: [],
    durationMs: 0,
    diffs: [],
    sentAgentMessages: [],
    backgroundOutput: null,
    ...overrides,
  };
}

/** Model-facing `ipython` tool definition for RLM sessions (DD-1). */
export function ipythonToolDefinition() {
  return {
    name: "ipython",
    label: "ipython",
    description:
      "Execute Python code in the session RLM kernel. Namespace persists across cells and turns.",
    promptSnippet: "Run Python in the session kernel via the ipython tool.",
    promptGuidelines: [
      "Use only the ipython tool for model-facing work in RLM sessions.",
      "File/shell/search/MCP go through kernel host_request, not model schemas.",
    ],
    parameters: {
      type: "object",
      properties: {
        code: { type: "string", description: "Python source to execute" },
      },
      required: ["code"],
      additionalProperties: false,
    },
    renderShell: "self",
    replayBuiltInToolName: "ipython",
  };
}

function newIdempotencyKey(prefix) {
  return `${prefix}_${Date.now().toString(36)}_${Math.random().toString(36).slice(2, 10)}`;
}

function textInput(message) {
  return [{ type: "text", text: message }];
}

function idleState(sessionId) {
  return {
    sessionId: sessionId ?? null,
    isStreaming: false,
    isCompacting: false,
    model: null,
    thinkingLevel: null,
    steeringMode: "all",
    followUpMode: "all",
    autoCompactionEnabled: true,
    autoRetryEnabled: true,
    queue: { steering: [], followUp: [] },
    waitingPhase: null,
  };
}

function emptySnapshot(sessionId) {
  return {
    state: idleState(sessionId),
    messages: [],
    sessionHeader: sessionId
      ? {
          type: "session",
          id: sessionId,
          timestamp: new Date().toISOString(),
          cwd: process.cwd(),
        }
      : undefined,
    rlmChildren: [],
  };
}

export class NativeAgentConnection {
  /**
   * @param {(line: string) => void} writeLine NDJSON sink toward the Native server
   * @param {string} protocolVersion Dated Native protocol version (exact match)
   * @param {{ sessionId?: string | null, expectedTurnId?: string | null }} [options]
   */
  constructor(writeLine, protocolVersion, options = {}) {
    this.writeLine = writeLine;
    this.protocolVersion = protocolVersion;
    this.nextId = 1;
    this.pending = new Map();
    this.notificationHandlers = new Set();
    this.eventListeners = new Set();
    this.sessionId = options.sessionId ?? null;
    this.expectedTurnId = options.expectedTurnId ?? null;
    this.subscriptionId = null;
    this.messages = [];
    this.queue = { steering: [], followUp: [] };
    this._state = idleState(this.sessionId);
  }

  handleIncomingLine(line) {
    const trimmed = line.trim();
    if (!trimmed) return;
    let msg;
    try {
      msg = JSON.parse(trimmed);
    } catch {
      return;
    }
    if (msg.method && msg.id === undefined) {
      for (const handler of this.notificationHandlers) {
        handler(msg.method, msg.params);
      }
      this.#emit({
        type: "session_event",
        event: { type: "native_notification", method: msg.method, params: msg.params },
      });
      return;
    }
    if (typeof msg.id === "number") {
      const pending = this.pending.get(msg.id);
      if (!pending) return;
      this.pending.delete(msg.id);
      if (msg.error) {
        pending.reject(new Error(msg.error.message ?? JSON.stringify(msg.error)));
      } else {
        pending.resolve(msg.result);
      }
    }
  }

  onNotification(handler) {
    this.notificationHandlers.add(handler);
    return () => this.notificationHandlers.delete(handler);
  }

  /** Local AgentConnection event bus (Prime `subscribe`). */
  subscribe(listener) {
    this.eventListeners.add(listener);
    return () => this.eventListeners.delete(listener);
  }

  #emit(event) {
    for (const listener of this.eventListeners) {
      try {
        const out = listener(event);
        if (out && typeof out.then === "function") {
          out.catch(() => {});
        }
      } catch {
        // Listener errors must not break the adapter.
      }
    }
  }

  async request(method, params) {
    const id = this.nextId++;
    const payload = {
      jsonrpc: "2.0",
      id,
      method,
      params,
    };
    const result = new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
    });
    this.writeLine(JSON.stringify(payload));
    return result;
  }

  notify(method, params) {
    this.writeLine(JSON.stringify({ jsonrpc: "2.0", method, params }));
  }

  requireSessionId() {
    if (!this.sessionId) {
      throw new Error(
        "NativeAgentConnection: no session bound — call bindSession/newSession or pass sessionId",
      );
    }
    return this.sessionId;
  }

  bindSession(sessionId, expectedTurnId = null) {
    this.sessionId = sessionId;
    this.expectedTurnId = expectedTurnId;
    this._state = idleState(sessionId);
  }

  async initialize(clientInfo) {
    return this.request("initialize", {
      protocolVersion: this.protocolVersion,
      clientInfo,
      capabilities: {},
    });
  }

  /**
   * Native `subscription/create` for the bound session (visible session only).
   * Also registers a local listener via `subscribe`.
   */
  async createNativeSubscription(selectors, includeSnapshot = true) {
    const sessionId = this.requireSessionId();
    const result = await this.request("subscription/create", {
      selectors: selectors ?? [{ type: "session", sessionId }],
      includeSnapshot,
      after: [],
    });
    this.subscriptionId = result?.subscriptionId ?? null;
    return result;
  }

  async unsubscribeNative() {
    if (!this.subscriptionId) {
      return { ok: true };
    }
    const result = await this.request("subscription/unsubscribe", {
      subscriptionId: this.subscriptionId,
    });
    this.subscriptionId = null;
    return result;
  }

  async getState() {
    return { ...this._state, queue: { ...this.queue } };
  }

  async getInitialSnapshot() {
    return {
      ...emptySnapshot(this.sessionId),
      state: await this.getState(),
      messages: [...this.messages],
    };
  }

  async getMessages() {
    if (!this.sessionId) {
      return [...this.messages];
    }
    try {
      const page = await this.request("session/items/list", {
        sessionId: this.sessionId,
      });
      const items = page?.items ?? page?.entries ?? [];
      if (Array.isArray(items) && items.length > 0) {
        this.messages = items;
      }
    } catch {
      // Fall back to local cache when server is not answering yet.
    }
    return [...this.messages];
  }

  async getQueue() {
    return { ...this.queue };
  }

  async clearQueue() {
    this.queue = { steering: [], followUp: [] };
    this._state.queue = { ...this.queue };
    return this.getQueue();
  }

  /** Heartbeats are stripped in Devo; IM may still call this. */
  async listHeartbeats() {
    return [];
  }

  async getToolDefinition(name) {
    if (name === "ipython") {
      return ipythonToolDefinition();
    }
    return undefined;
  }

  getIpythonToolDefinition() {
    return ipythonToolDefinition();
  }

  /**
   * @param {string} message
   * @param {{ streamingBehavior?: "steer" | "followUp", queueIfBusy?: boolean, images?: unknown[] }} [options]
   */
  async prompt(message, options = {}) {
    const sessionId = this.requireSessionId();
    const input = textInput(message);
    const idempotencyKey = newIdempotencyKey("prompt");

    if (options.streamingBehavior === "steer") {
      return this.steer(message, options.images);
    }
    if (options.streamingBehavior === "followUp" || options.queueIfBusy) {
      return this.followUp(message, options.images);
    }

    this._state.isStreaming = true;
    const result = await this.request("turn/start", {
      sessionId,
      input,
      idempotencyKey,
    });
    if (result?.turn?.id) {
      this.expectedTurnId = result.turn.id;
    }
    return result;
  }

  async steer(message, _images) {
    const sessionId = this.requireSessionId();
    if (!this.expectedTurnId) {
      this.queue.steering.push(message);
      this._state.queue = { ...this.queue };
      return this.request("session/queue/steer", {
        sessionId,
        input: textInput(message),
      }).catch(() => {
        // Structured local queue when server rejects / turn not steerable yet.
        return { outcome: "queuedLocal", lane: "steering", message };
      });
    }
    this.queue.steering.push(message);
    return this.request("turn/steer", {
      sessionId,
      expectedTurnId: this.expectedTurnId,
      input: textInput(message),
      idempotencyKey: newIdempotencyKey("steer"),
    });
  }

  async followUp(message, _images) {
    const sessionId = this.requireSessionId();
    this.queue.followUp.push(message);
    this._state.queue = { ...this.queue };
    return this.request("session/queue/push", {
      sessionId,
      input: textInput(message),
      idempotencyKey: newIdempotencyKey("queue"),
    });
  }

  async queuePush(message) {
    return this.followUp(message);
  }

  async abort() {
    if (!this.sessionId) {
      this._state.isStreaming = false;
      return { interrupted: false, reason: "no_session" };
    }
    const result = await this.request("session/interrupt", {
      sessionId: this.sessionId,
    });
    this._state.isStreaming = false;
    this.expectedTurnId = null;
    return result;
  }

  async abortAndClearQueue() {
    await this.abort();
    return this.clearQueue();
  }

  async compact(_customInstructions) {
    const sessionId = this.requireSessionId();
    this._state.isCompacting = true;
    this.#emit({
      type: "session_event",
      event: { type: "compaction_start", reason: "manual" },
    });
    try {
      const result = await this.request("session/compact/start", { sessionId });
      this._state.isCompacting = false;
      this.#emit({
        type: "session_event",
        event: {
          type: "compaction_end",
          reason: "manual",
          result: result ?? { summary: "", firstKeptEntryId: "", tokensBefore: 0 },
          aborted: false,
          willRetry: false,
        },
      });
      return result ?? { summary: "", firstKeptEntryId: "", tokensBefore: 0 };
    } catch (error) {
      this._state.isCompacting = false;
      throw error;
    }
  }

  async abortCompaction() {
    this._state.isCompacting = false;
    this.#emit({
      type: "session_event",
      event: {
        type: "compaction_end",
        reason: "manual",
        result: undefined,
        aborted: true,
        willRetry: false,
      },
    });
  }

  /** `/refine` maps here — never send refine instructions as prompt text. */
  async refineRun(params = {}) {
    const sessionId = params.sessionId ?? this.requireSessionId();
    return this.request("session/refine/run", {
      sessionId,
      instructions: params.instructions,
      global: params.global ?? false,
      rollbackId: params.rollbackId,
    });
  }

  async refine(options = {}) {
    return this.refineRun(options);
  }

  /**
   * @param {{ cwd?: string, idempotencyKey?: string }} [options]
   * @returns {Promise<{ cancelled: boolean, session?: unknown, error?: string }>}
   */
  async newSession(options = {}) {
    try {
      const result = await this.request("session/new", {
        cwd: options.cwd ?? process.cwd(),
        idempotencyKey: options.idempotencyKey ?? newIdempotencyKey("session_new"),
      });
      const id = result?.session?.id ?? result?.sessionId;
      if (id) {
        await this.unsubscribeNative().catch(() => {});
        this.bindSession(id);
        this.messages = [];
        this.#emit({
          type: "session_replaced",
          state: await this.getState(),
          messages: [],
        });
        return { cancelled: false, session: result.session ?? result };
      }
      return {
        cancelled: true,
        error: "session/new returned no session id",
      };
    } catch (error) {
      return {
        cancelled: true,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  /**
   * Switch visible session: unsubscribe previous, resume target.
   * @param {string} sessionPathOrId session id (Native) or path (Prime legacy)
   */
  async switchSession(sessionPathOrId, _options = {}) {
    if (!sessionPathOrId) {
      return {
        cancelled: true,
        error: "switchSession requires a session id",
      };
    }
    // Paths are Prime-local; Devo Native addresses sessions by id.
    if (sessionPathOrId.includes("/") || sessionPathOrId.includes("\\")) {
      return {
        cancelled: true,
        error:
          "switchSession: filesystem session paths are unsupported — pass a Native session id",
      };
    }
    try {
      await this.unsubscribeNative().catch(() => {});
      const result = await this.request("session/resume", {
        sessionId: sessionPathOrId,
      });
      this.bindSession(result?.session?.id ?? sessionPathOrId);
      this.messages = [];
      const snapshot = await this.getInitialSnapshot();
      this.#emit({ type: "session_resynced", snapshot });
      return { cancelled: false };
    } catch (error) {
      return {
        cancelled: true,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  /**
   * @param {string} entryId leaf / turn id (mapped to Native `atTurnId`)
   * @param {{ position?: "before" | "at" }} [options] Prime position → Native `cut`
   * @returns {Promise<{ cancelled: boolean, selectedText?: string, error?: string, session?: unknown }>}
   */
  async fork(entryId, options = {}) {
    if (!this.sessionId) {
      return {
        cancelled: true,
        error: "fork: no session bound",
      };
    }
    if (!entryId) {
      return {
        cancelled: true,
        error: "fork: entryId is required",
      };
    }
    try {
      const cut = options.position === "before" ? "before" : "through";
      const result = await this.request("session/fork", {
        sessionId: this.sessionId,
        atTurnId: entryId,
        cut,
      });
      const id = result?.session?.id;
      if (id) {
        await this.unsubscribeNative().catch(() => {});
        this.bindSession(id);
      }
      return {
        cancelled: false,
        session: result?.session,
        selectedText: result?.selectedText,
      };
    } catch (error) {
      return {
        cancelled: true,
        error: error instanceof Error ? error.message : String(error),
      };
    }
  }

  isPrimeCommandStripped(command) {
    const name = command.trim().split(/\s+/)[0]?.toLowerCase() ?? "";
    return STRIP_PRIME_COMMANDS.includes(name);
  }

  /**
   * Project a Native `Item::Refinement` (or envelope) to InteractiveMode
   * `refinement_outcome` shape (`edits[]` from `changes`).
   */
  projectRefinementOutcome(itemOrEnvelope) {
    const item = itemOrEnvelope?.item ?? itemOrEnvelope;
    if (!item || (item.type !== "refinement" && item.kind !== "refinement")) {
      if (!item?.refinementId && !item?.refinement_id) {
        return null;
      }
    }
    const refinementId = item.refinementId ?? item.refinement_id ?? "";
    const changes = item.changes ?? [];
    return {
      type: "refinement_outcome",
      refinementId,
      trigger: item.trigger ?? "",
      summary: item.summary ?? "",
      edits: changes.map((change) =>
        typeof change === "string" ? { description: change } : change,
      ),
      evidence: item.evidence ?? null,
      outcome: item.outcome ?? null,
    };
  }

  /** Map Native waiting / Ask phases to IM waitingPhase strings. */
  setWaitingPhase(phase) {
    const allowed = new Set([null, "kernel", "host_request", "refine", "ask"]);
    this._state.waitingPhase = allowed.has(phase) ? phase : phase;
    return this._state.waitingPhase;
  }
}

export function notImplemented() {
  throw new Error(
    "InteractiveMode UI not vendored yet — NativeAgentConnection is ready; pin Prime InteractiveMode under vendor/ (see README).",
  );
}
