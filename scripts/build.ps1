# Builds target\release\hwcast.exe for release (used locally and by CI).
$ErrorActionPreference = 'Stop'
$root = Split-Path $PSScriptRoot
Push-Location $root
try {
  if (-not (Test-Path 'vendor\mods\LpcIO.bin')) { & "$PSScriptRoot\fetch-modules.ps1" }
  # Keep build-machine paths (user name, cargo cache, checkout folder) out of the release exe.
  $cargoHome = if ($env:CARGO_HOME) { $env:CARGO_HOME } else { Join-Path $env:USERPROFILE '.cargo' }
  $rustupHome = if ($env:RUSTUP_HOME) { $env:RUSTUP_HOME } else { Join-Path $env:USERPROFILE '.rustup' }
  $env:RUSTFLAGS = "--remap-path-prefix=$cargoHome=cargo --remap-path-prefix=$rustupHome=rustup --remap-path-prefix=$root=hwcast --remap-path-prefix=$env:USERPROFILE=user"
  cargo build --release --locked
  if ($LASTEXITCODE) { throw 'cargo build failed' }
} finally {
  Remove-Item Env:RUSTFLAGS -ErrorAction SilentlyContinue
  Pop-Location
}
