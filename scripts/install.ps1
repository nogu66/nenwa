# Install nenwa on Windows.
#   irm https://github.com/nogu66/nenwa/releases/latest/download/install.ps1 | iex
#   With an invite: & ([scriptblock]::Create((irm https://github.com/nogu66/nenwa/releases/latest/download/install.ps1))) join <invite>
# 1. Download the binary and check its SHA256.
# 2. Put it at %LOCALAPPDATA%\nenwa\nenwa.exe.
# 3. Run "nenwa install" (or "nenwa <args>" when args are given).
# NENWA_DOWNLOAD_BASE changes where to download from (a local folder works too).
# NENWA_INSTALL_DRY_RUN=1 only places the binary and prints the command it would run.
# This file stays ASCII: Windows PowerShell 5.1 reads irm output as Latin-1 and local files in the ANSI code page.
# It never calls exit, because under "irm | iex" that would close the user's PowerShell window.
$ErrorActionPreference = 'Stop'
$ProgressPreference = 'SilentlyContinue'

$base = if ($env:NENWA_DOWNLOAD_BASE) { $env:NENWA_DOWNLOAD_BASE } else { 'https://github.com/nogu66/nenwa/releases/latest/download' }
# Windows on Arm runs the x64 binary too.
$file = 'nenwa-x86_64-pc-windows-msvc.zip'
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('nenwa-' + [guid]::NewGuid())
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  Write-Host 'Downloading nenwa (x86_64-pc-windows-msvc)...'
  foreach ($name in @($file, 'SHA256SUMS')) {
    if ($base -match '^[a-z]+://') {
      Invoke-WebRequest -UseBasicParsing -Uri "$base/$name" -OutFile (Join-Path $tmp $name)
    } else {
      Copy-Item -Path (Join-Path $base $name) -Destination (Join-Path $tmp $name)
    }
  }
  $want = ''
  foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
    $parts = $line -split '\s+'
    if ($parts.Count -ge 2 -and $parts[1].TrimStart('*') -eq $file) { $want = $parts[0].ToLower() }
  }
  $got = (Get-FileHash -Algorithm SHA256 -Path (Join-Path $tmp $file)).Hash.ToLower()
  if (-not $want -or $want -ne $got) { throw "SHA256 of $file does not match SHA256SUMS" }

  Expand-Archive -Path (Join-Path $tmp $file) -DestinationPath $tmp -Force
  $dir = Join-Path $env:LOCALAPPDATA 'nenwa'
  New-Item -ItemType Directory -Force -Path $dir | Out-Null
  $exe = Join-Path $dir 'nenwa.exe'
  Copy-Item -Force -Path (Join-Path $tmp 'nenwa.exe') -Destination $exe
  Write-Host "Placed: $exe"

  $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
  if (-not (($userPath -split ';') -contains $dir)) {
    Write-Host "$dir is not on your PATH. Add it to the user Path in Environment Variables to run nenwa by name."
  }
} finally {
  Remove-Item -Recurse -Force -Path $tmp -ErrorAction SilentlyContinue
}

$cmd = if ($args.Count -gt 0) { @($args) } else { @('install') }
if ($env:NENWA_INSTALL_DRY_RUN) {
  Write-Host "Would run (NENWA_INSTALL_DRY_RUN is set): $exe $($cmd -join ' ')"
} else {
  & $exe @cmd
}
