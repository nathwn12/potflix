# rust-test.ps1 - build the workspace test binaries once, run them concurrently.
#
# potflix-win window tests share process-global UI state and race when the
# machine hosts other test binaries (observed once under heavy load:
# potflix_win contributed 0 of its 288 tests, run reported
# `total passed=706, failed=1`). Scheduling fact: potflix_win never
# overlaps the parallel batch -- every other binary runs first under the
# -MaxJobs cap, then potflix_win runs alone with `--test-threads=1`.
#
# Doc-test guard: `cargo test --no-run` builds unit/integration binaries only,
# so doctests in `/// ``` ` or `//! ``` ` fences would silently never run. Before launching
# the binaries this script scans `rust\crates\**\*.rs` for a fence rustdoc could
# collect (bare, rust, or doctest attributes on a `///` or `//!` line, with open/close
# pairing so a bare closer never counts); fences tagged text/json/console-style never
# yield doctests, so when no collectible fence exists it skips silently, otherwise it runs
# `cargo test --workspace --release --locked --doc` from `rust\` and folds its
# pass count and failure into the result below.
#
#   powershell -ExecutionPolicy Bypass -File tools\rust-test.ps1
#   powershell -ExecutionPolicy Bypass -File tools\rust-test.ps1 -MaxJobs 8
#
# Exit codes: 0 = all binaries passed, 1 = a binary failed, 2 = no toolchain.
param(
  [int]$MaxJobs = 0
)
$ErrorActionPreference = 'Stop'
$scriptSw = [System.Diagnostics.Stopwatch]::StartNew()

$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
$rustDir = Join-Path $root 'rust'

if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) {
  Write-Host 'ERROR: cargo not found on PATH (Potflix builds with a Rust toolchain).' -ForegroundColor Red
  exit 2
}

Write-Host '[rust-test] build binaries'
$raw = @()
$buildCode = 0
$oldEap = $ErrorActionPreference
$ErrorActionPreference = 'Continue'
Push-Location -LiteralPath $rustDir
try {
  $raw = cargo test --workspace --release --locked --no-run --message-format=json 2>&1 | ForEach-Object { "$_" }
  $buildCode = $LASTEXITCODE
} finally {
  Pop-Location
  $ErrorActionPreference = $oldEap
}
if ($buildCode -ne 0) {
  Write-Host "RUST-TEST FAILED: build (cargo test --no-run exited $buildCode)" -ForegroundColor Red
  foreach ($line in $raw) {
    if ($line -notmatch '^\s*\{') { Write-Host $line }
  }
  exit 1
}

$binaries = @()
foreach ($line in $raw) {
  # Fast prefilter: only a compiler-artifact line can survive the checks below;
  # string containment is far cheaper than a regex plus a JSON parse per line.
  if (-not ($line.Contains('compiler-artifact'))) { continue }
  if ($line -notmatch '^\s*\{') { continue }
  $obj = $null
  try { $obj = ConvertFrom-Json $line } catch { continue }
  if ($obj.reason -ne 'compiler-artifact') { continue }
  if (-not $obj.target) { continue }
  if (-not $obj.target.test) { continue }
  if (-not $obj.executable) { continue }
  $exe = "$($obj.executable)"
  if ([string]::IsNullOrWhiteSpace($exe)) { continue }
  $manifest = "$($obj.manifest_path)"
  $wd = $rustDir
  if (-not [string]::IsNullOrWhiteSpace($manifest)) {
    $wd = Split-Path $manifest -Parent
  }
  $binaries += New-Object PSObject -Property @{ Exe = $exe; WorkDir = $wd }
}

$seen = @{}
$unique = @()
foreach ($b in $binaries) {
  $key = $b.Exe.ToLowerInvariant()
  if ($seen.ContainsKey($key)) { continue }
  $seen[$key] = $true
  $unique += $b
}
$binaries = $unique

if ($binaries.Count -eq 0) {
  Write-Host 'RUST-TEST FAILED: no test binaries found' -ForegroundColor Red
  exit 1
}

Write-Host ("[rust-test] run {0} binaries (MaxJobs={1})" -f $binaries.Count, $MaxJobs)

$parallel = @($binaries | Where-Object { (Split-Path $_.Exe -Leaf) -notlike 'potflix_win-*' })
$serial = @($binaries | Where-Object { (Split-Path $_.Exe -Leaf) -like 'potflix_win-*' })
Write-Host ("[rust-test] parallel: {0} binaries (MaxJobs={1})" -f $parallel.Count, $MaxJobs)
Write-Host ("[rust-test] serial: {0} binaries (--test-threads=1, after parallel)" -f $serial.Count)

