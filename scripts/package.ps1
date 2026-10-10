# Builds the release assets in dist\:
#   hwcast.exe                          (the whole app: double-click to install)
#   hwcast-<version>-windows-x64.zip    (exe + README + licenses)
#   SHA256SUMS.txt
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Push-Location $root
try {
  if (-not (Test-Path 'vendor\mods\LpcIO.bin')) { & "$PSScriptRoot\fetch-modules.ps1" }
  # Keep build-machine paths (user name, cargo cache, checkout folder) out of the release exe.
  $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
  $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
  $env:RUSTFLAGS = "--remap-path-prefix=$cargoHome=cargo --remap-path-prefix=$rustupHome=rustup --remap-path-prefix=$root=hwcast --remap-path-prefix=$env:USERPROFILE=user"
  cargo build --release
  if ($LASTEXITCODE) { throw 'cargo build failed' }
  $version = (Select-String -Path Cargo.toml -Pattern '^version\s*=\s*"(.+)"').Matches[0].Groups[1].Value
  $name = "hwcast-$version-windows-x64"
  Remove-Item dist -Recurse -Force -ErrorAction SilentlyContinue
  $stage = Join-Path 'dist' $name
  New-Item -ItemType Directory -Force "$stage\LICENSES" | Out-Null
  Copy-Item 'target\release\hwcast.exe' 'dist\hwcast.exe'
  Copy-Item 'target\release\hwcast.exe', 'README.md', 'LICENSE', 'THIRD_PARTY_NOTICES.md', 'config.example.json' $stage
  Copy-Item 'LICENSES\*' "$stage\LICENSES"
  Compress-Archive -Path "$stage\*" -DestinationPath "dist\$name.zip"
  Remove-Item $stage -Recurse -Force
  Get-ChildItem dist -File | ForEach-Object { '{0}  {1}' -f (Get-FileHash $_.FullName -Algorithm SHA256).Hash.ToLower(), $_.Name } |
    Set-Content 'dist\SHA256SUMS.txt' -Encoding ascii
  Get-Content 'dist\SHA256SUMS.txt'
} finally { Pop-Location }
