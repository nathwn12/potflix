# Build the Potflix installer: multi-size icon + ISCC compile of potflix.iss.
#
# Produces dist\Potflix-Setup-<version>.exe (per-user, no UAC), where <version>
# is the exe's own VERSIONINFO (the workspace version, rust\Cargo.toml).
# Requires Pillow (`python`, tested with 11.2.1) and Inno Setup 6 (`ISCC` on PATH).
$ErrorActionPreference = 'Stop'

$root = Split-Path (Split-Path $MyInvocation.MyCommand.Path -Parent) -Parent
$toolsDir = Join-Path $root 'tools'
$srcIcon = Join-Path $root 'assets\icon.ico'
$dstIcon = Join-Path $root 'build\potflix.ico'
$iss = Join-Path $toolsDir 'potflix.iss'

New-Item -ItemType Directory -Force (Split-Path $dstIcon -Parent) | Out-Null

# Multi-size icon for the installer + shortcuts. Pillow reads the 256x256
# source .ico and writes all sizes Windows may request into build\.
$pyCode = @'
from PIL import Image
src = r'__SRC__'
dst = r'__DST__'
img = Image.open(src)
img.save(dst, sizes=[(16, 16), (20, 20), (24, 24), (32, 32), (48, 48), (64, 64), (128, 128), (256, 256)])
print('wrote ' + dst)
'@ -replace '__SRC__', $srcIcon -replace '__DST__', $dstIcon
$pyCode | python -
if ($LASTEXITCODE -ne 0) { throw "icon generation failed (exit $LASTEXITCODE)" }

$iscc = Get-Command ISCC -ErrorAction SilentlyContinue | Select-Object -ExpandProperty Source -ErrorAction SilentlyContinue
if ([string]::IsNullOrWhiteSpace($iscc)) { $iscc = 'C:\Program Files (x86)\Inno Setup 6\ISCC.exe' }

# The single version fact flows exe -> installer: read the VERSIONINFO the
# build stamped from CARGO_PKG_VERSION, fail loudly when it is missing, and
# refuse a stale exe older than the newest TRACKED rust/icon source (a rebuilt
# exe is never older than the source it was built from; tools/ docs never
# stale it).
$exe = Join-Path $root 'dist\potflix.exe'
if (-not (Test-Path -LiteralPath $exe)) { throw "missing ${exe}: build it first with tools\rust-build.ps1 so VERSIONINFO carries the workspace version" }
$vi = (Get-Item -LiteralPath $exe).VersionInfo
$version = $vi.ProductVersion
if ([string]::IsNullOrWhiteSpace($version)) { $version = $vi.FileVersion }
$version = "$version".Trim()
if ([string]::IsNullOrWhiteSpace($version)) { throw "exe has no VERSIONINFO FileVersion/ProductVersion (${exe}): rebuild with tools\rust-build.ps1 so it carries the workspace version" }
$exeTime = (Get-Item -LiteralPath $exe).LastWriteTimeUtc
$tracked = & git -C $root ls-files -- rust assets/icon.ico
if ($LASTEXITCODE -ne 0 -or [string]::IsNullOrWhiteSpace("$tracked")) { throw "cannot list tracked source files (git ls-files failed)" }
$newest = $null
foreach ($rel in ($tracked -split "`r?`n")) {
  if ([string]::IsNullOrWhiteSpace($rel)) { continue }
  $full = Join-Path $root $rel
  if (-not (Test-Path -LiteralPath $full -PathType Leaf)) { continue }
  $t = (Get-Item -LiteralPath $full).LastWriteTimeUtc
  if ($null -eq $newest -or $t -gt $newest) { $newest = $t }
}
if ($null -ne $newest -and $exeTime -lt $newest) { throw "stale exe ($exe, $exeTime) is older than the newest tracked source ($newest): rebuild with tools\rust-build.ps1 first" }

& $iscc $iss "/DMyAppVersion=$version"
if ($LASTEXITCODE -ne 0) { throw "ISCC failed (exit $LASTEXITCODE)" }

$setup = Join-Path $root "dist\Potflix-Setup-$version.exe"
Write-Host "BUILT: $setup"
Get-Item $setup | Select-Object Name, Length
