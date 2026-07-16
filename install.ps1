#Requires -Version 5.1
<#
audible-rs installer — download a prebuilt `audible.exe` from GitHub Releases
and install it. PowerShell analog of install.sh; uses only built-in cmdlets
(no curl/tar), so it runs on any stock Windows 10/11.

  PowerShell:      irm https://raw.githubusercontent.com/mkb79/audible-rs/main/install.ps1 | iex
  Command Prompt:  powershell -c "irm https://raw.githubusercontent.com/mkb79/audible-rs/main/install.ps1 | iex"

Options (parameter, or environment variable for the piped one-liner, which
cannot take parameters):
  -Version <tag>   AUDIBLE_VERSION          install a specific release (default: latest stable)
  -Pre             AUDIBLE_PRERELEASE=1      install the newest release, pre-releases included
  -BinDir <dir>    AUDIBLE_INSTALL_DIR       install location (default: %LOCALAPPDATA%\Programs\audible-rs)
  -Force           AUDIBLE_FORCE=1           replace an existing non-audible-rs 'audible' without asking
  -NoModifyPath    AUDIBLE_NO_MODIFY_PATH=1  do not add the install dir to your user PATH
  -NoCompletions   AUDIBLE_NO_COMPLETIONS=1  do not set up PowerShell tab completion

By default the newest stable release is installed. While none exists yet
(pre-alpha) it falls back to the newest pre-release; once a stable release is
out, use -Pre to keep tracking pre-releases.

audible-rs is the successor to audible-cli and shares the command name
'audible'. Installing over a different 'audible' asks first unless -Force; the
config directories are separate, so audible-cli's data is left untouched.

No administrator rights are needed: it installs under your user profile and
edits only your user PATH and (for tab completion) your PowerShell profile.
Integrity: the download is verified against the release's SHA256SUMS over HTTPS.
#>
[CmdletBinding()]
param(
    [string] $Version,
    [switch] $Pre,
    [string] $BinDir,
    [switch] $Force,
    [switch] $NoModifyPath,
    [switch] $NoCompletions
)

# Stop on any error; skip Invoke-WebRequest's (slow) progress bar; force TLS 1.2
# for Windows PowerShell 5.1, whose default can still be TLS 1.0 (GitHub needs
# 1.2+). Errors `throw` rather than `exit`, so a failed `irm | iex` never closes
# the caller's shell.
$ErrorActionPreference = 'Stop'
$ProgressPreference    = 'SilentlyContinue'
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12

$Repo = 'mkb79/audible-rs'
$Bin  = 'audible'

# Environment-variable fallbacks, so the piped one-liner is still configurable.
if (-not $Version      -and $env:AUDIBLE_VERSION)                { $Version = $env:AUDIBLE_VERSION }
if (-not $Pre          -and $env:AUDIBLE_PRERELEASE -eq '1')     { $Pre = $true }
if (-not $BinDir       -and $env:AUDIBLE_INSTALL_DIR)            { $BinDir = $env:AUDIBLE_INSTALL_DIR }
if (-not $Force        -and $env:AUDIBLE_FORCE -eq '1')          { $Force = $true }
if (-not $NoModifyPath  -and $env:AUDIBLE_NO_MODIFY_PATH -eq '1') { $NoModifyPath = $true }
if (-not $NoCompletions -and $env:AUDIBLE_NO_COMPLETIONS -eq '1') { $NoCompletions = $true }

if (-not $BinDir) { $BinDir = Join-Path $env:LOCALAPPDATA 'Programs\audible-rs' }

function Info($msg) { Write-Host $msg }
function Fail($msg) { throw "error: $msg" }

$headers = @{ 'User-Agent' = 'audible-rs-install' }

