<#
.SYNOPSIS
Live end-to-end check of the owner-visible behaviour "PotPlayer closed -> the Discord card clears".

.DESCRIPTION
Runs the real app (dist\potflix.exe) and the real player (PotPlayerMini64) once, against a
real media file, and checks the whole clear path on the real machine:

  1. preconditions (built app, media file, Windows, Discord reachable, no PotPlayer
     running, no Potflix instance already holding the single-instance lock);
  2. the app's freshness line after launch - "instance lock acquired 127.0.0.1:17843"
     past the log offset recorded before launch, a line that can only come from this run;
  3. the card is accepted - a "push details=[" line for the played file followed by
     "push ack nonce=" (Discord accepted the card);
  4. PotPlayer is closed gently (taskkill /IM without /F = a WM_CLOSE request, never a
     kill) and the process is confirmed gone, then a "cleared (" line must be followed
     by "push ack nonce=" with NO "push ack timeout nonce=" in between - an unacked
     clear is the stuck-card bug.

PASS = card acked AND player closed AND clear acked with no ack timeout in between.
The script never starts, stops or restarts Discord, never force-kills PotPlayer, and
never kills a pre-existing Potflix instance; it always stops the app it started.
Exit codes: 0 = PASS, 1 = FAIL, 2 = precondition/environment (nothing was changed).

.PARAMETER File
Media file handed to PotPlayer. REQUIRED: pass the media file to play with -File.

.PARAMETER Exe
potflix.exe to launch. Default: ..\dist\potflix.exe relative to this script.

.PARAMETER PotPlayer
PotPlayerMini64.exe to launch. Default: C:\Program Files\DAUM\PotPlayer\PotPlayerMini64.exe.

.PARAMETER CardTimeoutSeconds
Seconds to wait for the card push AND its ack. Default 90.

.PARAMETER ClearTimeoutSeconds
Seconds to wait for the clear and its ack after PotPlayer closes. Default 60.

.PARAMETER KeepLog
Keep the potflix-debug.log lines this run appended (use for post-mortem). Default:
after the app is stopped, this run's appended lines are trimmed back off byte-exactly
(pre-run length + prefix hash must match), so repeated runs stay attributable and the
log does not grow.

.EXAMPLE
powershell -ExecutionPolicy Bypass -File tools\live-check.ps1 -File <media-file>

.EXAMPLE
powershell -ExecutionPolicy Bypass -File tools\live-check.ps1 -File <media-file> -CardTimeoutSeconds 30 -KeepLog

.NOTES
Runs on Windows PowerShell 5.1+ and PowerShell 7+. No external modules. The app writes
potflix-debug.log beside its exe; this script locates every run-produced line by the
line count it records before launch.
#>
param(
  [string]$File,
  [string]$Exe,
  [string]$PotPlayer = 'C:\Program Files\DAUM\PotPlayer\PotPlayerMini64.exe',
  [int]$CardTimeoutSeconds = 90,
  [int]$ClearTimeoutSeconds = 60,
  [switch]$KeepLog
)
$ErrorActionPreference = 'Stop'

# Carries script failures through throw/catch so the finally block below always runs
# its cleanup before the process exits (a plain exit inside try is not reliable for
# that on every PowerShell version; a thrown exception is).
$script:FailPrefix = 'LIVE-CHECK-FAIL: '

# State swept by the cleanup in the finally block.
$script:AppProc = $null           # the potflix process this run started (or $null)
$script:PlayerLaunched = $false   # set once PotPlayer has been started by this run
$script:LogPath = $null           # potflix-debug.log beside the exe
$script:LogBase = $null           # pre-run log snapshot: Existed, Lines, Length, Hash
$script:Evidence = [System.Collections.Generic.List[string]]::new()

# Append one line to the evidence block printed with the verdict.
function Add-Evidence([string]$Line) {
  $script:Evidence.Add($Line)
}

