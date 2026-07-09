<#
.SYNOPSIS
  mirri-code installer for Windows (PowerShell 5.1+).

.EXAMPLE
  irm https://mirricode.com/install.ps1 | iex

.EXAMPLE
  $env:MIRRICODE_VERSION = '0.5.0'
  irm https://mirricode.com/install.ps1 | iex

.NOTES
  Optional env:
    MIRRICODE_VERSION         Explicit version; if unset, fetch latest from GitHub
    MIRRICODE_INSTALL_DIR     Installation directory, default %USERPROFILE%\.mirri-code
    MIRRICODE_NO_MODIFY_PATH  Skip PATH modification when set to a non-empty value
#>

$ErrorActionPreference = 'Stop'

# PowerShell 5.1 on older Windows may not negotiate TLS 1.2 by default.
[Net.ServicePointManager]::SecurityProtocol = [Net.ServicePointManager]::SecurityProtocol -bor [Net.SecurityProtocolType]::Tls12

$GithubRepo      = 'mirri-ai/mirricode'
$GithubApi       = "https://api.github.com/repos/$GithubRepo"
$GithubReleases  = "https://github.com/$GithubRepo/releases/download"
$NpmPackage      = '@mirri-ai/mirri-code'
$TagPrefix       = "$NpmPackage@"

$MirriVersion    = $env:MIRRICODE_VERSION
$MirriInstallDir = if ($env:MIRRICODE_INSTALL_DIR) { $env:MIRRICODE_INSTALL_DIR } else { Join-Path $env:USERPROFILE '.mirri-code' }
$MirriNoPath     = $env:MIRRICODE_NO_MODIFY_PATH

function Write-Step($msg) { Write-Host "==> $msg" -ForegroundColor Cyan }
function Die($msg)        { Write-Host "error: $msg" -ForegroundColor Red; exit 1 }

function Detect-Target {
  # PowerShell 7+ (.NET Core) uses RuntimeInformation; PowerShell 5.1 falls back to environment variables.
  $rawArch = try {
    [System.Runtime.InteropServices.RuntimeInformation]::OSArchitecture.ToString()
  } catch {
    # PowerShell 5.1: detect WOW64 (32-bit PS on 64-bit Windows) so we don't
    # misreport x64 as x86. PROCESSOR_ARCHITEW6432 is only set in WOW64.
    if ($env:PROCESSOR_ARCHITEW6432) { $env:PROCESSOR_ARCHITEW6432 } else { $env:PROCESSOR_ARCHITECTURE }
  }

  $arch = switch ($rawArch) {
    'X64'     { 'x64' }
    'X86'     { 'x86' }
    'Arm64'   { 'arm64' }
    'ARM64'   { 'arm64' }
    'AMD64'   { 'x64' }
    'IA64'    { 'ia64' }
    default   { Die "unsupported architecture: $rawArch" }
  }

  return "win32-$arch"
}

function Test-Sha256([string]$file, [string]$expected) {
  $actual = (Get-FileHash $file -Algorithm SHA256).Hash.ToLower()
  if ($actual -ne $expected.ToLower()) {
    Die "checksum mismatch: expected $expected, got $actual"
  }
}

function Add-ToUserPath([string]$dir) {
  if ($MirriNoPath) { Write-Step "Skipping PATH update (MIRRICODE_NO_MODIFY_PATH set)"; return }
  $current = [Environment]::GetEnvironmentVariable('Path', 'User')
  if ($current -and ($current.Split(';') -contains $dir)) {
    Write-Step "$dir already in user PATH"
    return
  }
  $newPath = if ($current) { "$dir;$current" } else { $dir }
  [Environment]::SetEnvironmentVariable('Path', $newPath, 'User')
  Write-Step "Added $dir to user PATH (open a new terminal for it to take effect)"
}

# ---------- main ----------

