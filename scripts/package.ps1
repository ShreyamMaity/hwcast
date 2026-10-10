# Builds the release assets in dist\:
#   hwcast.exe                          (the whole app: double-click to install)
#   hwcast-<version>-windows-x64.zip    (exe + README + licenses)
#   SHA256SUMS.txt
# -Exe <path> packages an existing exe (for example the signed one from CI) instead of building.
param([string]$Exe)
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Push-Location $root
try {
  # The live demo on GitHub Pages is the same dashboard page in demo mode.
  New-Item -ItemType Directory -Force 'docs\demo' | Out-Null
  Copy-Item 'web\index.html' 'docs\demo\index.html' -Force

  if ($Exe) {
    $Exe = (Resolve-Path $Exe).Path
  } else {
    & "$PSScriptRoot\build.ps1"
    $Exe = Join-Path $root 'target\release\hwcast.exe'
  }
  $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
  $exeVersion = (Get-Item $Exe).VersionInfo.ProductVersion
  if ($exeVersion -ne $version) { throw "hwcast.exe is version '$exeVersion' but Cargo.toml says '$version'" }

  $name = "hwcast-$version-windows-x64"
  Remove-Item dist -Recurse -Force -ErrorAction SilentlyContinue
  $stage = Join-Path 'dist' $name
  New-Item -ItemType Directory -Force "$stage\LICENSES" | Out-Null
  Copy-Item $Exe 'dist\hwcast.exe'
  Copy-Item $Exe, 'README.md', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'config.example.json' $stage
  Copy-Item 'LICENSES\*' "$stage\LICENSES"
  Compress-Archive -Path "$stage\*" -DestinationPath "dist\$name.zip"
  Remove-Item $stage -Recurse -Force
  Get-ChildItem dist -File | ForEach-Object { '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name } |
    Set-Content 'dist\SHA256SUMS.txt' -Encoding ascii
  Get-Content 'dist\SHA256SUMS.txt'
} finally { Pop-Location }
