$ErrorActionPreference='Stop'
$Root=Join-Path $env:RUNNER_TEMP ('cpa-diagnostic-fixture-'+[guid]::NewGuid().ToString())
$Data=Join-Path $Root '中文路径\原账本数据'
New-Item -ItemType Directory -Path $Data -Force | Out-Null
try {
  python (Join-Path $PSScriptRoot 'test-diagnostics-fixtures.py') $Data
  if ($LASTEXITCODE) { throw 'Synthetic diagnostic fixture creation failed' }
  $Before=@{}
  Get-ChildItem $Data -Recurse -File | ForEach-Object { $Before[$_.FullName]=(Get-FileHash $_.FullName -Algorithm SHA256).Hash }
  & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'Read-Diagnostics.ps1') -Port 19999 -DataDirectory $Data -OutputDirectory $Root -Samples 1 -IntervalSeconds 0 -SkipNetwork
  if ($LASTEXITCODE) { throw 'Offline collector failed under Windows PowerShell 5.1' }
  $Zip=Get-ChildItem $Root -Filter 'CPA-Diagnostics-*.zip' | Select-Object -First 1
  Expand-Archive $Zip.FullName (Join-Path $Root 'report')
  $Text=Get-Content (Join-Path $Root 'report/report.json') -Raw -Encoding utf8
  $Report=$Text | ConvertFrom-Json
  if ($Text -match 'synthetic-credential|0xaaaaaaaa|private\.invalid|unexpected_secret|private_key|vault-secret|profile-secret') { throw 'Diagnostic leaked synthetic private content' }
  foreach ($Market in @('openai','anth')) {
    $Ledger=$Report.ledgers.$Market
    if (!$Ledger.readonly -or !$Ledger.available -or $Ledger.snapshot.held_groups -ne 7 -or $Ledger.snapshot.pending.first_filled -ne 700) { throw 'Offline snapshot extraction failed' }
    if ($Ledger.operation_events.events.Count -ne 2 -or $Ledger.recent_events.events.Count -ne 3) { throw 'Historical pending event window failed' }
    if ($Ledger.snapshot.reason -notmatch 'account worker request deadline exceeded') { throw 'Useful diagnostic cause was lost' }
    if ($Ledger.snapshot.pending.requests.first.order_ref -ne $Ledger.snapshot.fills_in_operation_window[0].order_ref) { throw 'Opaque order linkage was lost' }
  }
  foreach ($Path in $Before.Keys) { if ((Get-FileHash $Path -Algorithm SHA256).Hash -ne $Before[$Path]) { throw 'Read-only collector modified input' } }
  if (@(Get-ChildItem (Join-Path $Root 'report') -File).Count -ne 2) { throw 'Unexpected raw diagnostic attachments' }
  Write-Output 'PASS: Windows PowerShell 5.1 offline diagnostics, Unicode path, bounded historical evidence, redaction and unchanged input files.'
} finally { Remove-Item -LiteralPath $Root -Recurse -Force }
