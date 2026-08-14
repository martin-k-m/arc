<#
.SYNOPSIS
Install Arc from a GitHub release.

.EXAMPLE
.\install.ps1
.\install.ps1 -Version v1.0.0
.\install.ps1 -InstallDir C:\tools\arc

Download it, read it, then run it. Nothing here needs administrator rights.
#>
[CmdletBinding()]
param(
    [string]$Version = "latest",
    [string]$InstallDir = "$env:LOCALAPPDATA\Programs\arc"
)

$ErrorActionPreference = "Stop"
$Repo = "martin-k-m/arc"

if ([System.Environment]::Is64BitOperatingSystem -eq $false) {
    throw "Arc publishes 64-bit binaries only."
}
$target = "x86_64-pc-windows-msvc"

if ($Version -eq "latest") {
    $release = Invoke-RestMethod "https://api.github.com/repos/$Repo/releases/latest"
    $Version = $release.tag_name
    if (-not $Version) { throw "Could not determine the latest release." }
}

$name = "arc-$Version-$target"
$base = "https://github.com/$Repo/releases/download/$Version"
$tmp  = Join-Path ([System.IO.Path]::GetTempPath()) ([System.IO.Path]::GetRandomFileName())
New-Item -ItemType Directory -Path $tmp | Out-Null

try {
    Write-Host "arc install: downloading $name"
    $zip = Join-Path $tmp "$name.zip"
    Invoke-WebRequest -Uri "$base/$name.zip" -OutFile $zip
    $sums = Join-Path $tmp "SHA256SUMS"
    Invoke-WebRequest -Uri "$base/SHA256SUMS" -OutFile $sums

    # An archive whose checksum has not been verified is not installed.
    $actual = (Get-FileHash -Algorithm SHA256 $zip).Hash.ToLower()
    $line = Select-String -Path $sums -Pattern ([regex]::Escape("$name.zip")) | Select-Object -First 1
    if (-not $line) { throw "SHA256SUMS does not list $name.zip." }
    $expected = ($line.Line -split '\s+')[0].ToLower()
    if ($actual -ne $expected) {
        throw "Checksum mismatch for $name.zip - refusing to install."
    }
    Write-Host "arc install: checksum verified"

    Expand-Archive -Path $zip -DestinationPath $tmp -Force
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
    foreach ($b in "arc.exe", "arc-cache.exe", "arc-worker.exe") {
        Copy-Item (Join-Path $tmp "$name\$b") (Join-Path $InstallDir $b) -Force
    }

    Write-Host "arc install: installed to $InstallDir"
    $userPath = [Environment]::GetEnvironmentVariable("Path", "User")
    if ($userPath -notlike "*$InstallDir*") {
        Write-Host "arc install: $InstallDir is not on your PATH. To add it:"
        Write-Host "  [Environment]::SetEnvironmentVariable('Path', `"`$env:Path;$InstallDir`", 'User')"
    }
    & (Join-Path $InstallDir "arc.exe") --version
}
finally {
    Remove-Item -Recurse -Force $tmp -ErrorAction SilentlyContinue
}
