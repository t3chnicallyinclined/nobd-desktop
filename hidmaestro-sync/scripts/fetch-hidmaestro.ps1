# Fetches the prebuilt HIDMaestro.Core.dll (driver embedded) into lib/ so the
# spike builds WITHOUT the WDK. Pin the version so builds are reproducible.
#
#   powershell -ExecutionPolicy Bypass -File scripts\fetch-hidmaestro.ps1
#
param(
    [string]$Version = "v1.3.17",
    [string]$Repo    = "hifihedgehog/HIDMaestro"
)

$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent
$lib  = Join-Path $root "lib"
New-Item -ItemType Directory -Force -Path $lib | Out-Null

$tmp = Join-Path $env:TEMP "hidmaestro-fetch"
if (Test-Path $tmp) { Remove-Item $tmp -Recurse -Force }
New-Item -ItemType Directory -Force -Path $tmp | Out-Null

$zip = Join-Path $tmp "HIDMaestro-$Version.zip"
Write-Host "Downloading HIDMaestro $Version …"
if (Get-Command gh -ErrorAction SilentlyContinue) {
    gh release download $Version --repo $Repo --pattern "*.zip" --dir $tmp --clobber
    $zip = (Get-ChildItem $tmp -Filter *.zip | Select-Object -First 1).FullName
} else {
    $url = "https://github.com/$Repo/releases/download/$Version/HIDMaestro-$Version.zip"
    Invoke-WebRequest -Uri $url -OutFile $zip
}

Expand-Archive -Path $zip -DestinationPath $tmp -Force
$dll = Get-ChildItem $tmp -Recurse -Filter "HIDMaestro.Core.dll" | Select-Object -First 1
if (-not $dll) { throw "HIDMaestro.Core.dll not found in the release zip" }
Copy-Item $dll.FullName (Join-Path $lib "HIDMaestro.Core.dll") -Force
Write-Host "Placed lib\HIDMaestro.Core.dll  (SHA-256 $((Get-FileHash $dll.FullName -Algorithm SHA256).Hash))"
Write-Host "Done. Now: dotnet build"
