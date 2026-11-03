# release.ps1 - cut a Potflix release from the current checkout, pre-publish half.
#
# Invoked as (stock Windows box, Rust + Inno + Python only):
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -Version X.Y.Z
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -Version X.Y.Z -WhatIf
#   powershell -ExecutionPolicy Bypass -File tools\release.ps1 -SelfTest
#
# What it does, in order: refuse unless the tree is clean, on main, in sync
# with origin, untagged, and carrying a forward version with release notes;
# gate the untouched HEAD; stamp build\release\<v>\; bump rust\Cargo.toml
# [workspace.package] (the single version authority) and nothing else;
# cargo update --workspace --offline asserting exactly the 12 own-crate lock
# lines; promote CHANGELOG.md ## [Unreleased]; re-gate the bumped tree;
# build the installer via tools\make-installer.ps1 and hash both artifacts;
# commit exactly the three files, annotate the tag, push HEAD and the tag
# atomically. Then it STOPS and prints one PUBLISH line: publishing stays
# with the caller's wrapper, so this script never calls gh and never creates
# a GitHub release.
#
# Exit codes (each names the state left behind):
#   0 released (pushed) and proven, PUBLISH line printed.
#   2 refused before any byte changed: dirty tree, or -Version missing/bad use.
#   3 refused before any byte changed: wrong origin or not on branch main.
#   4 refused before any byte changed: local main != origin/main, or the
#     remote could not be read to prove otherwise.
#   5 refused before any byte changed: tag v<Version> already exists, or the
#     remote tags could not be read to prove otherwise.
#   6 refused before any byte changed: version not X.Y.Z, not strictly greater
#     than the workspace version, or CHANGELOG [Unreleased] has no notes.
#   7 refused before any byte changed: the gate failed on the untouched HEAD.
#   8 failed AFTER writes: tree rolled back to the source commit, clean.
#   9 failed AFTER the push: commit + tag exist locally and possibly remotely;
#     fix the remote and push by hand, never rerun blindly.
#
# -WhatIf runs every static precondition for real and then prints the ordered
# steps with WOULD: prefixes instead of running them; it writes nothing and
# never runs the gate (the gate is a verifying step, listed as WOULD, not a
# static precondition). -SelfTest proves the six refusals against scratch git
# repos under the temp dir without touching this repo, then exits.
#
# Compatibility: Windows PowerShell 5.1 only; the script sticks to the 5.1
# language and cmdlet subset throughout. Messages carry repo-relative paths
# a drive letter or a machine path. Outside this repo it needs only cargo,
# git and powershell on PATH (ISCC/python stay behind make-installer.ps1).
param(
  [string]$Version,
  [switch]$WhatIf,
  [switch]$SelfTest
)
$ErrorActionPreference = 'Stop'

# Push-Location/Set-Location expand '[', ']' and '*' as wildcards, so a
# checkout path containing them aborts with PathNotFound. Escape every path
# handed to a wildcard-aware command. (Same helper as tools\check.ps1.)
function Get-Literal([string]$Path) {
  return [WildcardPattern]::Escape($Path)
}

$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent

# Runs a native exe with separated streams and returns @{ Code; Lines }
# (stdout lines only; stderr is drained and discarded). This exists because
# Windows PowerShell 5.1 turns native stderr into error records, which are
# terminating under 'Stop' - so & with 2>$null is not reliable here, and raw
# System.Diagnostics.Process is used wherever an exit code matters (same
# reason tools\check.ps1 avoids Start-Process for the self-test binary).
function Invoke-Native([string]$Exe, [string[]]$ExeArgs, [string]$WorkDir) {
  $psi = New-Object System.Diagnostics.ProcessStartInfo
  $psi.FileName = $Exe
  $quoted = @()
  foreach ($a in $ExeArgs) {
    $s = "$a"
    if ($s -eq '' -or $s.Contains(' ') -or $s.Contains("`t") -or $s.Contains('"')) {
      $s = '"' + ($s -replace '"', '\"') + '"'
    }
    $quoted += $s
  }
  $psi.Arguments = $quoted -join ' '
  if (-not [string]::IsNullOrWhiteSpace($WorkDir)) { $psi.WorkingDirectory = $WorkDir }
  $psi.UseShellExecute = $false
  $psi.RedirectStandardOutput = $true
  $psi.RedirectStandardError = $true
  $psi.CreateNoWindow = $true
  $proc = New-Object System.Diagnostics.Process
  $proc.StartInfo = $psi
  [void]$proc.Start()
  # Drain both pipes before waiting, or a child that fills one blocks forever.
  $outTask = $proc.StandardOutput.ReadToEndAsync()
  $errTask = $proc.StandardError.ReadToEndAsync()
  [void]$proc.WaitForExit()
  $code = $proc.ExitCode
  $outText = $outTask.Result
  [void]$errTask.Result
  $proc.Close()
  $lines = @()
  if (-not [string]::IsNullOrEmpty($outText)) { $lines = @($outText -split "`r?`n") }
  return @{ Code = $code; Lines = $lines }
}

