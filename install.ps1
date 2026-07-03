# dexdo installer for Windows (PowerShell).
#
#   irm https://github.com/gosh-sh/dexdo-cli/releases/latest/download/install.ps1 | iex
#
# Downloads the latest Windows release archive, verifies its checksum, and
# installs dexdo.exe into %LOCALAPPDATA%\dexdo\bin (override with $env:DEXDO_BIN_DIR).
$ErrorActionPreference = 'Stop'

$repo   = 'gosh-sh/dexdo-cli'
$binDir = if ($env:DEXDO_BIN_DIR) { $env:DEXDO_BIN_DIR } else { Join-Path $env:LOCALAPPDATA 'dexdo\bin' }

$rel  = Invoke-RestMethod "https://api.github.com/repos/$repo/releases/latest"
$ver  = $rel.tag_name
if (-not $ver) { throw 'dexdo: could not resolve the latest release' }
$vern = $ver.TrimStart('v')

$asset = "dexdo-$vern-x86_64-windows.zip"
$base  = "https://github.com/$repo/releases/download/$ver"

$tmp = Join-Path $env:TEMP ("dexdo-" + [System.Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Force -Path $tmp | Out-Null
try {
  Write-Host "dexdo: downloading $asset ($ver)"
  Invoke-WebRequest "$base/$asset" -OutFile (Join-Path $tmp $asset)

  # Verify the archive checksum against SHA256SUMS. Fail closed: a missing
  # SHA256SUMS, a missing entry for this asset, or a mismatch aborts the install
  # ($ErrorActionPreference = 'Stop' + the throws propagate out of the outer try).
  Invoke-WebRequest "$base/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS')
  $line = Select-String -Path (Join-Path $tmp 'SHA256SUMS') -Pattern ([regex]::Escape($asset)) | Select-Object -First 1
  if (-not $line) { throw "dexdo: $asset not found in SHA256SUMS" }
  $expected = ($line.Line -split '\s+')[0].ToLower()
  $actual   = (Get-FileHash -Algorithm SHA256 (Join-Path $tmp $asset)).Hash.ToLower()
  if ($expected -ne $actual) { throw 'dexdo: checksum mismatch' }
  Write-Host 'dexdo: checksum verified'

  Expand-Archive -Path (Join-Path $tmp $asset) -DestinationPath $tmp -Force
  New-Item -ItemType Directory -Force -Path $binDir | Out-Null
  Copy-Item (Join-Path $tmp "dexdo-$vern-x86_64-windows\dexdo.exe") (Join-Path $binDir 'dexdo.exe') -Force
  Write-Host "dexdo: installed $ver to $binDir\dexdo.exe"

  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($userPath -notlike "*$binDir*") {
    [Environment]::SetEnvironmentVariable('Path', "$userPath;$binDir", 'User')
    Write-Host "dexdo: added $binDir to your user PATH (restart your shell)"
  }
}
finally {
  Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