$docPassed = 0
$docFailed = $false
$hasDocFence = $false
$cratesDir = Join-Path $rustDir 'crates'
if (Test-Path -LiteralPath $cratesDir) {
  # Rust-doctest trigger: only a fence rustdoc could collect (bare ```, rust, or
  # doctest attributes) starts the workspace-wide `cargo test --doc` run below.
  # ```text / ```json illustration fences (and their bare closers) never yield
  # doctests, so they must not pay for that run. Fail-open: an unreadable file is
  # skipped, any other scan error or any unknown tag triggers the run, and both
  # the /// and //! markers still count.
  $nonRustTags = New-Object System.Collections.Generic.HashSet[string](
    [string[]]@('text', 'plaintext', 'plain', 'console', 'terminal', 'shell', 'sh', 'bash', 'zsh', 'fish', 'powershell', 'pwsh', 'cmd', 'batch', 'bat', 'json', 'jsonc', 'yaml', 'yml', 'toml', 'ini', 'xml', 'html', 'css', 'markdown', 'md', 'diff', 'sql', 'log', 'output', 'none'),
    [System.StringComparer]::OrdinalIgnoreCase)
  $rxFence = New-Object System.Text.RegularExpressions.Regex('(?m)^[ \t]*(///|//!)[ \t]*```([^\r\n]*)', [System.Text.RegularExpressions.RegexOptions]::Compiled)
  try {
    # Native prefilter: only files containing a ``` fence need the precise
    # (paired, tag-aware) scan below; proving absence still reads every byte
    # once, but in findstr instead of the script interpreter.
    Push-Location -LiteralPath $cratesDir
    try {
      $fenceFiles = @(findstr /S /M '/C:```' '*.rs' 2>$null)
      $fenceCode = $LASTEXITCODE
    } finally {
      Pop-Location
    }
    if (($fenceCode -ne 0) -and ($fenceCode -ne 1)) { $hasDocFence = $true }
    foreach ($rel in $fenceFiles) {
      $f = Join-Path $cratesDir "$rel"
      try {
        $src = [System.IO.File]::ReadAllText($f)
      } catch {
        continue
      }
      $insideFence = $false
      foreach ($hit in $rxFence.Matches($src)) {
        if ($insideFence) { $insideFence = $false; continue }
        $insideFence = $true
        $info = "$($hit.Groups[2].Value)".Trim()
        if ($info -eq '') { $hasDocFence = $true; break }
        $collectible = $true
        foreach ($tok in @($info -split '[,\s]+')) {
          if ($nonRustTags.Contains($tok)) { $collectible = $false; break }
        }
        if ($collectible) { $hasDocFence = $true; break }
      }
      if ($hasDocFence) { break }
    }
  } catch {
    $hasDocFence = $true
  }
}
if ($hasDocFence) {
  Write-Host '[rust-test] run doc tests'
  $docRaw = @()
  $docCode = 0
  $oldEap = $ErrorActionPreference
  $ErrorActionPreference = 'Continue'
  Push-Location -LiteralPath $rustDir
  try {
    $docRaw = cargo test --workspace --release --locked --doc 2>&1 | ForEach-Object { "$_" }
    $docCode = $LASTEXITCODE
  } finally {
    Pop-Location
    $ErrorActionPreference = $oldEap
  }
  $docOut = ($docRaw -join "`n")
  $m = [regex]::Matches($docOut, '(?m)^test result:\s+ok\.\s+(\d+)\s+passed')
  foreach ($hit in $m) {
    $docPassed += [int]$hit.Groups[1].Value
  }
  if ($docOut -notmatch '(?m)^test result:') {
    $docFailed = $true
    Write-Host ("FAIL doc-tests (missing 'test result:' line, exit {0})" -f $docCode) -ForegroundColor Red
    Write-Host $docOut
  } elseif (($docCode -ne 0) -or ($docOut -match '(?m)^test result:\s+FAILED')) {
    $docFailed = $true
    Write-Host ("FAIL doc-tests (exit {0}, passed {1})" -f $docCode, $docPassed) -ForegroundColor Red
    Write-Host $docOut
  } else {
    Write-Host ("PASS doc-tests ({0} passed)" -f $docPassed)
  }
}

$failed = 0
$totalPassed = 0
$totalSw = [System.Diagnostics.Stopwatch]::StartNew()