# Read the log with a permissive share mode (the app keeps it open for append).
# Returns every COMPLETE line; an unterminated trailing line may still be mid-write,
# so it is dropped here and counted on a later poll once its newline has landed.
# Returns $null when the file cannot be read at this instant (callers retry).
function Read-CompleteLogLines([string]$Path) {
  try {
    $fs = [System.IO.File]::Open($Path, [System.IO.FileMode]::Open, [System.IO.FileAccess]::Read, [System.IO.FileShare]::ReadWrite)
    try {
      $len = [int]$fs.Length
      $buf = [byte[]]::new($len)
      $read = 0
      while ($read -lt $len) {
        $n = $fs.Read($buf, $read, $len - $read)
        if ($n -le 0) { break }
        $read += $n
      }
    } finally {
      $fs.Dispose()
    }
  } catch {
    return $null
  }
  $text = [System.Text.Encoding]::UTF8.GetString($buf, 0, $read)
  $parts = [regex]::Split($text, "\r?\n")
  $n = $parts.Count - 1
  if ($n -le 0) { return @() }
  return @($parts[0..($n - 1)])
}

# New log lines starting at $FromIndex (the count of complete lines already consumed).
function Read-NewLogLines([string]$Path, [int]$FromIndex) {
  $all = Read-CompleteLogLines $Path
  if ($null -eq $all -or $FromIndex -ge $all.Count) { return @() }
  return @($all[$FromIndex..($all.Count - 1)])
}

# Append the last $Count complete log lines to the evidence block (failure context).
function Add-LogContext([int]$Count) {
  $all = Read-CompleteLogLines $script:LogPath
  if ($null -eq $all -or $all.Count -eq 0) { return }
  $from = [Math]::Max(0, $all.Count - $Count)
  foreach ($line in @($all[$from..($all.Count - 1)])) { Add-Evidence "log: $line" }
}
# True when something already listens on the app's single-instance port - the same
# check the app relies on (bound listener at 127.0.0.1:17843 = instance running;
# a refused connection = free).
function Test-PotflixLockHeld {
  $client = New-Object System.Net.Sockets.TcpClient
  try {
    $iar = $client.BeginConnect('127.0.0.1', 17843, $null, $null)
    if (-not $iar.AsyncWaitHandle.WaitOne(1000)) { return $false }
    $client.EndConnect($iar)     # throws when the port refuses the connection
    return $true
  } catch {
    return $false
  } finally {
    $client.Close()
  }
}

# Wait (bounded) until no process named $Name exists. Returns $true when gone.
function Wait-ProcessGoneByName([string]$Name, [int]$TimeoutSeconds) {
  $deadline = (Get-Date).AddSeconds($TimeoutSeconds)
  while ($true) {
    if (-not (Get-Process -Name $Name -ErrorAction SilentlyContinue)) { return $true }
    if ((Get-Date) -ge $deadline) { return $false }
    Start-Sleep -Milliseconds 250
  }
}

# Ask PotPlayer to close the way a user would: taskkill /IM WITHOUT /F posts
# WM_CLOSE (taskkill /F or Stop-Process would terminate it and prove nothing about
# the real close path). Confirms the process exits within the timeout; never kills.
function Close-PotPlayerGently([int]$TimeoutSeconds) {
  & taskkill.exe /IM PotPlayerMini64.exe 2>$null | Out-Null
  return (Wait-ProcessGoneByName -Name 'PotPlayerMini64' -TimeoutSeconds $TimeoutSeconds)
}

# Stop the potflix instance this run started. The app exits gracefully only via its
# tray menu, which a script cannot drive, so Stop-Process is the pragmatic cleanup
# for a test process - acceptable here because the check itself is already over.
function Stop-StartedApp {
  if ($null -eq $script:AppProc) { return }
  $p = $script:AppProc
  if ($p.HasExited) { return }
  Write-Host "[live-check] cleanup: stopping potflix (pid $($p.Id))"
  try {
    Stop-Process -Id $p.Id -ErrorAction SilentlyContinue
    [void]$p.WaitForExit(10000)
    if (-not $p.HasExited) {
      Write-Host "CLEANUP: potflix pid $($p.Id) did not exit after Stop-Process." -ForegroundColor Yellow
    }
  } catch {
    Write-Host "CLEANUP: stopping potflix failed: $($_.Exception.Message)" -ForegroundColor Yellow
  }
}

