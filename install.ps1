# Installs cliproxy-rs on Windows, sets it up and starts it. In PowerShell:
#
#   irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1 | iex
#
# The first run downloads the Windows release, checks it against the release's SHA256SUMS
# and installs %LOCALAPPDATA%\Programs\cliproxy-rs\cliproxy.exe. It writes
# %USERPROFILE%\.cliproxy-rs\config.yaml with a free port (8317, or the next free one) and
# new keys, which it saves in keys.env next to it and never prints. Then it starts the
# server in the background and checks that it answers. Running it again upgrades the
# binary and restarts the server; an existing config.yaml or keys.env is never changed.
#
# Options, passed like this:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1))) -Service
#   -Service     also start cliproxy-rs when you sign in to Windows
#   -BinaryOnly  only install or upgrade the binary
#
# Environment: CLIPROXY_VERSION (a tag such as v0.1.0; default the latest),
# CLIPROXY_INSTALL_DIR, CLIPROXY_HOME, CLIPROXY_NO_OPEN=1 (never open a browser) and
# CLIPROXY_RELEASES (release page base URL, for mirrors and tests).
param([switch]$Service, [switch]$BinaryOnly)

# Everything runs in this function, so a download cut short cannot run half a script, and
# `throw` reports a failure without closing the window that ran `irm | iex`.
function Install-CliproxyRs([bool]$Service, [bool]$BinaryOnly) {
  $ErrorActionPreference = 'Stop'
  $ProgressPreference = 'SilentlyContinue'
  [Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

  $releases = if ($env:CLIPROXY_RELEASES) { $env:CLIPROXY_RELEASES } else { 'https://github.com/vayungodara/cliproxy-rs/releases' }
  $dir = if ($env:CLIPROXY_INSTALL_DIR) { $env:CLIPROXY_INSTALL_DIR } else { Join-Path $env:LOCALAPPDATA 'Programs\cliproxy-rs' }
  $data = if ($env:CLIPROXY_HOME) { $env:CLIPROXY_HOME } else { Join-Path $env:USERPROFILE '.cliproxy-rs' }
  $exe = Join-Path $dir 'cliproxy.exe'
  $runKey = 'HKCU:\Software\Microsoft\Windows\CurrentVersion\Run'
  if ($Service -and $BinaryOnly) { throw 'install.ps1: -Service and -BinaryOnly do not go together' }
  # A start-at-sign-in entry from an earlier run keeps being used.
  if (Get-ItemProperty -Path $runKey -Name 'cliproxy-rs' -ErrorAction SilentlyContinue) { $Service = -not $BinaryOnly }

  $arch = if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  # ARM64 Windows runs the x86_64 build through its emulation.
  if ($arch -notin 'AMD64', 'ARM64') { throw "install.ps1: there is no release binary for $arch Windows" }

  $tag = $env:CLIPROXY_VERSION
  if (-not $tag) {
    # /releases/latest redirects to /releases/tag/<tag> once a release exists.
    try { $r = Invoke-Retry { Invoke-WebRequest -Uri "$releases/latest" -Method Head -UseBasicParsing } }
    catch { throw "install.ps1: cannot reach $releases ($($_.Exception.Message))" }
    $final = if ($r.BaseResponse.ResponseUri) { $r.BaseResponse.ResponseUri.AbsoluteUri } else { $r.BaseResponse.RequestMessage.RequestUri.AbsoluteUri }
    if ($final -notmatch '/tag/([^/]+)$') { throw 'install.ps1: no release is published yet' }
    $tag = $Matches[1]
  }
  $name = "cliproxy-$($tag.TrimStart('v'))-x86_64-pc-windows-msvc"

  $tmp = Join-Path ([IO.Path]::GetTempPath()) ("cliproxy-" + [guid]::NewGuid())
  New-Item -ItemType Directory -Path $tmp | Out-Null
  try {
    $zip = Join-Path $tmp "$name.zip"
    try {
      Invoke-Retry { Invoke-WebRequest -Uri "$releases/download/$tag/$name.zip" -OutFile $zip -UseBasicParsing }
      Invoke-Retry { Invoke-WebRequest -Uri "$releases/download/$tag/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') -UseBasicParsing }
    } catch { throw "install.ps1: could not download $name.zip and SHA256SUMS from release $tag ($($_.Exception.Message))" }
    $expected = Get-Content (Join-Path $tmp 'SHA256SUMS') | ForEach-Object {
      $hash, $file = $_ -split '\s+', 2
      if ($file -eq "$name.zip" -or $file -eq "*$name.zip") { $hash }
    } | Select-Object -First 1
    if (-not $expected) { throw "install.ps1: $name.zip is not listed in SHA256SUMS" }
    if ((Get-FileHash $zip -Algorithm SHA256).Hash -ne $expected) { throw "install.ps1: checksum mismatch for $name.zip; nothing was installed" }
    Expand-Archive -Path $zip -DestinationPath $tmp -Force

    $pidfile = Join-Path $data 'cliproxy.pid'
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    # Windows permits renaming a running image. Leave it serving until the new
    # image is in place; -BinaryOnly never stops the running server.
    $previous = Join-Path $dir ("cliproxy.prev-" + [guid]::NewGuid() + '.exe')
    if (Test-Path $exe) { Move-Item $exe $previous }
    try { Copy-Item (Join-Path $tmp "$name\cliproxy.exe") $exe } catch {
      Remove-Item $exe -Force -ErrorAction SilentlyContinue
      if (Test-Path $previous) { Move-Item $previous $exe }
      throw
    }
    # A running old image may remain locked until the next installer run.
    Get-ChildItem $dir -Filter 'cliproxy.prev-*.exe' | Remove-Item -Force -ErrorAction SilentlyContinue
  } finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }
  Write-Host "Installed $(& $exe --version | Select-Object -Last 1) at $exe"
  if (($env:Path -split ';') -notcontains $dir) { Write-Host "Add $dir to your PATH to run cliproxy by name." }
  if ($BinaryOnly) { return }

  $config = Join-Path $data 'config.yaml'
  $keys = Join-Path $data 'keys.env'
  $log = Join-Path $data 'cliproxy.log'
  if (-not (Test-Path $config)) {
    if (Test-Path $keys) { throw "install.ps1: $keys exists but $config does not; restore config.yaml, or move keys.env away to make new keys" }
    New-Item -ItemType Directory -Force -Path (Join-Path $data 'auth') | Out-Null
    # Only you, SYSTEM and Administrators can read the folder, wherever CLIPROXY_HOME points.
    $me = [Security.Principal.WindowsIdentity]::GetCurrent().Name
    icacls $data /inheritance:r /grant:r "${me}:(OI)(CI)F" '*S-1-5-18:(OI)(CI)F' '*S-1-5-32-544:(OI)(CI)F' | Out-Null
    if ($LASTEXITCODE -ne 0) { throw "install.ps1: could not make $data private" }
    $listening = [Net.NetworkInformation.IPGlobalProperties]::GetIPGlobalProperties().GetActiveTcpListeners().Port
    $port = 8317..8336 | Where-Object { $listening -notcontains $_ } | Select-Object -First 1
    if (-not $port) { throw 'install.ps1: ports 8317 to 8336 are all in use; free one and run this again' }
    $client = 'sk-' + (New-Hex)
    $management = New-Hex
    $authDir = (Join-Path $data 'auth') -replace "'", "''"
    [IO.File]::WriteAllText($keys, (@(
          '# cliproxy-rs keys. Sign in to the dashboard with CLIPROXY_MANAGEMENT_KEY;'
          '# your tools send CLIPROXY_CLIENT_KEY. Keep this file private.'
          "CLIPROXY_PORT=$port"
          "CLIPROXY_CLIENT_KEY=$client"
          "CLIPROXY_MANAGEMENT_KEY=$management"
        ) -join "`n") + "`n")
    [IO.File]::WriteAllText($config, (@(
          '# Written by install.ps1. The plain keys are in keys.env next to this file.'
          'config-version: 8'
          'server:'
          '  host: "127.0.0.1"'
          "  port: $port"
          'access:'
          '  api-keys:'
          "    - `"$client`""
          'management:'
          "  secret-key: `"$management`""
          'oauth:'
          "  auth-dir: '$authDir'"
          'routing:'
          '  session-affinity: true'
        ) -join "`n") + "`n")
    Write-Host "Wrote $config, with new keys in $keys"
  }
  # server.port, or a top-level port as in older configs; 8317 when neither is set.
  $port = 8317
  $inServer = $false
  foreach ($line in Get-Content $config) {
    if ($line -match '^server:') { $inServer = $true; continue }
    if ($line -match '^[^ #]') { $inServer = $false }
    if (($inServer -or $line -match '^port:') -and $line -match '^\s*port:\s*["'']?(\d+)') { $port = [int]$Matches[1]; break }
  }

  # Direct process launch and in-process logging; no persistent shell or WMI host.
  $arguments = "--config `"$config`" --log-file `"$log`""
  Stop-Cliproxy $pidfile $dir
  $proc = Start-Process -FilePath $exe -ArgumentList $arguments -WorkingDirectory $data -WindowStyle Hidden -PassThru
  Set-Content -Path $pidfile -Value $proc.Id
  Get-ChildItem $dir -Filter 'cliproxy.prev-*.exe' | Remove-Item -Force -ErrorAction SilentlyContinue
  if ($Service) {
    # A new profile may have no Run key yet. Only create it then: New-Item -Force on an existing key
    # would replace it and drop the other programs' entries.
    if (-not (Test-Path $runKey)) { New-Item -Path $runKey | Out-Null }
    Set-ItemProperty -Path $runKey -Name 'cliproxy-rs' -Value "`"$exe`" $arguments"
    Remove-Item (Join-Path $data 'start.ps1') -Force -ErrorAction SilentlyContinue
  }

  $proc = Get-Process -Id ([int](Get-Content $pidfile)) -ErrorAction SilentlyContinue
  $up = $false
  Start-Sleep -Seconds 1
  for ($i = 0; $i -lt 20 -and -not $up; $i++) {
    try { $up = (Invoke-WebRequest -Uri "http://127.0.0.1:$port/healthz" -UseBasicParsing -TimeoutSec 2).StatusCode -eq 200 } catch {
      if (-not $proc -or $proc.HasExited) {
        $tail = Get-Content -Path $log -Tail 20 -ErrorAction SilentlyContinue | Out-String
        throw "install.ps1: cliproxy-rs stopped right after starting. The end of $log says:`n$tail"
      }
      Start-Sleep -Seconds 1
    }
  }
  if (-not $up) { throw "install.ps1: cliproxy-rs did not answer on port $port; see $log" }
  if (-not $proc -or $proc.HasExited) {
    Remove-Item $pidfile -Force
    throw "install.ps1: another program already answers on port $port, so cliproxy-rs could not start there; see $log"
  }

  $url = "http://127.0.0.1:$port/management.html"
  Write-Host ''
  Write-Host "cliproxy-rs is running at http://127.0.0.1:$port"
  Write-Host "  Dashboard  $url"
  if (Test-Path $keys) { Write-Host "  Keys       $keys (show them with: Get-Content `"$keys`")" }
  Write-Host "  Config     $config"
  Write-Host "  Log        $log"
  Write-Host "  Stop       Stop-Process -Id (Get-Content `"$pidfile`")"
  if ($Service) {
    Write-Host "It starts when you sign in to Windows. To stop that: Remove-ItemProperty $runKey -Name cliproxy-rs"
  } else {
    Write-Host 'It runs until you sign out or restart. Run this again with -Service to start it when you sign in.'
  }
  if ($env:SSH_CONNECTION) {
    Write-Host ''
    Write-Host 'No browser on this machine? Keep the server on 127.0.0.1 and reach it from your own computer'
    Write-Host 'through an SSH tunnel, then open the dashboard address there:'
    Write-Host "  ssh -L ${port}:127.0.0.1:${port} $env:USERNAME@$env:COMPUTERNAME"
    Write-Host 'Tailscale works too; see docs/MULTI-ACCOUNT.md.'
  } elseif (-not $env:CLIPROXY_NO_OPEN -and [Environment]::UserInteractive) {
    Start-Process $url
  }
  Write-Host ''
  if (Test-Path $keys) {
    Write-Host 'Next: open the dashboard, sign in with CLIPROXY_MANAGEMENT_KEY from keys.env, and choose Connect account.'
  } else {
    Write-Host 'Next: open the dashboard, sign in with your management key, and choose Connect account.'
  }
}

