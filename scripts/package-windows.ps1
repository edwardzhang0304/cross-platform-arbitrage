param([Parameter(Mandatory=$true)][string]$Exe)
$ErrorActionPreference='Stop'
$VSWhere='C:/Program Files (x86)/Microsoft Visual Studio/Installer/vswhere.exe'
$VS = & $VSWhere -latest -property installationPath
$Dumpbin = Get-ChildItem "$VS/VC/Tools/MSVC/*/bin/Hostx64/x64/dumpbin.exe" | Sort-Object FullName | Select-Object -Last 1
if (!$Dumpbin) { throw 'Cannot verify Windows binary dependencies' }
$Imports = & $Dumpbin.FullName /dependents $Exe
if ($LASTEXITCODE -or ($Imports -match '(?i)(VCRUNTIME|MSVCP|libssl|libcrypto|python).*\.dll')) { throw 'EXE requires a non-system runtime DLL' }
$Package='dist/OPENAI-Trader-Windows-x64'
New-Item -ItemType Directory -Force $Package | Out-Null
Copy-Item $Exe "$Package/OPENAI-Trader.exe"
Copy-Item README.md "$Package/使用说明.md"
Copy-Item config "$Package/config-templates" -Recurse
Copy-Item vendor/lighter-signing/licenses "$Package/licenses" -Recurse
Copy-Item vendor/lighter-signing/LICENSE* "$Package/licenses/" -ErrorAction Stop
Copy-Item vendor/hyperliquid_rust_sdk/LICENSE "$Package/licenses/Hyperliquid-MIT.txt"
# Only explicit public files are included. No data/, runtime/, keys or local config.
$Metadata = cargo metadata --locked --no-deps --format-version 1 | ConvertFrom-Json
if ($LASTEXITCODE) { throw 'Cannot read package version' }
$Version = ($Metadata.packages | Where-Object name -eq 'openai-paired-trader').version
if (!$Version) { throw 'Missing package version' }
@{version=$Version;commit=$env:GITHUB_SHA;target='x86_64-pc-windows-msvc';rust=(rustc --version);built_at=(Get-Date).ToUniversalTime().ToString('o')} | ConvertTo-Json | Set-Content "$Package/build-info.json" -Encoding utf8
$Zip='dist/OPENAI-Trader-Windows-x64.zip'
Compress-Archive -Path $Package -DestinationPath $Zip -Force
$Hash=(Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
"$Hash  OPENAI-Trader-Windows-x64.zip" | Set-Content 'dist/SHA256SUMS.txt' -Encoding ascii