# --- resolve the version --------------------------------------------------
# /releases/latest = newest *stable* (pre-releases excluded); /releases = every
# release, newest first. Default to stable; -Pre (or no stable yet) tracks pre.
if (-not $Version) {
    $api = "https://api.github.com/repos/$Repo"
    try {
        if ($Pre) {
            Info 'resolving the newest release (pre-releases included)...'
            $Version = (Invoke-RestMethod "$api/releases" -Headers $headers)[0].tag_name
        }
        else {
            Info 'resolving the latest stable release...'
            try { $Version = (Invoke-RestMethod "$api/releases/latest" -Headers $headers).tag_name }
            catch { $Version = $null }
            if (-not $Version) {
                Info 'no stable release yet - installing the newest pre-release (use -Pre to keep tracking pre-releases)'
                $Version = (Invoke-RestMethod "$api/releases" -Headers $headers)[0].tag_name
            }
        }
    }
    catch { Fail "could not reach the GitHub API: $($_.Exception.Message)" }
    if (-not $Version) { Fail 'could not determine a release to install (pass -Version <tag>)' }
}

$num     = $Version -replace '^v', ''
$target  = 'x86_64-pc-windows-msvc'   # the only Windows asset; runs on arm64 via emulation
$archive = "$Bin-$num-$target.zip"
$baseUrl = "https://github.com/$Repo/releases/download/$Version"
$dest    = Join-Path $BinDir "$Bin.exe"

Info "installing $Bin $Version ($target) into $BinDir"

# --- guard: an audible-rs upgrade vs replacing a different 'audible' -------
if (Test-Path -LiteralPath $dest) {
    $old = $null
    try { $old = (& $dest --version 2>$null | Select-Object -First 1) } catch { }
    if ($old -match '^audible\s+(\S+)$') {
        if ($Matches[1] -eq $num) { Info "audible-rs $num is already installed - reinstalling" }
        else { Info "upgrading audible-rs $($Matches[1]) -> $num" }
    }
    else {
        Info "warning: a different '$Bin' already exists at $dest."
        if (-not $Force) {
            if (-not [Environment]::UserInteractive) {
                Fail 'aborted (non-interactive) - re-run with -Force, or -BinDir <dir> to install elsewhere'
            }
            $ans = Read-Host 'replace it? [y/N]'
            if ($ans -notmatch '^(y|Y|yes|Yes)$') {
                Fail 'aborted - re-run with -Force, or -BinDir <dir> to install elsewhere'
            }
        }
    }
}