# Runs a download up to three times, for a brief network or server error.
function Invoke-Retry([scriptblock]$Action) {
  for ($i = 1; ; $i++) {
    try { return & $Action } catch { if ($i -ge 3) { throw } }
    Start-Sleep -Seconds (2 * $i)
  }
}

function New-Hex {
  $bytes = New-Object byte[] 24
  [Security.Cryptography.RandomNumberGenerator]::Create().GetBytes($bytes)
  -join ($bytes | ForEach-Object { $_.ToString('x2') })
}

# Also finds a direct Run-entry process: no launcher updates its pid file at login.
# ponytail: one managed server per install directory; use separate directories for multiple instances.
function Stop-Cliproxy($pidfile, $dir) {
  $directory = [IO.Path]::GetFullPath($dir).TrimEnd('\')
  $servers = Get-Process -Name 'cliproxy', 'cliproxy.prev-*' -ErrorAction SilentlyContinue | Where-Object {
    $_.Path -and [IO.Path]::GetDirectoryName($_.Path) -eq $directory -and
      ([IO.Path]::GetFileName($_.Path) -eq 'cliproxy.exe' -or [IO.Path]::GetFileName($_.Path) -like 'cliproxy.prev-*.exe')
  }
  foreach ($p in $servers) {
    Stop-Process -Id $p.Id -Force
    $p.WaitForExit(10000) | Out-Null
  }
  Remove-Item $pidfile -Force -ErrorAction SilentlyContinue
}

Install-CliproxyRs -Service $Service.IsPresent -BinaryOnly $BinaryOnly.IsPresent
