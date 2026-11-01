$ErrorActionPreference = 'Stop'
$repoRoot = Split-Path -Parent $PSScriptRoot
$manifest = Join-Path $repoRoot 'rust/Cargo.toml'
$env:CARGO_TARGET_DIR = Join-Path $repoRoot '.build'

cargo fmt --manifest-path $manifest --check
if ($LASTEXITCODE -ne 0) { throw 'cargo fmt failed' }
cargo clippy --manifest-path $manifest --all-targets --all-features -- -D warnings
if ($LASTEXITCODE -ne 0) { throw 'cargo clippy failed' }
cargo test --manifest-path $manifest --all-features
if ($LASTEXITCODE -ne 0) { throw 'cargo test failed' }
cargo build --release --manifest-path $manifest
if ($LASTEXITCODE -ne 0) { throw 'cargo build failed' }

$builtExecutable = Join-Path $env:CARGO_TARGET_DIR 'release/sog.exe'
icacls $builtExecutable /setintegritylevel M
if ($LASTEXITCODE -ne 0) { throw 'Cannot label generated executable for desktop access' }
$artifactDirectory = Join-Path $repoRoot 'artifacts'
New-Item -ItemType Directory -Path $artifactDirectory -Force | Out-Null
$packagedExecutable = Join-Path $artifactDirectory 'sog.exe'
Copy-Item -LiteralPath $builtExecutable -Destination $packagedExecutable -Force
icacls $packagedExecutable /setintegritylevel M
if ($LASTEXITCODE -ne 0) { throw 'Cannot label packaged executable for desktop access' }
Get-FileHash -LiteralPath $builtExecutable,$packagedExecutable -Algorithm SHA256
