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
$InfoPath = Join-Path (Resolve-Path $Package) 'build-info.json'
$Inspect = Start-Process -FilePath (Resolve-Path "$Package/OPENAI-Trader.exe") -ArgumentList '--build-info' -RedirectStandardOutput $InfoPath -Wait -PassThru
if ($Inspect.ExitCode -ne 0) { throw 'Cannot inspect packaged binary' }
$Info = Get-Content $InfoPath -Raw | ConvertFrom-Json
if ($Info.source_commit -ne $env:GITHUB_SHA -or $Info.dirty -or $Info.runtime -ne 'live' -or !$Info.strategy_core_sha256) { throw 'Packaged binary source identity mismatch' }
$Zip='dist/OPENAI-Trader-Windows-x64.zip'
Compress-Archive -Path $Package -DestinationPath $Zip -Force
$Hash=(Get-FileHash $Zip -Algorithm SHA256).Hash.ToLower()
"$Hash  OPENAI-Trader-Windows-x64.zip" | Set-Content 'dist/SHA256SUMS.txt' -Encoding ascii