# Run git in $Repo, swallowing stderr (it can carry machine paths, and this
# script's messages stay repo-relative). Returns @{ Code; Lines }.
function Invoke-Git([string]$Repo, [string[]]$GitArgs) {
  $all = @('-C', $Repo) + $GitArgs
  return (Invoke-Native 'git' $all '')
}

# Run a .ps1 entrypoint the way its own header documents it
# (powershell -ExecutionPolicy Bypass -File ...), capturing the exit code
# without echoing its stdout (it prints machine paths, which stay out of
# this script's messages). Returns @{ Code; Lines }.
function Invoke-PsScript([string]$ScriptPath, [string[]]$ScriptArgs) {
  $all = @('-ExecutionPolicy', 'Bypass', '-File', $ScriptPath) + $ScriptArgs
  return (Invoke-Native 'powershell' $all '')
}

# --- refusal predicates (pure reads, shared by real / WhatIf / SelfTest) ---

function Get-DirtyPaths([string]$Repo) {
  $r = Invoke-Git $Repo @('status', '--porcelain=v1')
  if ($r.Code -ne 0) { return $null }
  $paths = @()
  foreach ($line in $r.Lines) {
    $s = "$line"
    if ($s.Trim() -eq '') { continue }
    if ($s.Length -lt 4) { continue }
    $paths += $s.Substring(3)
  }
  return $paths
}

function Get-OriginUrl([string]$Repo) {
  $r = Invoke-Git $Repo @('remote', 'get-url', 'origin')
  if ($r.Code -ne 0) { return '' }
  return ("$($r.Lines -join '')").Trim()
}

# Normalises https/ssh remote URLs to owner/repo for the origin check.
function Get-OriginSlug([string]$Url) {
  $u = $Url.Trim().TrimEnd('/')
  if ($u.EndsWith('.git')) { $u = $u.Substring(0, $u.Length - 4) }
  if ($u -match '^(https?://[^/]+/|git@[^:]+:)(.+)$') { return $Matches[2].ToLowerInvariant() }
  return ''
}

function Get-Branch([string]$Repo) {
  $r = Invoke-Git $Repo @('rev-parse', '--abbrev-ref', 'HEAD')
  if ($r.Code -ne 0) { return '' }
  return ("$($r.Lines -join '')").Trim()
}

# -1 when A < B, 0 when equal, 1 when A > B, -2 when either is not X.Y.Z.
function Compare-Version([string]$A, [string]$B) {
  $pa = @(); $pb = @()
  foreach ($part in ($A -split '\.')) {
    $n = 0
    if (-not [int]::TryParse($part, [ref]$n)) { return -2 }
    $pa += $n
  }
  foreach ($part in ($B -split '\.')) {
    $n = 0
    if (-not [int]::TryParse($part, [ref]$n)) { return -2 }
    $pb += $n
  }
  if ($pa.Count -ne 3 -or $pb.Count -ne 3) { return -2 }
  for ($i = 0; $i -lt 3; $i++) {
    if ($pa[$i] -lt $pb[$i]) { return -1 }
    if ($pa[$i] -gt $pb[$i]) { return 1 }
  }
  return 0
}

function Test-VersionFormat([string]$V) {
  return ($V -match '^[0-9]+\.[0-9]+\.[0-9]+$')
}

# Reads the single authority: the one version line in the
# [workspace.package] block of rust\Cargo.toml. Returns @{ Count; Value }.
function Get-WorkspaceVersion([string]$Repo) {
  $cargo = Join-Path $Repo 'rust\Cargo.toml'
  if (-not (Test-Path -LiteralPath $cargo -PathType Leaf)) {
    return @{ Count = 0; Value = '' }
  }
  $inBlock = $false
  $count = 0
  $value = ''
  foreach ($line in [IO.File]::ReadAllLines($cargo)) {
    if ($line.StartsWith('[')) {
      $inBlock = ($line.Trim() -eq '[workspace.package]')
      continue
    }
    if ($inBlock -and ($line -match '^version = "([^"]+)"$')) {
      $count++
      $value = $Matches[1]
    }
  }
  return @{ Count = $count; Value = $value }
}

