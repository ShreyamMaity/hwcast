# Downloads the signed PawnIO modules that hwcast embeds at build time
# (LpcIO.bin for Super I/O, AMDFamily17.bin for Ryzen SMN/MSR reads).
# Source: https://github.com/namazso/PawnIO.Modules (LGPL-2.1)
param([string]$Version = '0.2.11')
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
$dest = Join-Path $root 'vendor\mods'
$tmp = Join-Path ([IO.Path]::GetTempPath()) "pawnio-modules-$Version"
New-Item -ItemType Directory -Force $dest, $tmp | Out-Null
$zip = Join-Path $tmp 'modules.zip'
Invoke-WebRequest "https://github.com/namazso/PawnIO.Modules/releases/download/$Version/release_$($Version -replace '\.', '_').zip" -OutFile $zip -UseBasicParsing
Expand-Archive $zip $tmp -Force
foreach ($f in 'LpcIO.bin', 'AMDFamily17.bin', 'COPYING') { Copy-Item (Join-Path $tmp $f) $dest -Force }
Write-Host "PawnIO modules $Version -> $dest"