# --- download to a temp dir, verify, install ------------------------------
$tmp = Join-Path ([IO.Path]::GetTempPath()) ('audible-install-' + [Guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $tmp -Force | Out-Null
try {
    $zip = Join-Path $tmp $archive
    Info "downloading $archive..."
    try {
        Invoke-WebRequest "$baseUrl/$archive"   -OutFile $zip -Headers $headers
        Invoke-WebRequest "$baseUrl/SHA256SUMS" -OutFile (Join-Path $tmp 'SHA256SUMS') -Headers $headers
    }
    catch { Fail "download failed: $($_.Exception.Message)" }

    # Verify the SHA256 against the release's own SHA256SUMS (line: `<hash>  <file>`).
    $expected = $null
    foreach ($line in Get-Content (Join-Path $tmp 'SHA256SUMS')) {
        $parts = $line -split '\s+'
        if ($parts.Count -ge 2 -and $parts[1] -eq $archive) { $expected = $parts[0].ToLower(); break }
    }
    if (-not $expected) { Fail "no checksum for $archive in SHA256SUMS" }
    $actual = (Get-FileHash -LiteralPath $zip -Algorithm SHA256).Hash.ToLower()
    if ($actual -ne $expected) { Fail "checksum mismatch for $archive" }
    Info 'checksum verified'

    Expand-Archive -LiteralPath $zip -DestinationPath $tmp -Force
    $stage = "$Bin-$num-$target"
    $src   = Join-Path (Join-Path $tmp $stage) "$Bin.exe"
    if (-not (Test-Path -LiteralPath $src)) { Fail "unexpected archive layout ($stage\$Bin.exe missing)" }
    New-Item -ItemType Directory -Path $BinDir -Force | Out-Null
    Copy-Item -LiteralPath $src -Destination $dest -Force
    Info "installed $dest"
}
finally { Remove-Item -LiteralPath $tmp -Recurse -Force -ErrorAction SilentlyContinue }

# --- PATH ------------------------------------------------------------------
# Windows has no ~/.local/bin convention and editing PATH by hand is a chore,
# so by default we add the install dir to the *user* PATH (no admin needed).
$binDirTrim = $BinDir.TrimEnd('\')
if (-not $NoModifyPath) {
    $userPath = [Environment]::GetEnvironmentVariable('Path', 'User')
    $entries  = @()
    if ($userPath) { $entries = @($userPath -split ';' | Where-Object { $_ -ne '' }) }
    if (-not ($entries | Where-Object { $_.TrimEnd('\') -eq $binDirTrim })) {
        [Environment]::SetEnvironmentVariable('Path', ((@($entries) + $BinDir) -join ';'), 'User')
        $env:Path = "$env:Path;$BinDir"   # reflect it in the current session too
        Info ''
        Info "added $BinDir to your user PATH - restart your shell (or open a new one) to pick it up."
    }
}
elseif (-not (@($env:Path -split ';') | Where-Object { $_.TrimEnd('\') -eq $binDirTrim })) {
    Info ''
    Info "note: $BinDir is not on your PATH. Add it, or run $dest directly."
}

# Another 'audible' earlier on PATH would shadow the one just installed.
$resolved = Get-Command $Bin -CommandType Application -ErrorAction SilentlyContinue | Select-Object -First 1
if ($resolved -and $resolved.Source -ne $dest) {
    Info ''
    Info "note: '$($resolved.Source)' comes earlier on your PATH and will run instead of $dest."
}

# --- tab completion (PowerShell) ------------------------------------------
# On by default (like the user PATH above); -NoCompletions skips it. Write the
# generated completion next to the exe and dot-source it from the profile,
# creating the profile *and its directory* when missing — a fresh Windows has
# neither, which is why editing $PROFILE by hand fails with "path not found".
if (-not $NoCompletions) {
    try {
        $completionFile = Join-Path $BinDir 'audible.completion.ps1'
        & $dest completions powershell | Out-File -FilePath $completionFile -Encoding utf8
        $profileDir = Split-Path -Parent $PROFILE
        if ($profileDir -and -not (Test-Path -LiteralPath $profileDir)) {
            New-Item -ItemType Directory -Path $profileDir -Force | Out-Null
        }
        $marker  = '# audible-rs tab completion'
        $already = (Test-Path -LiteralPath $PROFILE) -and
                   (Select-String -LiteralPath $PROFILE -SimpleMatch $marker -Quiet)
        if (-not $already) {
            # Guarded, so a later uninstall (file gone) does not error at startup.
            Add-Content -LiteralPath $PROFILE -Value @(
                '',
                $marker,
                "if (Test-Path -LiteralPath '$completionFile') { . '$completionFile' }"
            )
        }
        Info ''
        Info 'tab completion installed - open a new PowerShell to use it (skip next time with -NoCompletions).'
    }
    catch {
        Info ''
        Info "note: could not set up tab completion ($($_.Exception.Message))."
        Info '  do it manually: audible completions powershell | Out-String | Invoke-Expression'
    }
}

# --- optional decrypt tools -----------------------------------------------
Info ''
Info "Optional: 'audible download --decrypt' needs one of:"
Info '  * ffmpeg (>= 4.4)  -  winget install ffmpeg  (or a gyan.dev build), or'
Info '  * aaxclean-cli by Mbucari (faster): https://github.com/Mbucari/aaxclean-cli'
Info "Point at a specific binary with  `$env:AUDIBLE_FFMPEG / `$env:AUDIBLE_AAXCLEAN_CLI."