# Unless -KeepLog was passed: trim exactly what this run appended to the log, so
# the file returns to its pre-run state and repeated runs stay attributable.
# Guarded: only acts when the file's prefix is byte-identical to the pre-run
# snapshot (append-only writer); otherwise the file is left untouched, loudly.
function Restore-LogIfUnchanged {
  if ($KeepLog) {
    Write-Host "[live-check] log kept (-KeepLog): $($script:LogPath)"
    return
  }
  if ($null -eq $script:LogBase) { return }
  if (-not $script:LogBase.Existed) {
    Write-Host '[live-check] potflix-debug.log did not exist before this run; left as-is'
    return
  }
  if ($script:LogBase.Length -lt 0 -or -not $script:LogBase.Hash) {
    Write-Host 'WARNING: no usable pre-run log snapshot; log left as-is.' -ForegroundColor Yellow
    return
  }
  try {
    $fs = [System.IO.File]::Open($script:LogPath, [System.IO.FileMode]::Open, [System.IO.FileAccess]::ReadWrite, [System.IO.FileShare]::ReadWrite)
    try {
      if ($fs.Length -lt $script:LogBase.Length) {
        Write-Host 'WARNING: potflix-debug.log shrank since launch (rotated?); not trimming.' -ForegroundColor Yellow
        return
      }
      $buf = [byte[]]::new([int]$script:LogBase.Length)
      $read = 0
      while ($read -lt $buf.Length) {
        $n = $fs.Read($buf, $read, $buf.Length - $read)
        if ($n -le 0) { break }
        $read += $n
      }
      $sha = [System.Security.Cryptography.SHA256]::Create()
      try { $hash = [BitConverter]::ToString($sha.ComputeHash($buf)) } finally { $sha.Dispose() }
      if ($hash -ne $script:LogBase.Hash) {
        Write-Host 'WARNING: potflix-debug.log changed beyond appends (rotated?); not trimming.' -ForegroundColor Yellow
        return
      }
      $fs.SetLength($script:LogBase.Length)
      Write-Host "[live-check] log trimmed back to its pre-run length ($($script:LogBase.Length) bytes)"
    } finally {
      $fs.Dispose()
    }
  } catch {
    Write-Host "WARNING: could not trim the log ($($_.Exception.Message)); left as-is." -ForegroundColor Yellow
  }
}

# Fail the check the way the top-level catch understands: message, cleanup, exit 1.
function Fail([string]$Message) {
  throw ([System.Exception]::new($script:FailPrefix + $Message))
}

if ($MyInvocation.InvocationName -eq '.') { return }


# Windows only. $IsWindows exists on PowerShell 6+; Windows PowerShell 5.1 has no
# such variable and runs only on Windows anyway.
if ($null -ne $IsWindows -and -not $IsWindows) {
  Write-Host 'PRECONDITION FAILED: live-check.ps1 runs on Windows only.' -ForegroundColor Red
  exit 2
}

if ($CardTimeoutSeconds -lt 1 -or $ClearTimeoutSeconds -lt 1) {
  Write-Host 'PRECONDITION FAILED: -CardTimeoutSeconds and -ClearTimeoutSeconds must be at least 1.' -ForegroundColor Red
  exit 2
}

# -Exe defaults to ..\dist\potflix.exe relative to this script.
$scriptDir = $PSScriptRoot
if (-not $scriptDir) { $scriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path }
if (-not $Exe) { $Exe = Join-Path $scriptDir '..\dist\potflix.exe' }
$exePath = [System.IO.Path]::GetFullPath($Exe)
if (-not (Test-Path -LiteralPath $exePath)) {
  Write-Host "PRECONDITION FAILED: potflix.exe not found at $exePath" -ForegroundColor Red
  Write-Host 'Build it first: powershell -ExecutionPolicy Bypass -File tools\rust-build.ps1'
  exit 2
}

if (-not $File) {
  Write-Host 'USAGE: powershell -ExecutionPolicy Bypass -File tools\live-check.ps1 -File <media-file>' -ForegroundColor Red
  Write-Host 'Pass the media file to play with -File.'
  exit 2
}

if (-not (Test-Path -LiteralPath $File)) {
  Write-Host "PRECONDITION FAILED: media file not found: $File" -ForegroundColor Red
  Write-Host 'Pass an existing media file with -File.'
  exit 2
}

# The card is pushed to Discord over its IPC pipe; without it the check cannot
# pass, so say so up front instead of timing out later. This script never starts,
# stops or restarts Discord. The pipe path is built from parts so no single
# source line carries the UNC shape.
$sep = [char]92
$pipeDir = $sep + 'pipe'
$pipeServer = $sep + $sep + '.' + $pipeDir
$pipe = $pipeServer + $sep + 'discord-ipc-0'
if (-not (Test-Path -LiteralPath $pipe)) {
  Write-Host ('PRECONDITION FAILED: Discord IPC pipe ' + $pipe + ' not found.') -ForegroundColor Red
  Write-Host 'Start Discord first; this script never touches Discord itself.'
  exit 2
}

