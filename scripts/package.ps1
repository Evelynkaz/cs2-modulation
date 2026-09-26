# Builds the shareable cs2mod exe and zips it with the viewer, a start.cmd and the docs a
# non-technical user needs (packaging/README.txt, NOTICE.md, VERSION.txt) into
# dist\cs2mod-<version>-windows-x64.zip. Windows PowerShell 5.1 compatible - CI and a dev box both
# run this with the ships-with-Windows powershell.exe, not pwsh.
#
# Writes only into dist\ and target\dist - never touches cache\ and never runs cs2mod.exe.

$ErrorActionPreference = "Stop"

$repoRoot = Split-Path -Parent $PSScriptRoot
Push-Location $repoRoot
try {
    # ---- version, commit, date --------------------------------------------------------------
    $cargoToml = Get-Content (Join-Path $repoRoot "Cargo.toml") -Raw
    if ($cargoToml -notmatch '(?ms)\[workspace\.package\].*?version\s*=\s*"([^"]+)"') {
        throw "could not find [workspace.package] version in Cargo.toml"
    }
    $version = $Matches[1]
    $gitHash = (git rev-parse --short HEAD).Trim()
    $date = Get-Date -Format "yyyy-MM-dd"
    Write-Host "packaging cs2mod $version ($gitHash, $date)"

    # ---- build ---------------------------------------------------------------------------------
    $env:RUSTFLAGS = "-C target-feature=+crt-static"
    $env:CARGO_PROFILE_RELEASE_DEBUG = "0"
    & "$repoRoot\scripts\cargo-msvc.cmd" build --release -p cs2mod-cli --target-dir target\dist
    if ($LASTEXITCODE -ne 0) {
        throw "cargo build failed with exit code $LASTEXITCODE"
    }

    $exePath = Join-Path $repoRoot "target\dist\release\cs2mod.exe"
    if (-not (Test-Path $exePath)) {
        throw "build did not produce $exePath"
    }

    # ---- static-CRT check: must not import VCRUNTIME140.dll ------------------------------------
    $exeBytes = [System.IO.File]::ReadAllBytes($exePath)
    $exeText = [System.Text.Encoding]::ASCII.GetString($exeBytes)
    if ($exeText -match "VCRUNTIME140\.dll") {
        throw "$exePath still references VCRUNTIME140.dll - crt-static did not take effect"
    }
    Write-Host "OK: cs2mod.exe does not reference VCRUNTIME140.dll"

    # ---- assemble dist\cs2mod\ -------------------------------------------------------------------
    $distDir = Join-Path $repoRoot "dist"
    $pkgDir = Join-Path $distDir "cs2mod"
    if (Test-Path $pkgDir) {
        Remove-Item $pkgDir -Recurse -Force
    }
    New-Item -ItemType Directory -Path $pkgDir -Force | Out-Null

    Copy-Item $exePath (Join-Path $pkgDir "cs2mod.exe")

    $viewerArchive = Join-Path $env:TEMP "cs2mod-viewer-archive.zip"
    if (Test-Path $viewerArchive) {
        Remove-Item $viewerArchive -Force
    }
    git archive --format=zip --output $viewerArchive HEAD viewer
    Expand-Archive -Path $viewerArchive -DestinationPath $pkgDir -Force
    Remove-Item $viewerArchive -Force

    Set-Content -Path (Join-Path $pkgDir "start.cmd") -Value @'
@echo off
cd /d "%~dp0"
"%~dp0cs2mod.exe"
'@ -Encoding ascii

    Copy-Item (Join-Path $repoRoot "packaging\README.txt") (Join-Path $pkgDir "README.txt")
    Copy-Item (Join-Path $repoRoot "NOTICE.md") (Join-Path $pkgDir "NOTICE.md")
    Set-Content -Path (Join-Path $pkgDir "VERSION.txt") -Value @"
version: $version
commit: $gitHash
date: $date
"@ -Encoding ascii

    # ---- zip -------------------------------------------------------------------------------------
    $zipPath = Join-Path $distDir "cs2mod-$version-windows-x64.zip"
    if (Test-Path $zipPath) {
        Remove-Item $zipPath -Force
    }
    Add-Type -AssemblyName System.IO.Compression.FileSystem
    [System.IO.Compression.ZipFile]::CreateFromDirectory($pkgDir, $zipPath, [System.IO.Compression.CompressionLevel]::Optimal, $true)

    # ---- verify zip layout: one top-level cs2mod/ folder, forward slashes ------------------------
    $zip = [System.IO.Compression.ZipFile]::OpenRead($zipPath)
    try {
        $hasExe = $false
        $hasViewerIndex = $false
        foreach ($entry in $zip.Entries) {
            if (-not $entry.FullName.StartsWith("cs2mod/")) {
                throw "zip entry '$($entry.FullName)' does not start with cs2mod/"
            }
            if ($entry.FullName.Contains("\")) {
                throw "zip entry '$($entry.FullName)' contains a backslash"
            }
            if ($entry.FullName -eq "cs2mod/cs2mod.exe") { $hasExe = $true }
            if ($entry.FullName -eq "cs2mod/viewer/index.html") { $hasViewerIndex = $true }
        }
        if (-not $hasExe) {
            throw "zip is missing cs2mod/cs2mod.exe"
        }
        if (-not $hasViewerIndex) {
            throw "zip is missing cs2mod/viewer/index.html"
        }
    }
    finally {
        $zip.Dispose()
    }

    $sha256 = (Get-FileHash -Path $zipPath -Algorithm SHA256).Hash
    Write-Host "package: $zipPath"
    Write-Host "sha256:  $sha256"
}
finally {
    Pop-Location
}
