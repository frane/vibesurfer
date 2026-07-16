# vibesurfer (`vs`) installer for Windows PowerShell.
#
# Downloads the latest release binary for this machine, or falls back
# to `cargo install --git` if a Rust toolchain is on PATH.
#
# Usage:
#   irm https://raw.githubusercontent.com/frane/vibesurfer/main/install.ps1 | iex
#   # pin a version / bin dir:
#   & ([scriptblock]::Create((irm https://raw.githubusercontent.com/frane/vibesurfer/main/install.ps1))) -Version v0.1.26 -BinDir $HOME\bin

[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$BinDir = ""
)

$ErrorActionPreference = "Stop"
$Repo = "frane/vibesurfer"
$BinName = "vs.exe"

function Say($m) { Write-Host "vibesurfer-install: $m" }
function Fail($m) { Write-Error "vibesurfer-install: $m"; exit 1 }

# Resolve the release triple. Only x86_64 msvc is published today;
# arm64 Windows falls through to the cargo path below.
$arch = $env:PROCESSOR_ARCHITECTURE
switch ($arch) {
    "AMD64" { $target = "x86_64-pc-windows-msvc" }
    default { $target = $null; Say "no prebuilt binary for arch $arch; will try cargo" }
}

# Pick a bin dir: explicit, else the first existing entry on PATH under
# the user profile, else create $HOME\bin.
if (-not $BinDir) {
    $candidate = "$HOME\bin", "$HOME\.local\bin" |
        Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $candidate) { $candidate = "$HOME\bin" }
    $BinDir = $candidate
}
if (-not (Test-Path $BinDir)) { New-Item -ItemType Directory -Force -Path $BinDir | Out-Null }

function Install-FromRelease {
    if (-not $target) { return $false }
    $ver = $Version
    if ($ver -eq "latest") {
        $rel = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
        $ver = $rel.tag_name
        if (-not $ver) { return $false }
    }
    $asset = "vs-$ver-$target.zip"
    $url = "https://github.com/$Repo/releases/download/$ver/$asset"
    Say "trying release: $url"
    $tmp = Join-Path $env:TEMP ("vs-install-" + [System.Guid]::NewGuid().ToString("N"))
    New-Item -ItemType Directory -Force -Path $tmp | Out-Null
    try {
        $zip = Join-Path $tmp $asset
        Invoke-WebRequest -Uri $url -OutFile $zip -UseBasicParsing
        Expand-Archive -Path $zip -DestinationPath $tmp -Force
        $exe = Join-Path $tmp $BinName
        if (-not (Test-Path $exe)) { return $false }
        Copy-Item -Path $exe -Destination (Join-Path $BinDir $BinName) -Force
        Say "installed $BinDir\$BinName from release $ver"
        return $true
    } catch {
        Say "release download failed: $($_.Exception.Message)"
        return $false
    } finally {
        Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
    }
}

function Install-FromCargo {
    if (-not (Get-Command cargo -ErrorAction SilentlyContinue)) { return $false }
    Say "installing via cargo (this compiles from source)"
    if ($Version -eq "latest") {
        cargo install --git "https://github.com/$Repo" vibesurfer
    } else {
        cargo install --git "https://github.com/$Repo" --tag $Version vibesurfer
    }
    return ($LASTEXITCODE -eq 0)
}

if (-not (Install-FromRelease)) {
    if (-not (Install-FromCargo)) {
        Fail @"
could not install a release binary and no Rust toolchain was found.
Install Rust from https://rustup.rs and re-run, or grab a binary from
https://github.com/$Repo/releases
"@
    }
}

# Nudge about PATH so the first `vs` call resolves.
$paths = $env:PATH -split ";"
if ($paths -notcontains $BinDir) {
    Say "note: $BinDir is not on PATH. Add it, e.g.:"
    Say "  [Environment]::SetEnvironmentVariable('PATH', `"`$env:PATH;$BinDir`", 'User')"
}
Say "done. Run: vs --help"
