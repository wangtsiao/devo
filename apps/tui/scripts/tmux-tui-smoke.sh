#!/usr/bin/env bash
# Minimal InteractiveMode smoke under tmux (Unix).
# Usage: scripts/tmux-tui-smoke.sh [T1|T2|T3|goal|bash|agents|schedule]
set -euo pipefail
SCENARIO="${1:-T1}"
SESSION="${DEVO_TUI_TEST_SESSION:-devo-tui-test}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
REPO="$(cd "$ROOT/../.." && pwd)"
DEVO="${DEVO_BIN:-${DEVO_SERVER_BIN:-$REPO/target/debug/devo}}"
TMUX_BIN="${DEVO_TMUX_BIN:-tmux}"

cleanup() { "$TMUX_BIN" kill-session -t "$SESSION" 2>/dev/null || true; }
trap cleanup EXIT

cleanup
"$TMUX_BIN" new-session -d -s "$SESSION" -c "$REPO" -x 100 -y 30 -- "$DEVO"
sleep 4
echo "=== boot ==="
"$TMUX_BIN" capture-pane -t "$SESSION" -p || true

case "$SCENARIO" in
  T1)
    echo "T1 ok (boot captured)"
    ;;
  T2)
    "$TMUX_BIN" send-keys -t "$SESSION" "Say hello in one short sentence." Enter
    sleep 25
    "$TMUX_BIN" capture-pane -t "$SESSION" -p || true
    echo "T2 ok (prompt sent)"
    ;;
  T3)
    "$TMUX_BIN" send-keys -t "$SESSION" "Write a long reply." Enter
    sleep 3
    "$TMUX_BIN" send-keys -t "$SESSION" Escape
    sleep 2
    echo "T3 ok (interrupt attempted)"
    ;;
  goal)
    "$TMUX_BIN" send-keys -t "$SESSION" "/goal" Enter
    sleep 2
    "$TMUX_BIN" send-keys -t "$SESSION" "/goal smoke objective" Enter
    sleep 3
    "$TMUX_BIN" capture-pane -t "$SESSION" -p || true
    echo "goal ok (slash sent)"
    ;;
  bash)
    "$TMUX_BIN" send-keys -t "$SESSION" "!echo smoke-bash" Enter
    sleep 4
    "$TMUX_BIN" capture-pane -t "$SESSION" -p || true
    echo "bash ok (command sent)"
    ;;
  agents)
    "$TMUX_BIN" send-keys -t "$SESSION" Escape
    sleep 1
    "$TMUX_BIN" send-keys -t "$SESSION" C-o
    sleep 2
    "$TMUX_BIN" capture-pane -t "$SESSION" -p || true
    echo "agents ok (view toggle attempted)"
    ;;
  schedule)
    "$TMUX_BIN" send-keys -t "$SESSION" "/heartbeat" Enter
    sleep 2
    "$TMUX_BIN" capture-pane -t "$SESSION" -p || true
    echo "schedule ok (heartbeat slash sent)"
    ;;
  *)
    echo "boot ok ($SCENARIO)"
    ;;
esac
