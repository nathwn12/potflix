# check.ps1 - one-command local verification gate for Potflix.
#
# Runs clippy and the release test suite from `rust\`, verifies the built
# title-cleaner against the parity oracle (rust\parity-golden.sha256), and with
# the flags also rebuilds the app and runs the headless self-test. Fails fast:
# the first failing step stops the run with a non-zero exit code, so it is safe
# to use as a release gate.
#
#   powershell -ExecutionPolicy Bypass -File tools\check.ps1
#   powershell -ExecutionPolicy Bypass -File tools\check.ps1 -Build -Selftest
param(
  [switch]$Build,
  [switch]$Selftest
)
$ErrorActionPreference = 'Stop'

# Push-Location/Set-Location expand '[', ']', '*' and '?' as wildcards, so a
# checkout path containing them aborts with PathNotFound. Escape every path
# handed to a wildcard-aware command. (Literal-path parameters do not need it.)
function Get-Literal([string]$Path) {
  return [WildcardPattern]::Escape($Path)
}

# The title-cleaner parity corpora, in oracle order. Only the corpora live here;
# the expected hashes come from rust\parity-golden.sha256, so the oracle stays
# the single source of truth for the contract.
$parityCorpora = @('tools\titles-sample4.txt', 'tools\titles-sample.txt')

# Compare one built output against its expected hash. Returns $true on a match;
# on a mismatch it prints both hashes so the caller's failure carries them. It
# is a function so the comparator itself can be exercised in isolation without
# running the gate: dot-source this script, then call Test-ParityHash.
function Test-ParityHash([string]$Expected, [string]$ActualPath) {
  if (-not (Test-Path -LiteralPath $ActualPath)) {
    Write-Host "  no output produced at $ActualPath"
    return $false
  }
  $actual = (Get-FileHash -LiteralPath $ActualPath -Algorithm SHA256).Hash
  if ($actual -eq $Expected.ToUpperInvariant()) {
    Write-Host "  match $actual"
    return $true
  }
  Write-Host "  expected $($Expected.ToUpperInvariant())"
  Write-Host "  actual   $actual ($ActualPath)"
  return $false
}

# Precondition for the self-test step. A missing binary is a real failure
# (exit 1), not a missing toolchain (exit 2).
function Assert-SelftestExe([string]$Exe) {
  if (-not (Test-Path -LiteralPath $Exe)) {
    Write-Host "CHECK FAILED: selftest ($Exe not found - run with -Build to produce it)" -ForegroundColor Red
    exit 1
  }
}