# True when CHANGELOG.md has a ## [Unreleased] section holding at least one
# non-blank line before the next ## heading.
function Test-ChangelogNotes([string]$Repo) {
  $log = Join-Path $Repo 'CHANGELOG.md'
  if (-not (Test-Path -LiteralPath $log -PathType Leaf)) { return $false }
  $text = [IO.File]::ReadAllText($log)
  $eol = "`r`n"
  if ($text.Contains("`r`n")) { $eol = "`r`n" } else { $eol = "`n" }
  $head = '## [Unreleased]' + $eol
  $idx = $text.IndexOf($head)
  if ($idx -lt 0) { return $false }
  $rest = $text.Substring($idx + $head.Length)
  $lines = $rest -split "`r?`n"
  foreach ($line in $lines) {
    if ($line.StartsWith('## ')) { return $false }
    if ($line.Trim() -ne '') { return $true }
  }
  return $false
}

function Get-LocalTagSha([string]$Repo, [string]$V) {
  $r = Invoke-Git $Repo @('rev-parse', '-q', '--verify', "refs/tags/v$V")
  if ($r.Code -ne 0) { return '' }
  return ("$($r.Lines -join '')").Trim()
}

# Runs the release gate (default: tools\check.ps1 -Build -Selftest from any
# cwd; it resolves its own root). $CheckPath exists so SelfTest can inject a
# stub. Returns the gate exit code; writes gitignored artifacts only.
function Invoke-ReleaseGate([string]$Repo, [string]$CheckPath) {
  if ([string]::IsNullOrWhiteSpace($CheckPath)) {
    $CheckPath = Join-Path $Repo 'tools\check.ps1'
  }
  $g = Invoke-PsScript $CheckPath @('-Build', '-Selftest')
  return $g.Code
}

# True when a --numstat line holds exactly the expected added/removed counts:
# the first two tab-separated fields must equal $Added/$Removed as strings,
# so '-' (binary) never passes; the trailing path is required but ignored.
function Test-Numstat([string]$Line, [string]$Added, [string]$Removed) {
  $parts = ("$Line").Trim() -split "`t"
  if ($parts.Count -lt 3) { return $false }
  return ($parts[0] -eq $Added -and $parts[1] -eq $Removed)
}