$parallelSw = [System.Diagnostics.Stopwatch]::StartNew()
$runs = @()
$admissionTimeoutSecs = 120
foreach ($b in $parallel) {
  $admitStart = [DateTime]::UtcNow
  while ($MaxJobs -gt 0 -and ((@($runs | Where-Object { -not $_.Proc.HasExited })).Count -ge $MaxJobs)) {
    if ((([DateTime]::UtcNow - $admitStart).TotalSeconds -gt $admissionTimeoutSecs)) {
      $stragglers = @($runs | Where-Object { -not $_.Proc.HasExited })
      foreach ($s in $stragglers) { try { $s.Proc.Kill() } catch { } }
      Write-Host ("RUST-TEST FAILED: admission timeout after {0}s waiting for a slot (MaxJobs={1}, stragglers={2})" -f $admissionTimeoutSecs, $MaxJobs, $stragglers.Count) -ForegroundColor Red
      exit 1
    }
    Start-Sleep -Milliseconds 100
  }
  $exe = $b.Exe
  $wd = $b.WorkDir
  if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host "RUST-TEST FAILED: missing binary $exe" -ForegroundColor Red
  }
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $exe
  $psi.WorkingDirectory = $wd
  $psi.UseShellExecute = $false
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true
  $proc = New-Object System.Diagnostics.Process
  $proc.StartInfo = $psi
  [void]$proc.Start()
  $outTask = $proc.StandardOutput.ReadToEndAsync()
  $errTask = $proc.StandardError.ReadToEndAsync()
  $runs += New-Object PSObject -Property @{ Proc = $proc; OutTask = $outTask; ErrTask = $errTask; Exe = $exe; WorkDir = $wd }
}

foreach ($r in $runs) {
  $exe = $r.Exe
  $short = Split-Path $exe -Leaf
  # Per-child budget: elapsed is the child's own run time (ExitTime-StartTime),
  # not when the collector reaches it, so a fast child reaped late cannot false-fail.
  $elapsedSoFarMs = ([DateTime]::Now - $r.Proc.StartTime).TotalMilliseconds
  $remainingMs = 120000 - [int]$elapsedSoFarMs
  if ($remainingMs -lt 0) { $remainingMs = 0 }
  if (-not $r.Proc.WaitForExit($remainingMs)) {
    try { $r.Proc.Kill() } catch { }
    try { $r.Proc.WaitForExit(5000) } catch { }
    $r.Proc.Refresh()
    try { $elapsedS = ($r.Proc.ExitTime - $r.Proc.StartTime).TotalSeconds } catch { $elapsedS = ([DateTime]::Now - $r.Proc.StartTime).TotalSeconds }
    $failed += 1
    Write-Host ("FAIL {0} (timeout after 120s budget, elapsed {1:N1}s)" -f $short, $elapsedS) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $r.WorkDir)
    continue
  }
  $r.Proc.Refresh()
  $elapsedMs = ($r.Proc.ExitTime - $r.Proc.StartTime).TotalMilliseconds
  if ($elapsedMs -gt 120000) {
    $elapsedS = $elapsedMs / 1000.0
    $failed += 1
    Write-Host ("FAIL {0} (timeout after 120s budget, elapsed {1:N1}s)" -f $short, $elapsedS) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $r.WorkDir)
    continue
  }
  $code = $r.Proc.ExitCode
  $out = "$($r.OutTask.Result)`n$($r.ErrTask.Result)"
  try { $r.Proc.Close() } catch { }
  if ($out -notmatch '(?m)^test result:') {
    $failed += 1
    Write-Host ("FAIL {0} (missing 'test result:' line, exit {1})" -f $short, $code) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $r.WorkDir)
    if ($out) { Write-Host $out }
    continue
  }
  $passed = 0
  $m = [regex]::Matches($out, '(?m)^test result:\s+ok\.\s+(\d+)\s+passed')
  foreach ($hit in $m) {
    $passed += [int]$hit.Groups[1].Value
  }
  $totalPassed += $passed
  $isFail = $false
  if ($code -ne 0) { $isFail = $true }
  if ($out -match '(?m)^test result:\s+FAILED') { $isFail = $true }
  if ($isFail) {
    $failed += 1
    Write-Host ("FAIL {0} (exit {1}, passed {2})" -f $short, $code, $passed) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $r.WorkDir)
    Write-Host $out
  } else {
    Write-Host ("PASS {0} ({1} passed)" -f $short, $passed)
  }
}
$parallelSw.Stop()
Write-Host ("[rust-test] parallel done: {0} binaries in {1:N1}s" -f $parallel.Count, $parallelSw.Elapsed.TotalSeconds)

