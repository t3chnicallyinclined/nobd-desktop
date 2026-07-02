# Vendors + signs the HIDMaestro driver bundle for NOBD's pure-Rust installer.
#
# Run ONCE (non-elevated is fine). It:
#   1. copies HIDMaestro's driver files (inf + dll + cat) into hm-native\driver\
#   2. creates a self-signed code-signing cert in your CurrentUser store (dev)
#   3. re-signs the catalog with it (the cat's inf+dll hashes are preserved, so
#      it still validates the vendored inf+dll - only the signature changes)
#   4. exports the public .cer (committed, for the installer to trust) and a
#      private .pfx (GITIGNORED - never commit)
#
# For a PUBLIC release, replace this with an EV / attestation-signed bundle so it
# installs on Secure-Boot / HVCI machines without importing a self-signed cert.

param(
    [string]$Source   = "$env:TEMP\HIDMaestro_*",   # HIDMaestro SDK extraction dir
    [string]$CertName = "NOBD Driver Signing (dev)",
    [string]$PfxPass  = "nobd-dev"
)
$ErrorActionPreference = "Stop"
$root = Split-Path $PSScriptRoot -Parent          # hm-native\
$dst  = Join-Path $root "driver"
New-Item -ItemType Directory -Force -Path $dst | Out-Null

# 1. Locate + copy the driver files (skip the xusb + signing-helper DLLs).
$src = Get-ChildItem $Source -Directory -ErrorAction SilentlyContinue |
    Where-Object { Test-Path (Join-Path $_.FullName "hidmaestro.inf") } | Select-Object -First 1
if (-not $src) { throw "HIDMaestro driver files not found in $Source. Run the C# --install-only once first." }
Write-Host "Source: $($src.FullName)"
foreach ($f in "hidmaestro.inf", "HIDMaestro.dll", "hidmaestro.cat") {
    Copy-Item (Join-Path $src.FullName $f) (Join-Path $dst $f) -Force
}

# 2. Create (or reuse) a self-signed code-signing cert.
$cert = Get-ChildItem Cert:\CurrentUser\My |
    Where-Object { $_.Subject -eq "CN=$CertName" } | Select-Object -First 1
if (-not $cert) {
    $cert = New-SelfSignedCertificate -Type CodeSigningCert -Subject "CN=$CertName" `
        -CertStoreLocation Cert:\CurrentUser\My -KeyExportPolicy Exportable `
        -HashAlgorithm SHA256 -NotAfter (Get-Date).AddYears(10)
    Write-Host "Created cert: $($cert.Thumbprint)"
} else {
    Write-Host "Reusing cert: $($cert.Thumbprint)"
}

# 3. Export public .cer (committed) + private .pfx (gitignored).
$cer = Join-Path $dst "nobd-driver.cer"
$pfx = Join-Path $dst "nobd-driver.pfx"
Export-Certificate -Cert $cert -FilePath $cer -Force | Out-Null
$sec = ConvertTo-SecureString $PfxPass -AsPlainText -Force
Export-PfxCertificate -Cert $cert -FilePath $pfx -Password $sec | Out-Null

# 4. Re-sign the catalog with our cert.
$signtool = Get-ChildItem "C:\Program Files (x86)\Windows Kits\10\bin" -Recurse -Filter signtool.exe -ErrorAction SilentlyContinue |
    Where-Object { $_.FullName -like '*10.0.26100.0\x64*' } | Select-Object -First 1
if (-not $signtool) { throw "signtool.exe (SDK 26100 x64) not found" }
& $signtool.FullName sign /fd SHA256 /f $pfx /p $PfxPass (Join-Path $dst "hidmaestro.cat")
if ($LASTEXITCODE -ne 0) { throw "signtool failed ($LASTEXITCODE)" }

Write-Host ""
Write-Host "Vendored bundle ready in $dst"
Get-ChildItem $dst | ForEach-Object { Write-Host "  $($_.Name)" }
Write-Host ""
Write-Host "COMMIT: hidmaestro.inf, HIDMaestro.dll, hidmaestro.cat, nobd-driver.cer, NOTICE.md"
Write-Host "DO NOT COMMIT: nobd-driver.pfx (private signing key) - it is gitignored."
