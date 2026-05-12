# ------------------------------------------------------------------
#  RemoteControl release pipeline (PC server + installer + APK).
# ------------------------------------------------------------------
#  Run from anywhere — paths are resolved relative to this script.
#  Produces dist/ with three artifacts:
#
#    * RemoteControl-Setup-<version>.exe    Windows installer
#    * RemoteControl-Server-<version>.exe   Standalone server binary
#    * RemoteControl-<version>.apk          Signed Android release APK
#
#  Requirements (one-time):
#    * Rust toolchain + LLVM (for NVENC bindgen)
#    * Visual Studio 2022 Build Tools (cmake + msvc for opus build)
#    * Android SDK + JDK (Android Studio installs both)
#    * Inno Setup 6+ — https://jrsoftware.org/isdl.php
#    * app-android/release.jks + app-android/keystore.properties
# ------------------------------------------------------------------

#Requires -Version 5.1
[CmdletBinding()]
param(
    # Skip steps useful when iterating on just one half of the build.
    [switch]$SkipPcBuild,
    [switch]$SkipAndroid,
    [switch]$SkipInstaller
)

$ErrorActionPreference = 'Stop'

$ScriptDir = Split-Path -Parent $MyInvocation.MyCommand.Path
$RepoRoot  = Resolve-Path (Join-Path $ScriptDir '..')
$DistDir   = Join-Path $RepoRoot 'dist'
$VendorDir = Join-Path $ScriptDir 'vendor'

New-Item -ItemType Directory -Force -Path $DistDir   | Out-Null
New-Item -ItemType Directory -Force -Path $VendorDir | Out-Null

# Read the app version from server-pc/Cargo.toml so the installer
# filename and the .iss #define stay in sync without manual editing.
$cargoToml = Get-Content (Join-Path $RepoRoot 'server-pc\Cargo.toml') -Raw
$Version = if ($cargoToml -match '(?m)^version\s*=\s*"([^"]+)"') { $Matches[1] } else { '0.0.0' }
Write-Host "RemoteControl release pipeline — version $Version" -ForegroundColor Cyan

# ------------------------------------------------------------------
#  1. PC server: cargo build --release
# ------------------------------------------------------------------
if (-not $SkipPcBuild) {
    Write-Host "`n[1/4] cargo build --release -p remotecontrol-server" -ForegroundColor Yellow

    # CMake is bundled with VS 2022 Build Tools but isn't on PATH by
    # default — audiopus_sys's build.rs needs it for the Opus C build.
    $vsCMake = 'C:\Program Files (x86)\Microsoft Visual Studio\2022\BuildTools\Common7\IDE\CommonExtensions\Microsoft\CMake\CMake\bin'
    if ((Test-Path "$vsCMake\cmake.exe") -and ($env:Path -notlike "*$vsCMake*")) {
        $env:Path = "$env:Path;$vsCMake"
    }

    Push-Location $RepoRoot
    try {
        & cargo build --release -p remotecontrol-server
        if ($LASTEXITCODE -ne 0) { throw "cargo build failed" }
    } finally {
        Pop-Location
    }

    $exePath = Join-Path $RepoRoot 'target\release\remotecontrol-server.exe'
    if (-not (Test-Path $exePath)) { throw "expected $exePath not found" }
    Copy-Item $exePath (Join-Path $DistDir "RemoteControl-Server-$Version.exe") -Force
}

# ------------------------------------------------------------------
#  2. Extract icon.ico from cargo OUT_DIR (build.rs writes it there).
#     ISCC needs a stable path; copy it next to the .iss script.
# ------------------------------------------------------------------
if (-not $SkipInstaller) {
    Write-Host "`n[2/4] Staging icon.ico for installer" -ForegroundColor Yellow

    $icoCandidates = Get-ChildItem `
        -Path (Join-Path $RepoRoot 'target\release\build') `
        -Recurse -Filter 'icon.ico' -ErrorAction SilentlyContinue
    if (-not $icoCandidates) { throw "icon.ico not found under target/release/build/ — did the PC build succeed?" }
    $latestIco = $icoCandidates | Sort-Object LastWriteTime -Descending | Select-Object -First 1
    Copy-Item $latestIco.FullName (Join-Path $ScriptDir 'icon.ico') -Force
    Write-Host "  ← $($latestIco.FullName)"
}

# ------------------------------------------------------------------
#  3. WebView2 evergreen bootstrapper (~1 MB) — bundled into the
#     installer and only run if the runtime is missing on the target.
# ------------------------------------------------------------------
if (-not $SkipInstaller) {
    Write-Host "`n[3/4] Fetching WebView2 bootstrapper" -ForegroundColor Yellow

    $webview2Stub = Join-Path $VendorDir 'MicrosoftEdgeWebview2Setup.exe'
    if (-not (Test-Path $webview2Stub)) {
        $url = 'https://go.microsoft.com/fwlink/p/?LinkId=2124703'
        Write-Host "  downloading $url"
        Invoke-WebRequest -Uri $url -OutFile $webview2Stub -UseBasicParsing
    } else {
        Write-Host "  already cached"
    }

    # Locate ISCC.exe. Inno Setup's installer can drop it into Program
    # Files (admin install), Program Files (x86) (admin install of
    # older builds), or %LOCALAPPDATA%\Programs (per-user install, which
    # winget defaults to).
    # `Where-Object` collapses a 1-element pipeline to a bare string,
    # not an array — so `[0]` on the result would return the first
    # character "C". Pick with `Select-Object -First 1` instead.
    $iscc = @(
        'C:\Program Files (x86)\Inno Setup 6\ISCC.exe',
        'C:\Program Files\Inno Setup 6\ISCC.exe',
        "$env:LOCALAPPDATA\Programs\Inno Setup 6\ISCC.exe"
    ) | Where-Object { Test-Path $_ } | Select-Object -First 1
    if (-not $iscc) {
        throw "Inno Setup 6 not found. Install via ``winget install JRSoftware.InnoSetup`` (or from https://jrsoftware.org/isdl.php) and re-run."
    }

    Write-Host "`n[4/4] Compiling installer via $iscc" -ForegroundColor Yellow
    Push-Location $ScriptDir
    try {
        & $iscc 'RemoteControl.iss'
        if ($LASTEXITCODE -ne 0) { throw "ISCC failed" }
    } finally {
        Pop-Location
    }

    $installerOut = Join-Path $ScriptDir "dist\RemoteControl-Setup-$Version.exe"
    if (Test-Path $installerOut) {
        Move-Item $installerOut $DistDir -Force
    }
}

# ------------------------------------------------------------------
#  Android release APK.
# ------------------------------------------------------------------
if (-not $SkipAndroid) {
    Write-Host "`n[Android] assembleRelease (signed + minified)" -ForegroundColor Yellow

    $appRoot = Join-Path $RepoRoot 'app-android'
    Push-Location $appRoot
    try {
        & .\gradlew.bat assembleRelease --no-daemon
        if ($LASTEXITCODE -ne 0) { throw "gradlew assembleRelease failed" }
    } finally {
        Pop-Location
    }

    $apk = Join-Path $appRoot 'app\build\outputs\apk\release\app-release.apk'
    if (-not (Test-Path $apk)) { throw "expected $apk not found" }
    Copy-Item $apk (Join-Path $DistDir "RemoteControl-$Version.apk") -Force
}

Write-Host "`nDone. Artifacts in $DistDir :" -ForegroundColor Green
Get-ChildItem $DistDir | Format-Table Name, @{N='Size';E={'{0:N1} MB' -f ($_.Length/1MB)}}, LastWriteTime