# The title-cleaner parity step: build the binary, run it over both corpora, and
# require the frozen oracle's hashes. The oracle holds hashes only (the
# corpora are gitignored because they carry real media filenames), so a missing
# oracle, a missing corpus, or an oracle line without a hash FAILS - none of
# them is ever treated as "nothing to compare". It is a function so the guards
# and the comparison can be exercised in isolation: dot-source this script,
# then call Invoke-TitlecleanParity.
function Invoke-TitlecleanParity([string]$Root, [string]$RustDir) {
  Write-Host '[check] titleclean parity'

  $oracle = Join-Path $RustDir 'parity-golden.sha256'
  if (-not (Test-Path -LiteralPath $oracle)) {
    Write-Host "CHECK FAILED: titleclean parity ($oracle not found)" -ForegroundColor Red
    exit 1
  }
  $expected = @{}
  foreach ($line in [IO.File]::ReadAllLines($oracle)) {
    $t = $line.Trim()
    if ($t -eq '' -or $t.StartsWith('#')) { continue }
    if ($t -notmatch '^([0-9A-Fa-f]{64})\s+(\S.*)$') {
      Write-Host "CHECK FAILED: titleclean parity ($oracle malformed data line: $t)" -ForegroundColor Red
      exit 1
    }
    $expected[$Matches[2].Trim().Replace('\', '/')] = $Matches[1].ToUpperInvariant()
  }
  if ($expected.Count -eq 0) {
    Write-Host "CHECK FAILED: titleclean parity ($oracle has no data lines)" -ForegroundColor Red
    exit 1
  }
  foreach ($corpus in $parityCorpora) {
    if (-not (Test-Path -LiteralPath (Join-Path $Root $corpus))) {
      Write-Host "CHECK FAILED: titleclean parity ($corpus not found)" -ForegroundColor Red
      exit 1
    }
    if (-not $expected.ContainsKey($corpus.Replace('\', '/'))) {
      Write-Host "CHECK FAILED: titleclean parity ($oracle has no hash for $corpus)" -ForegroundColor Red
      exit 1
    }
  }

  $parityDir = Join-Path $Root 'build\parity'
  if (-not (Test-Path -LiteralPath $parityDir -PathType Container)) {
    New-Item -ItemType Directory -Force -Path (Get-Literal $parityDir) | Out-Null
  }

  # Build the parity binary first, so a compile failure is reported as such.
  Push-Location (Get-Literal $RustDir)
  try {
    cargo build --release --locked -p titleclean --bin titleclean
    $buildCode = $LASTEXITCODE
  } finally {
    Pop-Location
  }
  if ($buildCode -ne 0) {
    Write-Host "CHECK FAILED: titleclean parity (cargo build exited $buildCode)" -ForegroundColor Red
    exit 1
  }
  $bin = Join-Path $RustDir 'target\release\titleclean.exe'

  # Run from the repo root with repo-root-relative paths, exactly like the
  # commands in the oracle header, so the binary sees the same arguments.
  Push-Location (Get-Literal $Root)
  try {
    $mismatch = ''
    foreach ($corpus in $parityCorpora) {
      $out = Join-Path $parityDir ('actual-' + [IO.Path]::GetFileNameWithoutExtension($corpus) + '.tsv')
      Write-Host "  titleclean -corpus $corpus -out build\parity\$([IO.Path]::GetFileName($out))"
      & $bin -corpus $corpus -out $out
      if ($LASTEXITCODE -ne 0) {
        Write-Host "CHECK FAILED: titleclean parity (titleclean exited $LASTEXITCODE for $corpus)" -ForegroundColor Red
        exit 1
      }
      if (-not (Test-ParityHash $expected[$corpus.Replace('\', '/')] $out)) {
        if ($mismatch -eq '') { $mismatch = $corpus }
      }
    }
  } finally {
    Pop-Location
  }
  if ($mismatch -ne '') {
    Write-Host "CHECK FAILED: titleclean parity (hash mismatch for $mismatch - expected and actual above)" -ForegroundColor Red
    exit 1
  }
}

# The steps below run when this file is invoked, never when it is dot-sourced:
# dot-sourcing is how the comparators and guards above are exercised in isolation.
if ($MyInvocation.InvocationName -eq '.') { return }

# Resolve the repo root from this script's own path (it lives in tools\), so the
# gate behaves identically from any current directory. Same as tools\rust-build.ps1.
$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
$rustDir = Join-Path $root 'rust'

# Preflight: every step below needs `cargo` on PATH. Exit with a distinct code so
# a missing toolchain is never mistaken for a failing check.
if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  Write-Host 'ERROR: cargo not found on PATH (Potflix builds with a Rust toolchain).' -ForegroundColor Red
  exit 2
}

# rust\crates\app\build.rs embeds assets\icon.ico into the exe; fail here if it
# ever goes missing rather than at build time.
Write-Host '[check] icon'
$icon = Join-Path $root 'assets\icon.ico'
if (-not (Test-Path -LiteralPath $icon)) {
  Write-Host "CHECK FAILED: icon ($icon not found)" -ForegroundColor Red
  exit 1
}

Push-Location (Get-Literal $rustDir)
try {
  Write-Host '[check] clippy'
  cargo clippy --workspace --all-targets -- -D warnings
  if ($LASTEXITCODE -ne 0) {
    Write-Host "CHECK FAILED: clippy (exit $LASTEXITCODE)" -ForegroundColor Red
    exit 1
  }

  Write-Host '[check] test'
  & (Join-Path $root 'tools\rust-test.ps1') -MaxJobs 8
  if ($LASTEXITCODE -ne 0) {
    Write-Host "CHECK FAILED: test (exit $LASTEXITCODE)" -ForegroundColor Red
    exit 1
  }
} finally {
  Pop-Location
}

Invoke-TitlecleanParity -Root $root -RustDir $rustDir

if ($Build) {
  Write-Host '[check] build'
  try {
    # A stale $LASTEXITCODE from an earlier native command must not decide this.
    $LASTEXITCODE = 0
    & (Join-Path $root 'tools\rust-build.ps1')
  } catch {
    # A thrown cmdlet error leaves $LASTEXITCODE stale, so report the exception.
    Write-Host 'CHECK FAILED: build' -ForegroundColor Red
    Write-Host $_.Exception.Message
    exit 1
  }
  if ($LASTEXITCODE -ne 0) {
    Write-Host "CHECK FAILED: build (exit $LASTEXITCODE)" -ForegroundColor Red
    exit 1
  }
}

if ($Selftest) {
  $exe = Join-Path $root 'dist\potflix.exe'
  Write-Host '[check] selftest'
  Assert-SelftestExe $exe

  # Without -Build the exe may predate the current source; warn, never fail.
  if (-not $Build) {
    $exeTime = (Get-Item -LiteralPath $exe).LastWriteTimeUtc
    $newest = Get-ChildItem -LiteralPath $rustDir -Recurse -Filter *.rs -File |
      Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
    if ($newest -and $newest.LastWriteTimeUtc -gt $exeTime) {
      [Console]::Error.WriteLine("WARNING: $exe is older than $($newest.FullName); run with -Build to self-test the current source.")
    }
  }

  # The app is built -H=windowsgui and, before it can run the self-test, takes
  # the single-instance lock: a zero exit code alone cannot tell a real pass from
  # a no-op. Capture both streams and require the success marker
  # `M1 SELFTEST PASS`.
  #
  # The passing self-test deliberately spawns a second instance to prove the
  # single-instance lock and echoes its message `Potflix is already running.`,
  # so that text is normal here and is never a failure on its own.
  #
  # Use a raw System.Diagnostics.Process, not Start-Process: under Windows
  # PowerShell 5.1 Start-Process with -RedirectStandard* leaves $proc.ExitCode
  # $null, and its pipe handles are not drained concurrently.
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $exe
  $psi.Arguments = '--selftest'
  $psi.WorkingDirectory = $root
  $psi.UseShellExecute = $false
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true

  $proc = New-Object System.Diagnostics.Process
  $proc.StartInfo = $psi
  [void]$proc.Start()
  # Start draining both pipes BEFORE waiting; otherwise a child that fills a
  # pipe buffer blocks forever while we block in WaitForExit.
  $outTask = $proc.StandardOutput.ReadToEndAsync()
  $errTask = $proc.StandardError.ReadToEndAsync()
  if (-not $proc.WaitForExit(120000)) {
    try { $proc.Kill() } catch { }
    Write-Host 'CHECK FAILED: selftest (timeout)' -ForegroundColor Red
    exit 1
  }
  $code = $proc.ExitCode
  $outText = $outTask.Result
  $errText = $errTask.Result
  $tail = (($outText + "`n" + $errText) -split "`r?`n" | Where-Object { $_ -ne '' } | Select-Object -Last 15) -join "`n"

  if ($code -ne 0) {
    Write-Host "CHECK FAILED: selftest (exit $code)" -ForegroundColor Red
    if ($tail) { Write-Host $tail }
    exit 1
  }
  if ($outText -notmatch 'M1 SELFTEST PASS') {
    Write-Host "CHECK FAILED: selftest (missing 'M1 SELFTEST PASS')" -ForegroundColor Red
    if ($outText -match 'Potflix is already running\.' -or $errText -match 'Potflix is already running\.') {
      Write-Host 'a Potflix instance already holds 127.0.0.1:17843 - exit it and rerun.' -ForegroundColor Yellow
    }
    if ($tail) { Write-Host $tail }
    exit 1
  }
}

Write-Host 'CHECK PASSED' -ForegroundColor Green
exit 0
