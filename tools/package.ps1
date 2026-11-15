param([switch]$Offline)
$ErrorActionPreference = 'Stop'
Set-Location -LiteralPath (Split-Path -Parent $PSScriptRoot)
$cargoArgs = @('build', '--release', '--locked', '--jobs', '2', '--target-dir', 'target')
if ($Offline) { $cargoArgs += '--offline' }
& cargo @cargoArgs
if ($LASTEXITCODE -ne 0) { throw 'Release build failed' }
$version = (Select-String -LiteralPath 'Cargo.toml' -Pattern '^version = "([^"]+)"$').Matches[0].Groups[1].Value
$asset = "FileRecovery-v$version.exe"
New-Item -ItemType Directory -Force -Path 'dist' | Out-Null
Copy-Item -LiteralPath 'target/release/FileRecovery.exe' -Destination (Join-Path 'dist' $asset)
Copy-Item -LiteralPath 'LICENSE' -Destination 'dist/LICENSE.txt'
Copy-Item -LiteralPath 'assets/third-party-notices.txt' -Destination 'dist/THIRD-PARTY-NOTICES.txt'
$digest = (Get-FileHash -LiteralPath (Join-Path 'dist' $asset) -Algorithm SHA256).Hash.ToLowerInvariant()
[IO.File]::WriteAllText((Join-Path (Get-Location) 'dist/SHA256SUMS.txt'), "$digest  $asset`n", [Text.UTF8Encoding]::new($false))
Write-Output (Join-Path (Get-Location) "dist/$asset")
