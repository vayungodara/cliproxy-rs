# Installs cliproxy-rs on Windows, sets it up and starts it. In PowerShell:
#
#   irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1 | iex
#
# The first run downloads the Windows release, checks it against the release's SHA256SUMS
# and installs %LOCALAPPDATA%\Programs\cliproxy-rs\cliproxy.exe. It writes
# %USERPROFILE%\.cliproxy-rs\config.yaml with a free port (8317, or the next free one) and
# new keys, which it saves in keys.env next to it and never prints. Then it starts the
# server in the background and checks that it answers. Running it again upgrades the
# binary and restarts the server; if the new version does not answer, the previous binary
# is put back and restarted. An existing config.yaml or keys.env is never changed.
#
# Options, passed like this:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/vayungodara/cliproxy-rs/master/install.ps1))) -Service
#   -Service     also start cliproxy-rs when you sign in to Windows
#   -BinaryOnly  only install or upgrade the binary; a running server keeps running
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
  # Absolute paths: they go into the Run entry and identify this install's server process.
  $dir = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($dir)
  $data = $ExecutionContext.SessionState.Path.GetUnresolvedProviderPathFromPSPath($data)
  $exe = Join-Path $dir 'cliproxy.exe'
  $staged = Join-Path $dir 'cliproxy.new.exe'
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

  # Stage the new image next to the old one and check it before anything existing changes.
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
    New-Item -ItemType Directory -Force -Path $dir | Out-Null
    Copy-Item -LiteralPath (Join-Path $tmp "$name\cliproxy.exe") -Destination $staged -Force
  } finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
  }

  $pidfile = Join-Path $data 'cliproxy.pid'
  $config = Join-Path $data 'config.yaml'
  $keys = Join-Path $data 'keys.env'
  $log = Join-Path $data 'cliproxy.log'
  try {
    $help = Get-CliproxyHelp $staged
    if ($help -notmatch '--log-file' -or $help -notmatch '--working-dir') {
      throw "install.ps1: release $tag is too old for this installer (its cliproxy.exe has no --log-file or --working-dir option); install a newer release, or unset CLIPROXY_VERSION for the latest. Nothing was changed."
    }
    $version = & $staged --version | Select-Object -Last 1
    if (-not $BinaryOnly) { $probe = Initialize-CliproxyHome $data $config $keys }
  } catch {
    Remove-Item -LiteralPath $staged -Force -ErrorAction SilentlyContinue
    throw
  }

  # The previous image's flags decide how a rollback restarts it.
  $previousArguments = $null
  if (Test-Path -LiteralPath $exe) {
    $previousHelp = ''
    # An image that cannot print its help is restarted, if needed, with --config only.
    try { $previousHelp = Get-CliproxyHelp $exe } catch { $previousHelp = '' }
    $previousArguments = New-CliproxyArguments $previousHelp $config $log $data
  }
  # Windows permits renaming a running image, so a running server keeps serving from
  # this run's cliproxy.prev-<guid>.exe until it is stopped.
  $previous = $null
  if ($previousArguments) {
    $previous = Join-Path $dir ("cliproxy.prev-" + [guid]::NewGuid() + '.exe')
    [IO.File]::Move($exe, $previous)
  }
  try { [IO.File]::Move($staged, $exe) } catch {
    $why = $_.Exception.Message
    if ($previous) { [IO.File]::Move($previous, $exe) }
    throw "install.ps1: could not put the new cliproxy.exe in place ($why); the previous one is unchanged"
  }
  # Older images from earlier runs, unless a server still runs from them (then they are locked).
  # -BinaryOnly is done here, and its running server keeps its image locked.
  Remove-CliproxyImages $dir $(if ($BinaryOnly) { $null } else { $previous })
  Write-Host "Installed $version at $exe"
  if (($env:Path -split ';') -notcontains $dir) { Write-Host "Add $dir to your PATH to run cliproxy by name." }
  if ($BinaryOnly) { return }

  # Direct process launch and in-process logging; no persistent shell or WMI host.
  $arguments = New-CliproxyArguments $help $config $log $data
  try { Stop-Cliproxy $pidfile $dir $config } catch {
    $why = $_.Exception.Message
    if ($previous) { Restore-CliproxyImage $exe $previous }
    throw "install.ps1: could not stop the running cliproxy-rs ($why); the previous binary is back in place"
  }
  $proc = $null
  $problem = $null
  try {
    $proc = Start-Cliproxy $exe $arguments $data $pidfile
    $problem = Wait-Cliproxy $proc $probe
  } catch { $problem = "cliproxy-rs could not be started ($($_.Exception.Message))" }
  if ($problem) {
    $tail = Get-Content -LiteralPath $log -Tail 20 -ErrorAction SilentlyContinue | Out-String
    $stopped = $true
    if ($proc) { try { Stop-CliproxyId $proc.Id } catch { $stopped = $false } }
    Remove-Item -LiteralPath $pidfile -Force -ErrorAction SilentlyContinue
    if (-not $previous) { throw "install.ps1: $problem. The end of $log says:`n$tail" }
    $rollback = $null
    try {
      if (-not $stopped) { throw "the new process $($proc.Id) did not stop" }
      Restore-CliproxyImage $exe $previous
      $old = $null
      try {
        $old = Start-Cliproxy $exe $previousArguments $data $pidfile
        $oldProblem = Wait-Cliproxy $old $probe
      } catch { $oldProblem = "it could not be started ($($_.Exception.Message))" }
      if ($oldProblem) {
        if ($old) { Stop-CliproxyId $old.Id }
        Remove-Item -LiteralPath $pidfile -Force -ErrorAction SilentlyContinue
        throw "the restored previous version failed too: $oldProblem"
      }
    } catch { $rollback = $_.Exception.Message }
    if ($rollback) {
      throw "install.ps1: the upgrade failed: $problem. ROLLBACK FAILED: $rollback. cliproxy-rs may not be running; see $log. The end of $log said:`n$tail"
    }
    throw "install.ps1: the upgrade failed: $problem. Rolled back: the previous binary is in place and answers again at $($probe.Base). The end of $log said:`n$tail"
  }

  # Only a healthy new server replaces the sign-in entry or this run's previous image.
  if ($Service) {
    # A new profile may have no Run key yet. Only create it then: New-Item -Force on an existing key
    # would replace it and drop the other programs' entries.
    if (-not (Test-Path $runKey)) { New-Item -Path $runKey | Out-Null }
    Set-ItemProperty -Path $runKey -Name 'cliproxy-rs' -Value "`"$exe`" $arguments"
    Remove-Item (Join-Path $data 'start.ps1') -Force -ErrorAction SilentlyContinue
  }
  Remove-CliproxyImages $dir $null

  $base = $probe.Base
  $url = "$base/management.html"
  $configText = $config -replace "'", "''"
  Write-Host ''
  Write-Host "cliproxy-rs is running at $base"
  Write-Host "  Dashboard  $url"
  if (Test-Path $keys) { Write-Host "  Keys       $keys (show them with: Get-Content `"$keys`")" }
  Write-Host "  Config     $config"
  Write-Host "  Log        $log"
  Write-Host "  Stop       Get-CimInstance Win32_Process -Filter `"Name LIKE 'cliproxy%.exe'`" | Where-Object { `$_.CommandLine -and `$_.CommandLine.Contains('$configText') } | ForEach-Object { Stop-Process -Id `$_.ProcessId }"
  if ($Service) {
    Write-Host "It starts when you sign in to Windows. To stop that: Remove-ItemProperty $runKey -Name cliproxy-rs"
  } else {
    Write-Host 'It runs until you sign out or restart. Run this again with -Service to start it when you sign in.'
  }
  if ($env:SSH_CONNECTION) {
    $port = $probe.Port
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

# Writes config.yaml and keys.env on a first run, then returns where the server answers.
function Initialize-CliproxyHome($data, $config, $keys) {
  if (-not (Test-Path -LiteralPath $config)) {
    if (Test-Path -LiteralPath $keys) { throw "install.ps1: $keys exists but $config does not; restore config.yaml, or move keys.env away to make new keys" }
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
  Get-CliproxyProbe $config
}

function Get-CliproxyHelp($image) {
  try { $text = & $image --help | Out-String } catch { throw "install.ps1: $image does not run ($($_.Exception.Message)); nothing was changed" }
  if ($LASTEXITCODE -ne 0) { throw "install.ps1: $image --help failed with exit code $LASTEXITCODE; nothing was changed" }
  $text
}

# A quoted Windows argument; a trailing backslash would otherwise escape the closing quote.
function ConvertTo-Argument([string]$value) { '"' + ($value -replace '(\\+)$', '$1$1') + '"' }

# The same options for the direct start and the Run entry. An older previous image restarted
# by a rollback gets only the options its --help lists.
function New-CliproxyArguments($help, $config, $log, $data) {
  $arguments = "--config $(ConvertTo-Argument $config)"
  if ($help -match '--log-file') { $arguments += " --log-file $(ConvertTo-Argument $log)" }
  if ($help -match '--working-dir') { $arguments += " --working-dir $(ConvertTo-Argument $data)" }
  $arguments
}

function Start-Cliproxy($image, $arguments, $data, $pidfile) {
  $proc = Start-Process -FilePath $image -ArgumentList $arguments -WorkingDirectory $data -WindowStyle Hidden -PassThru
  $null = $proc.Handle # keeps ExitCode readable after the process ends
  Set-Content -LiteralPath $pidfile -Value $proc.Id
  $proc
}

# Address, port and scheme from server.host/port/tls.enable, or the older top-level
# host/port/tls keys. A wildcard host is probed on 127.0.0.1.
# ponytail: block-style YAML only; a flow mapping such as `server: {port: 9000}` falls back to
# the defaults. Parse with the binary itself if configs start using that form.
function Get-CliproxyProbe($config) {
  $values = @{}
  $stack = New-Object System.Collections.ArrayList
  foreach ($line in Get-Content -LiteralPath $config) {
    if ($line -notmatch '^( *)([A-Za-z0-9_.-]+) *:(.*)$') { continue }
    $indent = $Matches[1].Length
    $key = $Matches[2]
    $value = ($Matches[3] -replace '(^|\s+)#.*$', '').Trim()
    while ($stack.Count -and $stack[$stack.Count - 1].Indent -ge $indent) { $stack.RemoveAt($stack.Count - 1) }
    $path = (@($stack | ForEach-Object { $_.Key }) + $key) -join '.'
    if ($value -eq '') { [void]$stack.Add([pscustomobject]@{ Indent = $indent; Key = $key }) }
    if ($value -match '^"(.*)"$' -or $value -match "^'(.*)'$") { $value = $Matches[1] }
    if (-not $values.ContainsKey($path)) { $values[$path] = $value }
  }
  $pick = { param($new, $old) if ($values.ContainsKey($new)) { $values[$new] } elseif ($values.ContainsKey($old)) { $values[$old] } else { $null } }
  $address = [string](& $pick 'server.host' 'host')
  $address = $address.Trim().TrimStart('[').TrimEnd(']')
  if ($address -in '', '0.0.0.0', '::') { $address = '127.0.0.1' }
  $port = 0
  if (-not [int]::TryParse([string](& $pick 'server.port' 'port'), [ref]$port) -or $port -le 0) { $port = 8317 }
  $tls = [string](& $pick 'server.tls.enable' 'tls.enable') -match '^(?i:true)$'
  $urlHost = if ($address.Contains(':')) { "[$address]" } else { $address }
  $scheme = if ($tls) { 'https' } else { 'http' }
  [pscustomobject]@{ Address = $address; Port = $port; Tls = $tls; Base = "${scheme}://${urlHost}:$port" }
}

# True when the server answers: GET /healthz over HTTP, or a TCP connect with TLS on, because
# Windows PowerShell 5.1 cannot skip certificate checks for one request.
function Test-CliproxyUp($probe) {
  if (-not $probe.Tls) {
    try { return (Invoke-WebRequest -Uri "$($probe.Base)/healthz" -UseBasicParsing -TimeoutSec 2).StatusCode -eq 200 } catch { return $false }
  }
  $ip = $null
  $client = if ([Net.IPAddress]::TryParse($probe.Address, [ref]$ip)) { New-Object Net.Sockets.TcpClient($ip.AddressFamily) } else { New-Object Net.Sockets.TcpClient }
  try { return $client.ConnectAsync($probe.Address, $probe.Port).Wait(2000) -and $client.Connected } catch { return $false } finally { $client.Dispose() }
}

# $null once the process answers and is still alive after about 20 s at most; otherwise why not.
function Wait-Cliproxy($proc, $probe) {
  Start-Sleep -Seconds 1
  for ($i = 0; $i -lt 20; $i++) {
    if (Test-CliproxyUp $probe) {
      # A server that lost the port to another program exits right after binding fails.
      Start-Sleep -Seconds 1
      if ($proc.HasExited) { return "another program already answers at $($probe.Base), so cliproxy-rs could not start there" }
      return $null
    }
    if ($proc.HasExited) { return "cliproxy-rs stopped right after starting (exit code $($proc.ExitCode))" }
    Start-Sleep -Seconds 1
  }
  "cliproxy-rs did not answer at $($probe.Base)"
}

function Stop-CliproxyId([int]$id) {
  $p = Get-Process -Id $id -ErrorAction SilentlyContinue
  if (-not $p) { return }
  try { Stop-Process -Id $id -Force } catch { if (Get-Process -Id $id -ErrorAction SilentlyContinue) { throw } }
  if (-not $p.WaitForExit(10000)) { throw "install.ps1: cliproxy-rs process $id did not stop" }
}

# Puts this run's previous image back as cliproxy.exe; the failed one is removed when unlocked.
function Restore-CliproxyImage($exe, $previous) {
  $failed = Join-Path ([IO.Path]::GetDirectoryName($exe)) ("cliproxy.prev-" + [guid]::NewGuid() + '.exe')
  [IO.File]::Move($exe, $failed)
  [IO.File]::Move($previous, $exe)
  Remove-Item -LiteralPath $failed -Force -ErrorAction SilentlyContinue
}

# Removes previous images no process holds open, except $keep.
function Remove-CliproxyImages($dir, $keep) {
  Get-ChildItem -LiteralPath $dir -Filter 'cliproxy.prev-*.exe' | Where-Object { $_.FullName -ne $keep } |
    ForEach-Object { Remove-Item -LiteralPath $_.FullName -Force -ErrorAction SilentlyContinue }
}

# Stops this install's server: a cliproxy image from the install directory started with this
# config, which also finds a Run-entry launch the pid file does not know, plus the pid file's
# process if it is a cliproxy image. A server for another config sharing the binary keeps running.
function Stop-Cliproxy($pidfile, $dir, $config) {
  $directory = $dir.TrimEnd('\')
  $recorded = 0
  $text = Get-Content -LiteralPath $pidfile -ErrorAction SilentlyContinue | Select-Object -First 1
  if (-not $text -or -not [int]::TryParse($text.Trim(), [ref]$recorded)) { $recorded = 0 }
  try { $all = @(Get-CimInstance Win32_Process -Filter "Name LIKE 'cliproxy%.exe'") }
  catch { throw "install.ps1: could not list running processes ($($_.Exception.Message))" }
  $servers = $all | Where-Object {
    ($_.Name -eq 'cliproxy.exe' -or $_.Name -like 'cliproxy.prev-*.exe') -and (
      ($recorded -gt 0 -and $_.ProcessId -eq $recorded) -or
      ($_.ExecutablePath -and [IO.Path]::GetDirectoryName($_.ExecutablePath) -eq $directory -and
        $_.CommandLine -and $_.CommandLine.IndexOf($config, [StringComparison]::OrdinalIgnoreCase) -ge 0))
  }
  foreach ($p in $servers) { Stop-CliproxyId $p.ProcessId }
  Remove-Item -LiteralPath $pidfile -Force -ErrorAction SilentlyContinue
}

Install-CliproxyRs -Service $Service.IsPresent -BinaryOnly $BinaryOnly.IsPresent