$serialSw = [System.Diagnostics.Stopwatch]::StartNew()
foreach ($b in $serial) {
  $exe = $b.Exe
  $wd = $b.WorkDir
  $short = Split-Path $exe -Leaf
  if (-not (Test-Path -LiteralPath $exe)) {
    Write-Host "RUST-TEST FAILED: missing binary $exe" -ForegroundColor Red
  }
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $exe
  $psi.Arguments = '--test-threads=1'
  $psi.WorkingDirectory = $wd
  $psi.UseShellExecute = $false
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true
  $proc = New-Object System.Diagnostics.Process
  $proc.StartInfo = $psi
  [void]$proc.Start()
  $outTask = $proc.StandardOutput.ReadToEndAsync()
  $errTask = $proc.StandardError.ReadToEndAsync()
  # Per-child budget: elapsed is the child's own run time (ExitTime-StartTime).
  $elapsedSoFarMs = ([DateTime]::Now - $proc.StartTime).TotalMilliseconds
  $remainingMs = 120000 - [int]$elapsedSoFarMs
  if ($remainingMs -lt 0) { $remainingMs = 0 }
  if (-not $proc.WaitForExit($remainingMs)) {
    try { $proc.Kill() } catch { }
    try { $proc.WaitForExit(5000) } catch { }
    $proc.Refresh()
    try { $elapsedS = ($proc.ExitTime - $proc.StartTime).TotalSeconds } catch { $elapsedS = ([DateTime]::Now - $proc.StartTime).TotalSeconds }
    $failed += 1
    Write-Host ("FAIL {0} (timeout after 120s budget, elapsed {1:N1}s)" -f $short, $elapsedS) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $wd)
    continue
  }
  $proc.Refresh()
  $elapsedMs = ($proc.ExitTime - $proc.StartTime).TotalMilliseconds
  if ($elapsedMs -gt 120000) {
    $elapsedS = $elapsedMs / 1000.0
    $failed += 1
    Write-Host ("FAIL {0} (timeout after 120s budget, elapsed {1:N1}s)" -f $short, $elapsedS) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $wd)
    continue
  }
  $code = $proc.ExitCode
  $out = "$($outTask.Result)`n$($errTask.Result)"
  try { $proc.Close() } catch { }
  if ($out -notmatch '(?m)^test result:') {
    $failed += 1
    Write-Host ("FAIL {0} (missing 'test result:' line, exit {1})" -f $short, $code) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $wd)
    if ($out) { Write-Host $out }
    continue
  }
  $passed = 0
  $m = [regex]::Matches($out, '(?m)^test result:\s+ok\.\s+(\d+)\s+passed')
  foreach ($hit in $m) {
    $passed += [int]$hit.Groups[1].Value
  }
  $totalPassed += $passed
  $isFail = $false
  if ($code -ne 0) { $isFail = $true }
  if ($out -match '(?m)^test result:\s+FAILED') { $isFail = $true }
  if ($isFail) {
    $failed += 1
    Write-Host ("FAIL {0} (exit {1}, passed {2})" -f $short, $code, $passed) -ForegroundColor Red
    Write-Host ("  exe: {0}" -f $exe)
    Write-Host ("  cwd: {0}" -f $wd)
    Write-Host $out
  } else {
    Write-Host ("PASS {0} ({1} passed)" -f $short, $passed)
  }
}
$serialSw.Stop()
Write-Host ("[rust-test] serial done: {0} binaries in {1:N1}s" -f $serial.Count, $serialSw.Elapsed.TotalSeconds)
$totalSw.Stop()
Write-Host ("[rust-test] total wall: {0:N1}s" -f $totalSw.Elapsed.TotalSeconds)

$totalPassed += $docPassed
if ($docFailed) { $failed += 1 }
Write-Host ("rust-test: {0} binaries, total passed={1}, failed={2}" -f $binaries.Count, $totalPassed, $failed)
$scriptSw.Stop()
Write-Host ("[rust-test] script wall: {0:N1}s (overhead {1:N1}s)" -f $scriptSw.Elapsed.TotalSeconds, ($scriptSw.Elapsed - $totalSw.Elapsed).TotalSeconds)
if ($failed -gt 0) {
  Write-Host ("RUST-TEST FAILED: {0} binary(ies) failed" -f $failed) -ForegroundColor Red
  exit 1
}
exit 0