try {

$target = Detect-Target
Write-Step "Detected target: $target"

# 1. Version
if ($MirriVersion) {
  $version = $MirriVersion
  Write-Step "Using pinned version $version"
} else {
  Write-Step "Resolving latest version from GitHub"
  $apiUrl = "$GithubApi/releases/latest"
  $response = Invoke-RestMethod -Uri $apiUrl
  $tag = $response.tag_name
  if (-not $tag) { Die "could not resolve latest release from GitHub" }
  # Strip the @mirri-ai/mirri-code@ prefix to get the raw semver
  if ($tag.StartsWith($TagPrefix)) {
    $version = $tag.Substring($TagPrefix.Length)
  } elseif ($tag.StartsWith('v')) {
    $version = $tag.Substring(1)
  } else {
    $version = $tag
  }
  if (-not $version) { Die "could not parse version from tag: $tag" }
  Write-Step "Latest version: $version"
}

# 2. Manifest
$tagRef = "$TagPrefix$version"
$manifestUrl = "$GithubReleases/$tagRef/manifest.json"
Write-Step "Fetching manifest $manifestUrl"
$manifest = Invoke-RestMethod -Uri $manifestUrl
$entry = $manifest.platforms.$target
if (-not $entry) { Die "platform $target not found in manifest" }
$filename = $entry.filename
$checksum = $entry.checksum
if ($checksum -notmatch '^[a-f0-9]{64}$') { Die "invalid checksum for ${target}: $checksum" }

# 3. Download binary
$tmp = Join-Path $env:TEMP ([guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $binaryUrl = "$GithubReleases/$tagRef/$filename"
  $tmpBinary = Join-Path $tmp $filename
  Write-Step "Downloading $binaryUrl"
  Invoke-WebRequest -Uri $binaryUrl -OutFile $tmpBinary

  Write-Step "Verifying checksum"
  Test-Sha256 $tmpBinary $checksum

  # 4. Install
  $binDir = Join-Path $MirriInstallDir 'bin'
  New-Item -ItemType Directory -Path $binDir -Force | Out-Null
  $binaryDest = Join-Path $binDir 'mirri.exe'
  if (Test-Path $binaryDest) {
    $backup = "$binaryDest.bak"
    if (Test-Path $backup) {
      try {
        Remove-Item $backup -Force -ErrorAction Stop
      } catch {
        # File is locked by a running mirri process; use a unique backup name so
        # the install can proceed. The locked .bak is released when mirri exits.
        $backup = "$binaryDest.$([guid]::NewGuid().ToString('N').Substring(0,8)).bak"
      }
    }
    # Windows allows renaming a running .exe but not overwriting it, so move the old one first and then copy the new one.
    Move-Item $binaryDest $backup -Force
    Write-Step "Backed up existing mirri.exe to $([System.IO.Path]::GetFileName($backup))"
  }
  Copy-Item $tmpBinary $binaryDest -Force
  Write-Step "Installed to $binaryDest"

  # 5. PATH
  Add-ToUserPath $binDir

  Write-Step "Done. Open a new terminal and run: mirri --version"
} finally {
  Remove-Item $tmp -Recurse -Force -ErrorAction SilentlyContinue
}

} catch {
  $err = $_
  [Console]::Error.WriteLine("")
  [Console]::Error.WriteLine("================ DEBUG: FULL ERROR ================")
  [Console]::Error.WriteLine("ExceptionType         : $($err.Exception.GetType().FullName)")
  [Console]::Error.WriteLine("Message               : $($err.Exception.Message)")
  if ($err.Exception.InnerException) {
    [Console]::Error.WriteLine("InnerException        : $($err.Exception.InnerException.Message)")
  }
  [Console]::Error.WriteLine("FullyQualifiedErrorId : $($err.FullyQualifiedErrorId)")
  [Console]::Error.WriteLine("CategoryInfo          : $($err.CategoryInfo)")
  if ($err.InvocationInfo) {
    [Console]::Error.WriteLine("Line                  : $($err.InvocationInfo.Line)")
    [Console]::Error.WriteLine("PositionMessage       : $($err.InvocationInfo.PositionMessage.Trim())")
  }
  if ($err.ScriptStackTrace) {
    [Console]::Error.WriteLine("ScriptStackTrace:")
    [Console]::Error.WriteLine($err.ScriptStackTrace)
  }
  [Console]::Error.WriteLine("===================================================")
  [Console]::Error.WriteLine("")
  [Console]::Error.WriteLine("Installation failed.")
  try { Read-Host "Press Enter to exit" } catch {}
  exit 1
}
