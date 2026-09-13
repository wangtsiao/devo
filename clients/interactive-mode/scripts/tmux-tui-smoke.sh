#!/usr/bin/env bash
# InteractiveMode tmux scenario suite (Unix) — NEW TUI only (clients/interactive-mode).
# Never point this harness at legacy crates/tui.
#
# Safety (non-negotiable):
# - Only create/kill the session named $DEVO_TUI_TEST_SESSION (default: devo-tui-test).
# - Never run `tmux kill-server`.
# - Never kill unrelated tmux sessions.
# - Trap cleanup on EXIT; leave no orphan test session.
# - On Windows hosts without tmux, exit 0 with a skip message.
#
# Scenario matrix (ship gate for new InteractiveMode; expand through P6):
#   T0  Launch InteractiveMode under tmux TTY — process starts; not “non-TTY refused”
#   T1  Prompt / idle chrome — composer or status visible (adapter smoke markers until IM vendored)
#   T2  User prompt → assistant/ipython cell — Cell or streaming text appears
#   T3  Interrupt — Turn aborts; idle restored
#   T4  Ask / approval — Approval chrome or waiting phase; approve/deny path
#   T5  /refine — Refine loader / refinement_outcome row (not prompt text)
#   T6  Queue / steer — Second input queued or steered while busy
#   T7  Compact notice — Compaction start/end or summary row
#   T8  Agent message / bash-done preview — Agent message received / Background command finished
#   T9  Detach / reattach — Reattach shows prior InteractiveMode chrome
#   T10 /btw or side-question — Ephemeral path does not poison root harness
#
# Early P5: T0–T3 via adapter smoke markers; skip T4–T10 until InteractiveMode is
# vendored under vendor/. Do not point this harness at crates/tui.
#
# Usage:
#   bash scripts/tmux-tui-smoke.sh           # run T0–T3 (+ skip T4–T10)
#   bash scripts/tmux-tui-smoke.sh T0        # single scenario
#   npm run test:tmux
set -euo pipefail

SESSION="${DEVO_TUI_TEST_SESSION:-devo-tui-test}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REQUESTED="${1:-all}"

if ! command -v tmux >/dev/null 2>&1; then
  echo "skip: tmux not available (expected on windows-latest CI)"
  exit 0
fi

cleanup() {
  if tmux has-session -t "$SESSION" 2>/dev/null; then
    tmux kill-session -t "$SESSION"
  fi
}
trap cleanup EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_pane_not_nontty() {
  local pane="$1"
  if echo "$pane" | grep -qi "Piped sidecars are invalid\|requires an inherited TTY"; then
    fail "non-TTY refuse seen under tmux (expected inherited TTY)"
  fi
}

capture_after_launch() {
  local cmd="$1"
  cleanup
  tmux new-session -d -s "$SESSION" -x 80 -y 24
  # Launch InteractiveMode entry only — never crates/tui / legacy ratatui binary.
  tmux send-keys -t "$SESSION" "cd \"$ROOT\" && $cmd" Enter
  sleep 2
  tmux capture-pane -t "$SESSION" -p
}

run_t0() {
  echo "=== T0: launch under tmux TTY ==="
  local pane
  pane="$(capture_after_launch "node src/index.js")"
  echo "--- capture ---"
  echo "$pane"
  echo "--- end ---"
  assert_pane_not_nontty "$pane"
  if echo "$pane" | grep -qi "InteractiveMode UI not vendored\|NativeAgentConnection is ready"; then
    echo "ok: T0 adapter entry exercised under tmux session $SESSION (TTY path)"
  else
    fail "T0 expected not-vendored / NativeAgentConnection readiness message"
  fi
}

run_t1() {
  echo "=== T1: idle / composer chrome (adapter smoke until IM vendored) ==="
  local pane
  pane="$(capture_after_launch "node src/index.js --smoke-adapter")"
  echo "--- capture ---"
  echo "$pane"
  echo "--- end ---"
  assert_pane_not_nontty "$pane"
  if echo "$pane" | grep -q "devo-im: tty-ok" \
    && echo "$pane" | grep -q "devo-im: adapter-idle" \
    && echo "$pane" | grep -q "devo-im: composer-ready"; then
    echo "ok: T1 idle/composer markers visible under tmux session $SESSION"
  else
    fail "T1 expected devo-im tty-ok / adapter-idle / composer-ready markers"
  fi
}

run_t2() {
  echo "=== T2: prompt → assistant/ipython cell (adapter smoke) ==="
  local pane
  pane="$(capture_after_launch "node src/index.js --smoke-prompt-cell")"
  echo "--- capture ---"
  echo "$pane"
  echo "--- end ---"
  assert_pane_not_nontty "$pane"
  if echo "$pane" | grep -q "devo-im: streaming-start" \
    && echo "$pane" | grep -q "devo-im: cell-visible" \
    && echo "$pane" | grep -q "devo-im: t2-ok"; then
    echo "ok: T2 prompt/cell markers visible under tmux session $SESSION"
  else
    fail "T2 expected streaming-start / cell-visible / t2-ok markers"
  fi
}

run_t3() {
  echo "=== T3: interrupt restores idle (adapter smoke) ==="
  local pane
  pane="$(capture_after_launch "node src/index.js --smoke-interrupt")"
  echo "--- capture ---"
  echo "$pane"
  echo "--- end ---"
  assert_pane_not_nontty "$pane"
  if echo "$pane" | grep -q "devo-im: interrupt-sent" \
    && echo "$pane" | grep -q "devo-im: idle-restored" \
    && echo "$pane" | grep -q "devo-im: t3-ok"; then
    echo "ok: T3 interrupt/idle markers visible under tmux session $SESSION"
  else
    fail "T3 expected interrupt-sent / idle-restored / t3-ok markers"
  fi
}

skip_until_vendored() {
  local id="$1"
  local title="$2"
  echo "skip: $id ($title) — InteractiveMode UI not vendored yet"
}

run_scenario() {
  case "$1" in
    T0) run_t0 ;;
    T1) run_t1 ;;
    T2) run_t2 ;;
    T3) run_t3 ;;
    T4) skip_until_vendored T4 "Ask / approval" ;;
    T5) skip_until_vendored T5 "/refine" ;;
    T6) skip_until_vendored T6 "queue / steer" ;;
    T7) skip_until_vendored T7 "compact notice" ;;
    T8) skip_until_vendored T8 "agent message / bash-done preview" ;;
    T9) skip_until_vendored T9 "detach / reattach" ;;
    T10) skip_until_vendored T10 "/btw or side-question" ;;
    *)
      echo "unknown scenario: $1 (expected T0–T10 or all)" >&2
      exit 2
      ;;
  esac
}

if [[ "$REQUESTED" == "all" ]]; then
  for id in T0 T1 T2 T3 T4 T5 T6 T7 T8 T9 T10; do
    run_scenario "$id"
  done
else
  run_scenario "$REQUESTED"
fi

cleanup
trap - EXIT
echo "ok: session $SESSION cleaned up"