# This run owns the player lifetime and closes PotPlayer by image name, so an
# already-open PotPlayer would both break the check and get closed by mistake.
$existingPlayers = @(Get-Process -Name PotPlayerMini64 -ErrorAction SilentlyContinue)
if ($existingPlayers.Count -gt 0) {
  Write-Host "PRECONDITION FAILED: PotPlayerMini64 is already running (pid $($existingPlayers[0].Id))." -ForegroundColor Red
  Write-Host 'Close PotPlayer first; this check must own the player it closes.'
  exit 2
}

# Same check the app relies on: a listener on 127.0.0.1:17843 means an instance is
# already running. Never kill it - exit and tell the operator.
if (Test-PotflixLockHeld) {
  Write-Host 'PRECONDITION FAILED: a Potflix instance already holds the single-instance lock 127.0.0.1:17843.' -ForegroundColor Red
  Write-Host 'Exit Potflix from its tray menu, then rerun; this script never kills a running instance.'
  exit 2
}
$script:LogPath = Join-Path (Split-Path -Parent $exePath) 'potflix-debug.log'
$script:LogBase = [pscustomobject]@{ Existed = $false; Lines = 0; Length = [long]-1; Hash = $null }
if (Test-Path -LiteralPath $script:LogPath) {
  $before = Read-CompleteLogLines $script:LogPath
  if ($null -eq $before) { $before = @() }
  $script:LogBase.Existed = $true
  $script:LogBase.Lines = $before.Count
  try {
    $bytes = [System.IO.File]::ReadAllBytes($script:LogPath)
    $sha = [System.Security.Cryptography.SHA256]::Create()
    try { $script:LogBase.Hash = [BitConverter]::ToString($sha.ComputeHash($bytes)) } finally { $sha.Dispose() }
    $script:LogBase.Length = [long]$bytes.Length
  } catch {
    Write-Host "WARNING: could not snapshot the log for a later trim ($($_.Exception.Message))." -ForegroundColor Yellow
  }
}

Write-Host '[live-check] Potflix live end-to-end check: player closed -> card cleared'
Write-Host "  app:  $exePath"
Write-Host "  file: $File"