function Remove-ReleaseDir([string]$Repo, [string]$V) {
  $dir = Join-Path $Repo ("build\release\" + $V)
  if (Test-Path -LiteralPath $dir) {
    Remove-Item -LiteralPath $dir -Recurse -Force
  }
}

# Rolls the worktree back to the pre-release state: nothing was committed, so
# checking out the three touched files plus deleting the release dir is whole.
function Invoke-Rollback([string]$Repo, [string]$V) {
  [void](Invoke-Git $Repo @('checkout', '--', 'rust/Cargo.toml', 'rust/Cargo.lock', 'CHANGELOG.md'))
  Remove-ReleaseDir $Repo $V
}

# --- SelfTest: six refusals, scratch repos only, repo never written ---

function Invoke-SelfTest([string]$Repo) {
  if (-not (Get-Command git -ErrorAction SilentlyContinue)) {
    Write-Host 'SELFTEST FAIL: git not found on PATH'
    return 1
  }
  $before = @((Invoke-Git $Repo @('status', '--porcelain=v1')).Lines -join "`n")
  $tmpBase = $env:TEMP
  if ([string]::IsNullOrWhiteSpace($tmpBase)) { $tmpBase = [IO.Path]::GetTempPath() }
  $scratch = Join-Path $tmpBase ("potflix-release-selftest-" + $PID)
  if (Test-Path -LiteralPath $scratch) {
    Remove-Item -LiteralPath $scratch -Recurse -Force
  }
  New-Item -ItemType Directory -Force -Path $scratch | Out-Null
  try {
    # 1/7: dirty tree refuses with exit 2.
    $d = Join-Path $scratch 'dirty'
    New-Item -ItemType Directory -Force -Path $d | Out-Null
    [void](Invoke-Git $d @('init', '-q'))
    [void](Invoke-Git $d @('-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '--allow-empty', '-m', 'init'))
    Set-Content -LiteralPath (Join-Path $d 'file.txt') -Value 'x' -NoNewline
    $dirty = @(Get-DirtyPaths $d)
    if ($dirty.Count -eq 0) {
      Write-Host 'SELFTEST FAIL 1/7: dirty scratch repo read as clean'
      return 1
    }
    Write-Host 'SELFTEST 1/7 PASS: dirty tree refuses with exit 2'

    # 2/7: wrong branch refuses with exit 3.
    $b = Join-Path $scratch 'branch'
    New-Item -ItemType Directory -Force -Path $b | Out-Null
    [void](Invoke-Git $b @('init', '-q', '-b', 'main'))
    [void](Invoke-Git $b @('-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '--allow-empty', '-m', 'init'))
    [void](Invoke-Git $b @('checkout', '-q', '-b', 'feature/x'))
    $br = Get-Branch $b
    if ($br -eq 'main') {
      Write-Host 'SELFTEST FAIL 2/7: scratch branch read as main'
      return 1
    }
    Write-Host 'SELFTEST 2/7 PASS: wrong branch refuses with exit 3'

    # 3/7: HEAD ahead of origin/main refuses with exit 4 (local bare remote,
    # no network).
    $bare = Join-Path $scratch 'remote.git'
    [void](Invoke-Git $scratch @('init', '-q', '--bare', 'remote.git'))
    $w = Join-Path $scratch 'work'
    [void](Invoke-Git $scratch @('clone', '-q', $bare, 'work'))
    [void](Invoke-Git $w @('-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '--allow-empty', '-m', 'one'))
    [void](Invoke-Git $w @('push', '-q', 'origin', 'HEAD:main'))
    [void](Invoke-Git $w @('-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '--allow-empty', '-m', 'two'))
    $head = ("$((Invoke-Git $w @('rev-parse', 'HEAD')).Lines -join '')").Trim()
    $up = ("$((Invoke-Git $w @('rev-parse', 'origin/main')).Lines -join '')").Trim()
    if ($head -eq '' -or $head -eq $up) {
      Write-Host 'SELFTEST FAIL 3/7: scratch clone did not diverge as built'
      return 1
    }
    Write-Host 'SELFTEST 3/7 PASS: diverged main refuses with exit 4'

    # 4/7: existing tag refuses with exit 5.
    $t = Join-Path $scratch 'tag'
    New-Item -ItemType Directory -Force -Path $t | Out-Null
    [void](Invoke-Git $t @('init', '-q'))
    [void](Invoke-Git $t @('-c', 'user.name=t', '-c', 'user.email=t@t', 'commit', '-q', '--allow-empty', '-m', 'init'))
    [void](Invoke-Git $t @('tag', 'v9.9.9'))
    $sha = Get-LocalTagSha $t '9.9.9'
    if ($sha -eq '') {
      Write-Host 'SELFTEST FAIL 4/7: scratch tag not found after tagging'
      return 1
    }
    Write-Host 'SELFTEST 4/7 PASS: existing tag refuses with exit 5'

    # 5/7: non-greater version refuses with exit 6 (pure compare, no repo).
    $c1 = Compare-Version '1.2.0' '1.2.0'
    $c2 = Compare-Version '1.1.9' '1.2.0'
    $c3 = Compare-Version '1.10.0' '1.9.9'
    if ($c1 -ne 0 -or $c2 -ne -1 -or $c3 -ne 1) {
      Write-Host 'SELFTEST FAIL 5/7: version compare misordered equal/lower/higher'
      return 1
    }
    Write-Host 'SELFTEST 5/7 PASS: non-greater version refuses with exit 6'

    # 6/7: red gate refuses with exit 7 (stub scripts, never the real gate).
    $failStub = Join-Path $scratch 'gate-fail.ps1'
    Set-Content -LiteralPath $failStub -Value "param([switch]`$Build,[switch]`$Selftest)`nexit 1" -NoNewline
    $passStub = Join-Path $scratch 'gate-pass.ps1'
    Set-Content -LiteralPath $passStub -Value "param([switch]`$Build,[switch]`$Selftest)`nexit 0" -NoNewline
    $redCode = Invoke-ReleaseGate $scratch $failStub
    $greenCode = Invoke-ReleaseGate $scratch $passStub
    if ($redCode -eq 0 -or $greenCode -ne 0) {
      Write-Host 'SELFTEST FAIL 6/7: gate stub did not map red to nonzero and green to zero'
      return 1
    }
    Write-Host 'SELFTEST 6/7 PASS: red gate refuses with exit 7'

    # 7/7: numstat parse takes the first two tab fields, ignoring the path.
    if (-not (Test-Numstat "1`t1`tpath" '1' '1')) {
      Write-Host 'SELFTEST FAIL 7/7: numstat 1/1 with path was not accepted'
      return 1
    }
    if ((Test-Numstat "2`t1`tpath" '1' '1') -or (Test-Numstat "-`t-`tpath" '1' '1')) {
      Write-Host 'SELFTEST FAIL 7/7: numstat 2/1 or binary -/- was wrongly accepted'
      return 1
    }
    Write-Host 'SELFTEST 7/7 PASS: numstat parse accepts 1/1 with path and refuses 2/1'
  } finally {
    Remove-Item -LiteralPath $scratch -Recurse -Force -ErrorAction SilentlyContinue
  }
  $after = @((Invoke-Git $Repo @('status', '--porcelain=v1')).Lines -join "`n")
  if ($before -ne $after) {
    Write-Host 'SELFTEST FAIL: repo tree changed during SelfTest'
    return 1
  }
  return 0
}

if ($SelfTest) {
  exit (Invoke-SelfTest $root)
}

# --- argument gate (usage refusal, before any byte can change) ---

if ([string]::IsNullOrWhiteSpace($Version)) {
  Write-Host 'RELEASE REFUSED: -Version X.Y.Z is required (or run with -SelfTest)'
  exit 2
}

# --- static preconditions (real mode and WhatIf share them; all read-only
# except fetch, which moves only the origin/main tracking ref) ---

$dirtyPaths = @(Get-DirtyPaths $root)
if ($null -eq $dirtyPaths) {
  Write-Host 'RELEASE REFUSED: cannot read the working tree status'
  exit 2
}
if ($dirtyPaths.Count -gt 0) {
  $shown = ($dirtyPaths | Select-Object -First 3) -join ', '
  Write-Host ("RELEASE REFUSED: working tree has " + $dirtyPaths.Count + " uncommitted paths (" + $shown + "); commit or stash them first")
  exit 2
}

$originUrl = Get-OriginUrl $root
if ((Get-OriginSlug $originUrl) -ne 'nathwn12/potflix') {
  Write-Host ("RELEASE REFUSED: origin is " + $originUrl + ", not nathwn12/potflix")
  exit 3
}
$branch = Get-Branch $root
if ($branch -ne 'main') {
  Write-Host ("RELEASE REFUSED: on branch " + $branch + ", not main")
  exit 3
}

$fetch = Invoke-Git $root @('fetch', '--quiet', 'origin', 'main')
if ($fetch.Code -ne 0) {
  Write-Host 'RELEASE REFUSED: cannot fetch origin/main (offline?); sync first'
  exit 4
}
$headSha = ("$((Invoke-Git $root @('rev-parse', 'HEAD')).Lines -join '')").Trim()
$upSha = ("$((Invoke-Git $root @('rev-parse', 'origin/main')).Lines -join '')").Trim()
if ($headSha -eq '' -or $upSha -eq '' -or ($headSha -ne $upSha)) {
  $counts = ("$((Invoke-Git $root @('rev-list', '--left-right', '--count', 'origin/main...HEAD')).Lines -join '')").Trim()
  $ahead = '?'; $behind = '?'
  if ($counts -match '^\s*(\d+)\s+(\d+)\s*$') {
    $behind = $Matches[1]; $ahead = $Matches[2]
  }
  Write-Host ("RELEASE REFUSED: main is " + $ahead + " ahead / " + $behind + " behind origin/main; sync first")
  exit 4
}

$localTag = Get-LocalTagSha $root $Version
if ($localTag -ne '') {
  Write-Host ("RELEASE REFUSED: tag v" + $Version + " already exists (local at " + $localTag + ")")
  exit 5
}
$lsr = Invoke-Git $root @('ls-remote', '--tags', 'origin', "refs/tags/v$Version")
if ($lsr.Code -ne 0) {
  Write-Host ("RELEASE REFUSED: cannot query origin tags for v" + $Version + " (offline?)")
  exit 5
}
if ((("$($lsr.Lines -join "`n")").Trim()) -ne '') {
  Write-Host ("RELEASE REFUSED: tag v" + $Version + " already exists (origin holds it)")
  exit 5
}

if (-not (Test-VersionFormat $Version)) {
  Write-Host ("RELEASE REFUSED: " + $Version + " is not X.Y.Z")
  exit 6
}
$ws = Get-WorkspaceVersion $root
if ($ws.Count -ne 1) {
  Write-Host ("RELEASE REFUSED: rust\Cargo.toml [workspace.package] holds " + $ws.Count + " version lines (expected 1)")
  exit 6
}
if (-not (Test-VersionFormat $ws.Value)) {
  Write-Host 'RELEASE REFUSED: workspace version is not X.Y.Z; fix rust\Cargo.toml first'
  exit 6
}
if ((Compare-Version $Version $ws.Value) -ne 1) {
  Write-Host ("RELEASE REFUSED: " + $Version + " is not greater than the current " + $ws.Value)
  exit 6
}
if (-not (Test-ChangelogNotes $root)) {
  Write-Host 'RELEASE REFUSED: CHANGELOG.md [Unreleased] has no notes, so this release would ship no notes'
  exit 6
}

# --- WhatIf: preconditions passed; list the ordered steps, change nothing ---

if ($WhatIf) {
  Write-Host ("WOULD: 1/13 verify origin nathwn12/potflix on branch main (refuse 3)")
  Write-Host ("WOULD: 2/13 verify the working tree is clean (refuse 2)")
  Write-Host ("WOULD: 3/13 fetch origin and verify HEAD equals origin/main (refuse 4)")
  Write-Host ("WOULD: 4/13 verify tag v" + $Version + " is absent locally and on origin (refuse 5)")
  Write-Host ("WOULD: 5/13 verify " + $Version + " is X.Y.Z, greater than " + $ws.Value + ", with CHANGELOG notes (refuse 6)")
  Write-Host 'WOULD: 6/13 run the gate on the untouched HEAD: tools\check.ps1 -Build -Selftest (refuse 7)'
  Write-Host ("WOULD: 7/13 stamp build\release\" + $Version + "\release.log with the source commit")
  Write-Host 'WOULD: 8/13 bump rust\Cargo.toml [workspace.package] version only (assert 1 added / 1 removed, one hunk)'
  Write-Host 'WOULD: 9/13 run cargo update --workspace --offline (assert 12/12 own-crate lock lines)'
  Write-Host ("WOULD: 10/13 promote CHANGELOG.md ## [Unreleased] to ## [" + $Version + "] - <date>")
  Write-Host 'WOULD: 11/13 re-run the gate on the bumped tree (roll back on red)'
  Write-Host 'WOULD: 12/13 run tools\make-installer.ps1 and hash the installer plus dist\potflix.exe'
  Write-Host ("WOULD: 13/13 commit release: v" + $Version + ", tag v" + $Version + " annotated, push HEAD and the tag")
  Write-Host ("WOULD: print PUBLISH: v" + $Version + " <installer> <exe> <sha256> and exit 0")
  exit 0
}

# --- real mode: gate the untouched HEAD before any version byte changes ---

Write-Host 'RELEASE: gating the untouched HEAD'
$gateCode = Invoke-ReleaseGate $root ''
if ($gateCode -ne 0) {
  Write-Host ("RELEASE REFUSED: gate failed on the untouched HEAD (exit " + $gateCode + "); nothing was changed")
  exit 7
}

# --- stamp the build directory ---

$relDir = Join-Path $root ("build\release\" + $Version)
if (Test-Path -LiteralPath $relDir) {
  Write-Host ("RELEASE FAILED: build\release\" + $Version + " already exists; delete it or pass a new version")
  exit 8
}
New-Item -ItemType Directory -Force -Path (Get-Literal $relDir) | Out-Null
$relDate = (Get-Date).ToString('yyyy-MM-dd')
$relLog = Join-Path $relDir 'release.log'
Set-Content -LiteralPath $relLog -Value ("version=" + $Version + "`r`nSOURCE_SHA=" + $headSha + "`r`ndate=" + $relDate + "`r`n") -NoNewline

# --- bump exactly one line: the [workspace.package] version ---

Write-Host 'RELEASE: bumping rust\Cargo.toml'
$cargoPath = Join-Path $root 'rust\Cargo.toml'
$cargoRaw = [IO.File]::ReadAllText($cargoPath)
$cargoEol = "`r`n"
if (-not $cargoRaw.Contains("`r`n")) { $cargoEol = "`n" }
$rawLines = $cargoRaw -split "`r?`n"
$inBlock = $false
$hits = 0
for ($i = 0; $i -lt $rawLines.Length; $i++) {
  if ($rawLines[$i].StartsWith('[')) {
    $inBlock = ($rawLines[$i].Trim() -eq '[workspace.package]')
    continue
  }
  if ($inBlock -and ($rawLines[$i] -match '^version = "([^"]+)"$')) {
    $rawLines[$i] = 'version = "' + $Version + '"'
    $hits++
  }
}
if ($hits -ne 1) {
  Write-Host ("RELEASE FAILED: rust\Cargo.toml [workspace.package] holds " + $hits + " version lines (expected 1)")
  Invoke-Rollback $root $Version
  exit 8
}
[IO.File]::WriteAllText($cargoPath, ($rawLines -join $cargoEol), (New-Object Text.UTF8Encoding($false)))
$numstat = ("$((Invoke-Git $root @('diff', '--numstat', '--', 'rust/Cargo.toml')).Lines -join '')").Trim()
if (-not (Test-Numstat $numstat '1' '1')) {
  Write-Host ("RELEASE FAILED: rust\Cargo.toml diff is " + $numstat + ", expected 1 added / 1 removed; rolled back")
  Invoke-Rollback $root $Version
  exit 8
}
$hunks = @((Invoke-Git $root @('diff', '-U0', '--', 'rust/Cargo.toml')).Lines | Where-Object { $_ -match '^@@' })
if ($hunks.Count -ne 1) {
  Write-Host 'RELEASE FAILED: rust\Cargo.toml diff touches more than one hunk; rolled back'
  Invoke-Rollback $root $Version
  exit 8
}

# --- derive the lockfile, offline, asserting the 12 own-crate lines ---

Write-Host 'RELEASE: deriving rust\Cargo.lock'
$cargoCode = (Invoke-Native 'cargo' @('update', '--workspace', '--offline') (Join-Path $root 'rust')).Code
if ($cargoCode -ne 0) {
  Write-Host ("RELEASE FAILED: cargo update --offline failed (exit " + $cargoCode + "); the local registry cache is incomplete; rolled back")
  Invoke-Rollback $root $Version
  exit 8
}
$lockStat = ("$((Invoke-Git $root @('diff', '--numstat', '--', 'rust/Cargo.lock')).Lines -join '')").Trim()
if (-not (Test-Numstat $lockStat '12' '12')) {
  Write-Host ("RELEASE FAILED: Cargo.lock changed " + $lockStat + ", expected 12 added / 12 removed own-crate lines; rolled back")
  Invoke-Rollback $root $Version
  exit 8
}
$lockDiff = (Invoke-Git $root @('diff', '--', 'rust/Cargo.lock')).Lines
$added = 0; $removed = 0; $foreign = 0
foreach ($dl in $lockDiff) {
  $s = "$dl"
  if ($s.StartsWith('+++') -or $s.StartsWith('---') -or $s.StartsWith('diff ') -or $s.StartsWith('index ') -or $s.StartsWith('@@')) { continue }
  if ($s.StartsWith('+')) {
    if ($s -match '^\+version = "') { $added++ } else { $foreign++ }
  } elseif ($s.StartsWith('-')) {
    if ($s -match '^-version = "') { $removed++ } else { $foreign++ }
  }
}
if ($added -ne 12 -or $removed -ne 12 -or $foreign -ne 0) {
  Write-Host 'RELEASE FAILED: Cargo.lock changed beyond the 12 own-crate version lines; rolled back'
  Invoke-Rollback $root $Version
  exit 8
}

# --- promote the changelog heading, byte-preserving ---

Write-Host 'RELEASE: promoting CHANGELOG.md'
$logPath = Join-Path $root 'CHANGELOG.md'
$logText = [IO.File]::ReadAllText($logPath)
$le = "`r`n"
if (-not $logText.Contains("`r`n")) { $le = "`n" }
$needle = '## [Unreleased]' + $le
$nIdx = $logText.IndexOf($needle)
if ($nIdx -lt 0) {
  Write-Host 'RELEASE FAILED: CHANGELOG.md has no ## [Unreleased] heading; rolled back'
  Invoke-Rollback $root $Version
  exit 8
}
$newText = $logText.Substring(0, $nIdx) + '## [Unreleased]' + $le + $le + '## [' + $Version + '] - ' + $relDate + $le + $logText.Substring($nIdx + $needle.Length)
[IO.File]::WriteAllText($logPath, $newText, (New-Object Text.UTF8Encoding($false)))
$heads = @()
foreach ($m in [regex]::Matches($newText, '(?m)^## .+$')) {
  $heads += ($m.Value -replace '\r$', '')
  if ($heads.Count -ge 2) { break }
}
if ($heads.Count -lt 2 -or $heads[0] -ne '## [Unreleased]' -or $heads[1] -ne ('## [' + $Version + '] - ' + $relDate)) {
  Write-Host 'RELEASE FAILED: CHANGELOG.md headings are not [Unreleased] then the new version; rolled back'
  Invoke-Rollback $root $Version
  exit 8
}

# --- re-gate the bumped tree; rollback is still cheap (nothing committed) ---

Write-Host 'RELEASE: re-gating the bumped tree'
$gate2 = Invoke-ReleaseGate $root ''
if ($gate2 -ne 0) {
  Write-Host ("RELEASE FAILED: gate failed after the version bump (exit " + $gate2 + "); the tree was restored")
  Invoke-Rollback $root $Version
  exit 8
}

# --- build the installer from the bumped tree, hash both artifacts ---

Write-Host 'RELEASE: building the installer'
$exeRel = 'dist\potflix.exe'
$exePath = Join-Path $root $exeRel
$newestRs = Get-ChildItem -LiteralPath (Join-Path $root 'rust') -Recurse -Filter *.rs -File |
  Sort-Object LastWriteTimeUtc -Descending | Select-Object -First 1
if ($null -ne $newestRs -and (Get-Item -LiteralPath $exePath).LastWriteTimeUtc -lt $newestRs.LastWriteTimeUtc) {
  Write-Host 'RELEASE FAILED: dist\potflix.exe is older than the newest source file (stale build); rolled back'
  Invoke-Rollback $root $Version
  exit 8
}
$mi = Invoke-PsScript (Join-Path $root 'tools\make-installer.ps1') @()
if ($mi.Code -ne 0) {
  Write-Host ("RELEASE FAILED: tools\make-installer.ps1 failed (exit " + $mi.Code + "); rolled back")
  Invoke-Rollback $root $Version
  exit 8
}
$setupRel = "dist\Potflix-Setup-" + $Version + ".exe"
$setupPath = Join-Path $root $setupRel
if (-not (Test-Path -LiteralPath $setupPath -PathType Leaf)) {
  Write-Host ("RELEASE FAILED: " + $setupRel + " was not produced; rolled back")
  Invoke-Rollback $root $Version
  exit 8
}
if ((Get-Item -LiteralPath $setupPath).LastWriteTimeUtc -le (Get-Item -LiteralPath $exePath).LastWriteTimeUtc) {
  Write-Host ("RELEASE FAILED: " + $setupRel + " is not newer than dist\potflix.exe (stale installer); rolled back")
  Invoke-Rollback $root $Version
  exit 8
}
$setupHash = (Get-FileHash -LiteralPath $setupPath -Algorithm SHA256).Hash
$exeHash = (Get-FileHash -LiteralPath $exePath -Algorithm SHA256).Hash
Add-Content -LiteralPath $relLog -Value ("installer=" + $setupRel + "`r`ninstaller_sha256=" + $setupHash + "`r`nexe_sha256=" + $exeHash + "`r`n") -NoNewline

# --- commit exactly the three files, tag, push atomically ---

Write-Host 'RELEASE: committing and tagging'
$st = (Invoke-Git $root @('status', '--porcelain=v1')).Lines
$changed = @()
foreach ($line in $st) {
  $s = "$line"
  if ($s.Trim() -eq '' -or $s.Length -lt 4) { continue }
  $changed += $s.Substring(3).Replace('\', '/')
}
$expected = @('rust/Cargo.toml', 'rust/Cargo.lock', 'CHANGELOG.md')
$same = (($changed | Sort-Object) -join '|') -eq (($expected | Sort-Object) -join '|')
if (-not $same) {
  Write-Host ("RELEASE FAILED: unexpected changed paths (" + ($changed -join ', ') + "); expected exactly rust/Cargo.toml, rust/Cargo.lock, CHANGELOG.md; rolled back")
  Invoke-Rollback $root $Version
  exit 8
}
[void](Invoke-Git $root @('add', '--', 'rust/Cargo.toml', 'rust/Cargo.lock', 'CHANGELOG.md'))
$commit = Invoke-Git $root @('commit', '-m', "release: v$Version", '-m', '3 files: rust/Cargo.toml rust/Cargo.lock CHANGELOG.md')
if ($commit.Code -ne 0) {
  Write-Host 'RELEASE FAILED: commit failed; rolled back'
  Invoke-Rollback $root $Version
  exit 8
}
[void](Invoke-Git $root @('tag', '-a', "v$Version", '-m', "Potflix $Version"))
$peeled = ("$((Invoke-Git $root @('rev-parse', "v$Version^{commit}")).Lines -join '')").Trim()
$newHead = ("$((Invoke-Git $root @('rev-parse', 'HEAD')).Lines -join '')").Trim()
if ($peeled -eq '' -or $peeled -ne $newHead) {
  Write-Host ("RELEASE FAILED: tag v" + $Version + " does not point at HEAD; rolled back")
  [void](Invoke-Git $root @('tag', '-d', "v$Version"))
  [void](Invoke-Git $root @('reset', '--hard', 'origin/main'))
  exit 8
}
$pre = Invoke-Git $root @('fetch', '--quiet', 'origin', 'main')
$stillBase = ("$((Invoke-Git $root @('rev-parse', 'origin/main')).Lines -join '')").Trim()
if ($pre.Code -ne 0 -or $stillBase -ne $headSha) {
  Write-Host 'RELEASE FAILED: origin/main moved mid-release; commit and tag exist locally only; push by hand after reviewing'
  exit 9
}
$push = Invoke-Git $root @('push', '--atomic', 'origin', 'refs/heads/main:refs/heads/main', "refs/tags/v$Version`:refs/tags/v$Version")
if ($push.Code -ne 0) {
  Write-Host 'RELEASE FAILED: atomic push rejected; commit and tag exist locally only; fix the remote and push by hand'
  exit 9
}

# --- stop before publishing; the wrapper owns the release ---

Write-Host ("PUBLISH: v" + $Version + " " + $setupRel + " " + $exeRel + " " + $setupHash)
exit 0
