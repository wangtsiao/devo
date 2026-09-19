---
name: compact
description: Check context usage and compact the conversation from the Python REPL. Use when context is filling up and substantial work remains, so the session is summarized and you keep working instead of stopping early.
---

# Compact

Compaction replaces older conversation history with a dense summary, freeing
context so long-running work can continue. The host owns compaction (same path
as `/compact`); this skill is the kernel-side interface.

```python
await compact.status()
await compact.run()
await compact.run("keep the failing test names and the migration checklist")
```

## API

- `await compact.status()` — context usage: `tokens`, `context_window`, `percent`, `scheduled`.
- `await compact.run(instructions=None)` — schedule compaction for turn end.

## Rules

- Compaction never runs mid-cell; it runs when the current turn ends.
- The Python kernel persists through compaction (variables remain).
- Check `await compact.status()` when unsure; keep working after `run`.

**Status (P1b):** Python package stub — `host_request` lands in P2.
