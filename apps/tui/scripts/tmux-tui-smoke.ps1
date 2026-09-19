# Minimal InteractiveMode smoke under psmux/tmux (Windows).
# Usage: powershell -File scripts/tmux-tui-smoke.ps1 [T1|T2|T3|goal|bash|agents|schedule]
# Markers only — do not assert exact model reply text.
param(
  [string]$Scenario = "T1"
)

$ErrorActionPreference = "Stop"
$Session = if ($env:DEVO_TUI_TEST_SESSION) { $env:DEVO_TUI_TEST_SESSION } else { "devo-tui-test" }
$tmux = if ($env:DEVO_TMUX_BIN) { $env:DEVO_TMUX_BIN } else { "psmux" }
$Root = Split-Path -Parent $PSScriptRoot
$Repo = Split-Path -Parent (Split-Path -Parent $Root)
$Devo = Join-Path $Repo "target\debug\devo.exe"
if (-not (Test-Path $Devo)) {
  Write-Error "Build devo first: cargo build -p devo-cli"
}

function Cleanup {
  & $tmux kill-session -t $Session 2>$null
}
trap { Cleanup }

# Prefer -- before the binary (psmux requires it for a custom command).
Cleanup
& $tmux new-session -d -s $Session -x 100 -y 30 -- $Devo
Start-Sleep -Seconds 4
$pane = & $tmux capture-pane -t $Session -p 2>$null
if (-not $pane) {
  # Fallback: some psmux builds list the session but fail capture briefly.
  Start-Sleep -Seconds 1
  $pane = & $tmux capture-pane -t $Session -p 2>$null
}
Write-Host "=== boot ==="
Write-Host $pane

switch ($Scenario) {
  "T1" {
    if ($pane -notmatch "devo|prime|agent|/" ) {
      Write-Warning "T1: no clear idle chrome yet (fullscreen may hide boot text)"
    }
    Write-Host "T1 ok (boot captured)"
  }
  "T2" {
    & $tmux send-keys -t $Session "Say hello in one short sentence." Enter
    Start-Sleep -Seconds 25
    $pane2 = & $tmux capture-pane -t $Session -p
    Write-Host $pane2
    Write-Host "T2 ok (prompt sent)"
  }
  "T3" {
    & $tmux send-keys -t $Session "Write a long reply." Enter
    Start-Sleep -Seconds 3
    & $tmux send-keys -t $Session Escape
    Start-Sleep -Seconds 2
    Write-Host "T3 ok (interrupt attempted)"
  }
  "goal" {
    & $tmux send-keys -t $Session "/goal" Enter
    Start-Sleep -Seconds 2
    & $tmux send-keys -t $Session "/goal smoke objective" Enter
    Start-Sleep -Seconds 3
    $paneG = & $tmux capture-pane -t $Session -p
    Write-Host $paneG
    Write-Host "goal ok (slash sent)"
  }
  "bash" {
    & $tmux send-keys -t $Session "!echo smoke-bash" Enter
    Start-Sleep -Seconds 4
    $paneB = & $tmux capture-pane -t $Session -p
    Write-Host $paneB
    Write-Host "bash ok (command sent)"
  }
  "agents" {
    & $tmux send-keys -t $Session Escape
    Start-Sleep -Seconds 1
    & $tmux send-keys -t $Session C-o
    Start-Sleep -Seconds 2
    $paneA = & $tmux capture-pane -t $Session -p
    Write-Host $paneA
    Write-Host "agents ok (view toggle attempted)"
  }
  "schedule" {
    & $tmux send-keys -t $Session "/heartbeat" Enter
    Start-Sleep -Seconds 2
    $paneS = & $tmux capture-pane -t $Session -p
    Write-Host $paneS
    Write-Host "schedule ok (heartbeat slash sent)"
  }
  default {
    Write-Host "Unknown scenario $Scenario — ran boot only"
  }
}

Cleanup