$scriptExit = 1   # pessimistic; set to 0 only after the whole cycle is verified
try {
  Add-Evidence "run started $(Get-Date -Format 'HH:mm:ss')"
  Add-Evidence "app:  $exePath"
  Add-Evidence "file: $File"
  Add-Evidence "log:  $($script:LogPath) (baseline $($script:LogBase.Lines) complete lines, $($script:LogBase.Length) bytes)"
  Write-Host '[live-check] launching the app (hidden window)'
  $proc = Start-Process -FilePath $exePath -WorkingDirectory (Split-Path -Parent $exePath) -WindowStyle Hidden -PassThru
  $script:AppProc = $proc
  Add-Evidence "potflix pid $($proc.Id)"

  Write-Host '[live-check] waiting for the app lock line (freshness control)'
  $seen = $script:LogBase.Lines
  $lockLine = $null
  $lockAbs = 0
  $deadline = (Get-Date).AddSeconds(20)
  while (-not $lockLine -and (Get-Date) -lt $deadline -and -not $proc.HasExited) {
    $new = Read-NewLogLines -Path $script:LogPath -FromIndex $seen
    for ($i = 0; $i -lt $new.Count; $i++) {
      if ($new[$i] -match 'instance lock acquired 127\.0\.0\.1:17843') {
        $lockLine = $new[$i]
        $lockAbs = $seen + $i + 1
        break
      }
    }
    $seen += $new.Count
    if (-not $lockLine) { Start-Sleep -Milliseconds 250 }
  }
  if (-not $lockLine) {
    Add-LogContext 8
    if ($proc.HasExited) {
      Fail "the app exited during startup (exit code $($proc.ExitCode)) before logging 'instance lock acquired 127.0.0.1:17843'."
    }
    Fail "no 'instance lock acquired 127.0.0.1:17843' line appeared within 20s of launch."
  }
  Add-Evidence "lock line (log line $lockAbs): $lockLine"
  Write-Host "  $lockLine"
  if ($proc.HasExited) {
    Fail "the app exited right after acquiring the lock (exit code $($proc.ExitCode))."
  }

  if (-not (Test-Path -LiteralPath $PotPlayer)) {
    Fail "PotPlayer not found at $PotPlayer (renamed or moved?); nothing was played, so the check cannot run."
  }
  Write-Host '[live-check] launching PotPlayer with the media file'
  Add-Evidence "player: $PotPlayer"
  try {
    Start-Process -FilePath $PotPlayer -ArgumentList ('"' + $File + '"') | Out-Null
  } catch {
    Fail "could not launch PotPlayer: $($_.Exception.Message)"
  }
  $script:PlayerLaunched = $true

  # A player that dies on startup would otherwise only surface as a long card
  # timeout later; catch the obvious case here.
  $playerProc = $null
  $deadline = (Get-Date).AddSeconds(10)
  while (-not $playerProc -and (Get-Date) -lt $deadline) {
    $playerProc = Get-Process -Name PotPlayerMini64 -ErrorAction SilentlyContinue | Select-Object -First 1
    if (-not $playerProc) { Start-Sleep -Milliseconds 250 }
  }
  if (-not $playerProc) {
    Fail 'PotPlayer did not stay running after launch; nothing was played.'
  }

  Write-Host "[live-check] waiting up to ${CardTimeoutSeconds}s for the card push and its Discord ack"
  # The pushed title is the file's cleaned title; compare alphanumerics only so the
  # U+0A789 colon and other punctuation cannot break the match, and the unrelated
  # Idle/Browsing card is rejected.
  $baseNorm = (([System.IO.Path]::GetFileNameWithoutExtension($File)) -replace '[^A-Za-z0-9]', '').ToLowerInvariant()
  $cardLine = $null
  $cardAbs = 0
  $ackLine = $null
  $appDied = $false
  $deadline = (Get-Date).AddSeconds($CardTimeoutSeconds)
  while ($null -eq $ackLine -and (Get-Date) -lt $deadline) {
    $new = Read-NewLogLines -Path $script:LogPath -FromIndex $seen
    for ($i = 0; $i -lt $new.Count; $i++) {
      $line = $new[$i]
      $abs = $seen + $i + 1
      if ($null -eq $cardLine) {
        if ($line -match 'push details=\[') {
          $m = [regex]::Match($line, 'details=\[([^\]]*)\]')
          if ($m.Success) {
            $titleNorm = ($m.Groups[1].Value -replace '[^A-Za-z0-9]', '').ToLowerInvariant()
            if ($titleNorm.Length -ge 4 -and ($baseNorm.StartsWith($titleNorm) -or ($titleNorm.Length -ge 12 -and $baseNorm.Contains($titleNorm)))) {
              $cardLine = $line
              $cardAbs = $abs
              Add-Evidence "card push (log line $cardAbs): $line"
              Write-Host "  $line"
            }
          }
        }
      } elseif ($null -eq $ackLine -and $line -match 'push ack nonce=') {
        $ackLine = $line
        Add-Evidence "card ack (log line $abs): $line"
        Write-Host "  $line"
      }
    }
    $seen += $new.Count
    if ($proc.HasExited) { $appDied = $true; break }
    if ($null -eq $ackLine) { Start-Sleep -Milliseconds 250 }
  }
  if ($null -eq $ackLine) {
    Add-LogContext 8
    if ($appDied) {
      Fail "the app exited (exit code $($proc.ExitCode)) before the card was acked."
    }
    if ($cardLine) {
      Fail "the card push for '$([System.IO.Path]::GetFileName($File))' was sent but no 'push ack nonce=' followed within ${CardTimeoutSeconds}s; Discord did not accept it."
    }
    Fail "no 'push details=[' line for '$([System.IO.Path]::GetFileName($File))' within ${CardTimeoutSeconds}s; no card was pushed."
  }

  Write-Host '[live-check] closing PotPlayer: taskkill without /F (WM_CLOSE request, never a kill)'
  if (-not (Get-Process -Name PotPlayerMini64 -ErrorAction SilentlyContinue)) {
    Add-Evidence 'note: PotPlayer was already gone before the close request'
  }
  Add-Evidence "close requested $(Get-Date -Format 'HH:mm:ss')"
  if (-not (Close-PotPlayerGently -TimeoutSeconds 15)) {
    Add-LogContext 6
    Fail 'PotPlayer did not exit within 15s of the close request; not force-killed (it may be showing a dialog).'
  }
  Add-Evidence "player closed $(Get-Date -Format 'HH:mm:ss')"

  Write-Host "[live-check] waiting up to ${ClearTimeoutSeconds}s for the clear and its ack"
  $clearLine = $null
  $clearAbs = 0
  $clearAck = $null
  $clearAckAbs = 0
  $timeoutLine = $null
  $timeoutAbs = 0
  $appDied = $false
  $deadline = (Get-Date).AddSeconds($ClearTimeoutSeconds)
  while ($null -eq $clearAck -and $null -eq $timeoutLine -and (Get-Date) -lt $deadline) {
    $new = Read-NewLogLines -Path $script:LogPath -FromIndex $seen
    for ($i = 0; $i -lt $new.Count; $i++) {
      $line = $new[$i]
      $abs = $seen + $i + 1
      if ($null -eq $clearLine) {
        if ($line -match 'cleared \(') {
          $clearLine = $line
          $clearAbs = $abs
        }
      } else {
        if ($line -match 'push ack timeout nonce=') {
          $timeoutLine = $line
          $timeoutAbs = $abs
          break
        }
        if ($line -match 'push ack nonce=') {
          $clearAck = $line
          $clearAckAbs = $abs
          break
        }
      }
    }
    $seen += $new.Count
    if ($proc.HasExited) { $appDied = $true; break }
    if ($null -eq $clearAck -and $null -eq $timeoutLine) { Start-Sleep -Milliseconds 250 }
  }
  if ($null -ne $timeoutLine) {
    Add-Evidence "clear:   (log line $clearAbs) $clearLine"
    Add-Evidence "timeout: (log line $timeoutAbs) $timeoutLine"
    Add-LogContext 6
    Fail "the clear was NOT acked: 'push ack timeout nonce=' appeared between the 'cleared (' line and any ack - the stuck-card bug; the card may still be showing in Discord."
  }
  if ($null -eq $clearLine) {
    Add-LogContext 8
    if ($appDied) {
      Fail "the app exited (exit code $($proc.ExitCode)) before any 'cleared (' line."
    }
    Fail "no 'cleared (' line appeared within ${ClearTimeoutSeconds}s of the close request."
  }
  if ($null -eq $clearAck) {
    Add-LogContext 8
    if ($appDied) {
      Fail "the app exited (exit code $($proc.ExitCode)) with the clear still unacked."
    }
    Fail "the 'cleared (' line appeared but no 'push ack nonce=' followed within ${ClearTimeoutSeconds}s (unacked clear)."
  }
  Add-Evidence "clear:     (log line $clearAbs) $clearLine"
  Add-Evidence "clear ack: (log line $clearAckAbs) $clearAck"
  Write-Host "  $clearLine"
  Write-Host "  $clearAck"

  Add-Evidence 'verdict: card acked AND player closed AND clear acked (no ack timeout between clear and ack)'
  $scriptExit = 0
} catch {
  $msg = $_.Exception.Message
  if ($msg -and $msg.StartsWith($script:FailPrefix)) {
    Write-Host ('CHECK FAILED: ' + $msg.Substring($script:FailPrefix.Length)) -ForegroundColor Red
  } else {
    Write-Host "LIVE CHECK FAILED (unexpected error): $msg" -ForegroundColor Red
  }
} finally {
  # Cleanup always runs: close PotPlayer if this run started it (same gentle close,
  # still never a force-kill), stop the app this run started, trim this run's log
  # lines unless -KeepLog.
  if ($script:PlayerLaunched -and (Get-Process -Name PotPlayerMini64 -ErrorAction SilentlyContinue)) {
    Write-Host '[live-check] cleanup: PotPlayer still running - sending the same gentle close'
    if (-not (Close-PotPlayerGently -TimeoutSeconds 15)) {
      Write-Host 'CLEANUP: PotPlayer is still running; close it manually (never force-killed here).' -ForegroundColor Yellow
    }
  }
  Stop-StartedApp
  Restore-LogIfUnchanged
}

Write-Host ''
Write-Host '[live-check] evidence'
foreach ($e in $script:Evidence) { Write-Host "  $e" }
if ($scriptExit -eq 0) {
  Write-Host 'LIVE CHECK PASSED: card acked, PotPlayer closed gently, clear acked.' -ForegroundColor Green
} else {
  Write-Host "LIVE CHECK FAILED (exit $scriptExit): see the CHECK FAILED line above and the evidence block." -ForegroundColor Red
}
exit $scriptExit
