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
    # Windows cannot replace a running .exe, so the server stops first (a full run starts it again).
    if (-not $BinaryOnly) { Stop-Cliproxy $pidfile }
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    try { Copy-Item (Join-Path $tmp "$name\cliproxy.exe") $exe -Force } catch { throw "install.ps1: could not replace $exe; if cliproxy is running, stop it and run this again" }
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

  # The start command, as a script so the sign-in entry can run the same thing. Win32_Process.Create
  # starts the server outside this window: its parent is the WMI host, it gets its own hidden console
  # and none of this shell's handles, so closing the window (or a CI step ending) leaves it running.
  # cmd.exe only appends the server's output and errors to cliproxy.log.
  $q = { param($s) "'" + ($s -replace "'", "''") + "'" }
  $command = 'cmd.exe /d /c ""' + $exe + '" --config "' + $config + '" >> "' + $log + '" 2>&1"'
  $start = @'
# Starts cliproxy-rs in the background, outside this window. Written by install.ps1.
$startup = New-CimInstance -ClassName Win32_ProcessStartup -ClientOnly -Property @{ ShowWindow = [uint16]0 }
$r = Invoke-CimMethod -ClassName Win32_Process -MethodName Create -Arguments @{ CommandLine = @@COMMAND@@; CurrentDirectory = @@DATA@@; ProcessStartupInformation = $startup }
if ($r.ReturnValue -ne 0) { throw "cliproxy-rs could not be started: Win32_Process.Create returned $($r.ReturnValue)" }
# The server is that cmd.exe's child. It may be gone already if its config is broken.
$server = $null
for ($i = 0; $i -lt 30 -and -not $server; $i++) {
  Start-Sleep -Milliseconds 100
  $server = Get-CimInstance Win32_Process -Filter "ParentProcessId = $($r.ProcessId) AND Name = 'cliproxy.exe'"
}
Set-Content -Path @@PIDFILE@@ -Value $(if ($server) { $server.ProcessId } else { $r.ProcessId })
'@
  $start = $start.Replace('@@COMMAND@@', (& $q $command)).Replace('@@DATA@@', (& $q $data)).Replace('@@PIDFILE@@', (& $q $pidfile))
  Stop-Cliproxy $pidfile
  & ([scriptblock]::Create($start))
  if ($Service) {
    $startFile = Join-Path $data 'start.ps1'
    [IO.File]::WriteAllText($startFile, $start + "`n")
    # A new profile may have no Run key yet. Only create it then: New-Item -Force on an existing key
    # would replace it and drop the other programs' entries.
    if (-not (Test-Path $runKey)) { New-Item -Path $runKey | Out-Null }
    Set-ItemProperty -Path $runKey -Name 'cliproxy-rs' -Value "powershell.exe -NoProfile -ExecutionPolicy Bypass -WindowStyle Hidden -File `"$startFile`""
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

# Stops the server an earlier run started, if it is still running.
function Stop-Cliproxy($pidfile) {
  if (-not (Test-Path $pidfile)) { return }
  $p = Get-Process -Id ([int](Get-Content $pidfile)) -ErrorAction SilentlyContinue
  if ($p -and $p.ProcessName -eq 'cliproxy') {
    Stop-Process -Id $p.Id -Force
    $p.WaitForExit(10000) | Out-Null
  }
  Remove-Item $pidfile -Force
}

Install-CliproxyRs -Service $Service.IsPresent -BinaryOnly $BinaryOnly.IsPresent
