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
    if ($Ledger.snapshot.ledger[0].venue -ne 'lighter' -or $Ledger.snapshot.ledger[1].venue -ne 'entropy') { throw 'Ledger venue mapping is incorrect' }
    if ($Ledger.snapshot.pending.recovery_slippage_bps -ne 3 -or $Ledger.snapshot.pending.recovery_wait_reason -notmatch 'depth exceeds protected execution price') { throw 'Recovery price diagnostics were lost' }
    if ($Ledger.operation_events.events.Count -ne 2 -or $Ledger.recent_events.events.Count -ne 3) {
      # Only synthetic, already redaction-checked evidence is printed on CI failure.
      Write-Output ($Ledger | ConvertTo-Json -Depth 16)
      Write-Output ($Report.collection_problems | ConvertTo-Json)
      throw 'Historical pending event window failed'
    }
    if ($Ledger.snapshot.emergency_exit.order_count -ne 1 -or $Ledger.snapshot.emergency_exit.buffered_fill_count -ne 1) { throw 'Emergency progress missing from diagnostics' }
    if ($Ledger.snapshot.emergency_exit.orders[0].request.order_ref -ne $Ledger.snapshot.emergency_exit.buffered_fills[0].order_ref) { throw 'Emergency order/fill linkage lost' }
    if ($Ledger.snapshot.reason -notmatch 'account worker request deadline exceeded') { throw 'Useful diagnostic cause was lost' }
    if ($Ledger.snapshot.pending.requests.first.order_ref -ne $Ledger.snapshot.fills_in_operation_window[0].order_ref) { throw 'Opaque order linkage was lost' }
  }
  foreach ($Path in $Before.Keys) { if ((Get-FileHash $Path -Algorithm SHA256).Hash -ne $Before[$Path]) { throw 'Read-only collector modified input' } }
  if (@(Get-ChildItem (Join-Path $Root 'report') -File).Count -ne 2) { throw 'Unexpected raw diagnostic attachments' }
  $OnlineOutput=Join-Path $Root 'online-output'
  New-Item -ItemType Directory -Path $OnlineOutput | Out-Null
  $Server=Start-Process -FilePath (Get-Command python).Source -ArgumentList @(('"{0}"' -f (Join-Path $PSScriptRoot 'test-diagnostics-fixtures.py')),('"{0}"' -f $Data),'--serve') -PassThru
  try {
    for ($i=0; $i -lt 50 -and !(Test-Path (Join-Path $Data 'server-ready')); $i++) { Start-Sleep -Milliseconds 100 }
    if (!(Test-Path (Join-Path $Data 'server-ready'))) { throw 'Synthetic API failed to bind' }
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'Read-Diagnostics.ps1') -Port 19998 -DataDirectory $Data -OutputDirectory $OnlineOutput -Samples 1 -IntervalSeconds 0 -SkipNetwork
    if ($LASTEXITCODE) { throw 'Loaded-account collector failed' }
    $OnlineZip=Get-ChildItem $OnlineOutput -Filter '*.zip' | Select-Object -First 1
    Expand-Archive $OnlineZip.FullName (Join-Path $OnlineOutput 'report')
    $OnlineText=Get-Content (Join-Path $OnlineOutput 'report/report.json') -Raw -Encoding utf8
    if ($OnlineText -match 'synthetic-credential|0xaaaaaaaa|private\.invalid|unexpected_secret|private_key') { throw 'Live API redaction failed' }
    $OnlineReport=$OnlineText | ConvertFrom-Json
    if ($OnlineReport.health.entropy_info.limit -ne 900 -or $OnlineReport.health.entropy_info.local_deferrals -ne 7) { throw 'Info limiter diagnostics were lost' }
    foreach ($Observation in $OnlineReport.observations) {
      if (!$Observation.available -or !$Observation.loaded -or $Observation.snapshot.held_groups -ne 7 -or $Observation.accounts.Count -ne 2) { throw 'Loaded-account API extraction failed' }
      if ($Observation.flags -notcontains 'PENDING_OPERATION' -or $Observation.flags -match 'LEDGER_MISMATCH') { throw 'Pending ledger attribution is incorrect' }
      if ($Observation.snapshot.pending.requests.first.order_ref -ne $Observation.snapshot.fills_in_operation_window[0].order_ref) { throw 'Live API order linkage was lost' }
    }
  } finally { if (!$Server.HasExited) { Stop-Process -Id $Server.Id -Force } }
  $LockedOutput=Join-Path $Root 'locked-output'
  New-Item -ItemType Directory -Path $LockedOutput | Out-Null
  $Lock=Start-Process -FilePath (Get-Command python).Source -ArgumentList @(('"{0}"' -f (Join-Path $PSScriptRoot 'test-diagnostics-fixtures.py')),('"{0}"' -f $Data),'--hold-lock') -PassThru
  try {
    for ($i=0; $i -lt 50 -and !(Test-Path (Join-Path $Data 'lock-ready')); $i++) { Start-Sleep -Milliseconds 100 }
    if (!(Test-Path (Join-Path $Data 'lock-ready'))) { throw 'Synthetic exclusive lock failed' }
    $Watch=[Diagnostics.Stopwatch]::StartNew()
    & powershell.exe -NoProfile -ExecutionPolicy Bypass -File (Join-Path $PSScriptRoot 'Read-Diagnostics.ps1') -Port 19999 -DataDirectory $Data -OutputDirectory $LockedOutput -Samples 1 -IntervalSeconds 0 -SkipNetwork
    if ($LASTEXITCODE -or $Watch.Elapsed.TotalSeconds -gt 60) { throw 'Collector did not finish with bounded locked-ledger errors' }
    $LockedZip=Get-ChildItem $LockedOutput -Filter '*.zip' | Select-Object -First 1
    Expand-Archive $LockedZip.FullName (Join-Path $LockedOutput 'report')
    $LockedReport=Get-Content (Join-Path $LockedOutput 'report/report.json') -Raw -Encoding utf8 | ConvertFrom-Json
    foreach ($Market in @('openai','anth')) {
      if ($null -ne $LockedReport.ledgers.$Market.snapshot -or !$LockedReport.ledgers.$Market.error) { throw 'Locked ledger was mistaken for a valid snapshot' }
    }
  } finally { if (!$Lock.HasExited) { Stop-Process -Id $Lock.Id -Force } }
  Write-Output 'PASS: Windows PowerShell 5.1 offline diagnostics, Unicode path, bounded historical evidence, redaction and unchanged input files.'
} finally { Remove-Item -LiteralPath $Root -Recurse -Force }
