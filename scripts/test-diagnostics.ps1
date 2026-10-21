$ErrorActionPreference='Stop'
$Root=Join-Path $env:RUNNER_TEMP ('cpa-diagnostic-fixture-'+[guid]::NewGuid().ToString())
$ExtraId='p-'+('1'*32)
$ProfileIds=@('openai','anth',$ExtraId)
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
  foreach ($Market in $ProfileIds) {
    $Ledger=$Report.ledgers.$Market
    if ($Ledger.snapshot.entry_margin_block.rejection -ne 'margin_not_allowed' -or !$Ledger.snapshot.entry_margin_block.order_ref) { throw 'Margin protection evidence missing' }
    if ($Ledger.market_transport.Count -ne 2 -or $Ledger.market_transport[1].reconnect_count -ne 9 -or $Ledger.market_transport[1].history[0].detail -notmatch 'L2 silent') { throw 'Offline transport diagnosis missing' }
    if ($Ledger.market_alerts.schema -ne 2 -or $Ledger.market_alerts.evidence.unavailable_ms[0] -ne 50000 -or $Ledger.market_alerts.history[0].kind -ne 'delivered') { throw 'Market alert evidence/history missing' }
    if ($Ledger.one_time_notices.Count -ne 1 -or !$Ledger.one_time_notices[0].incident_ref -or !$Ledger.one_time_notices[0].delivered_ms -or $Ledger.one_time_notices[0].queued) { throw 'One-time notification receipt missing' }
    if (!$Ledger.readonly -or !$Ledger.available -or $Ledger.snapshot.held_groups -ne 7 -or $Ledger.snapshot.pending.first_filled -ne 700) { throw 'Offline snapshot extraction failed' }
    if ($Ledger.snapshot.ledger[0].venue -ne 'lighter' -or $Ledger.snapshot.ledger[1].venue -ne 'entropy') { throw 'Ledger venue mapping is incorrect' }
    if ($Ledger.snapshot.pending.entry_hedge_retry.attempt -ne 2 -or $Ledger.snapshot.pending.entry_hedge_retry.reason -notmatch 'hedge retry waiting') { throw 'Entry hedge retry diagnostics missing' }
    if ($Ledger.snapshot.pending.recovery_slippage_bps -ne 3 -or $Ledger.snapshot.pending.recovery_wait_reason -notmatch 'depth exceeds protected execution price') { throw 'Recovery price diagnostics were lost' }
    if ($Ledger.operation_events.events.Count -ne 2 -or $Ledger.recent_events.events.Count -ne 4) {
      # Only synthetic, already redaction-checked evidence is printed on CI failure.
      Write-Output ($Ledger | ConvertTo-Json -Depth 16)
      Write-Output ($Report.collection_problems | ConvertTo-Json)
      throw 'Historical pending event window failed'
    }
    $Recovery=$Ledger.recent_events.events[0].manual_recovery
    if ($Recovery.previous_rollbacks -ne 3 -or $Recovery.accounts.Count -ne 2 -or $Recovery.accounts[0].position_units -ne -4900 -or $Recovery.accounts[1].open_orders -ne 0) { throw 'Manual recovery audit evidence missing' }
    if ($Ledger.snapshot.emergency_exit.order_count -ne 1 -or $Ledger.snapshot.emergency_exit.buffered_fill_count -ne 1) { throw 'Emergency progress missing from diagnostics' }
    if ($Ledger.snapshot.emergency_exit.account_retry.Count -ne 2 -or $Ledger.snapshot.emergency_exit.prior[0].lookup_retry.attempts -ne 5 -or $Ledger.snapshot.emergency_exit.orders[0].lookup_note -notmatch 'remaining=500 expected=200 observed=200') { throw 'Order reconciliation quantities or backoff evidence missing' }
    if ($Ledger.snapshot.emergency_exit.orders[0].request.order_ref -ne $Ledger.snapshot.emergency_exit.buffered_fills[0].order_ref) { throw 'Emergency order/fill linkage lost' }
    if ($Ledger.snapshot.reason -notmatch 'account worker request deadline exceeded') { throw 'Useful diagnostic cause was lost' }
    if ($Ledger.snapshot.pending.requests.first.order_ref -ne $Ledger.snapshot.fills_in_operation_window[0].order_ref) { throw 'Opaque order linkage was lost' }
  }
  if ($Report.profiles.Count -ne 3 -or $Report.ledgers.$ExtraId.snapshot.closed_groups -ne 99) { throw 'Offline same-market account discovery or isolation failed' }
  $LargeRules=$Report.ledgers.$ExtraId.snapshot.rules
  if ($LargeRules.capital_multiplier -ne 10 -or $LargeRules.group_notional -ne '150' -or $LargeRules.leverage -ne 3 -or $LargeRules.max_loss_usdc -ne '300') { throw '50U account capital evidence missing' }
  if (@($Report.profiles | Where-Object { $_.id -eq $ExtraId })[0].capital_multiplier -ne 10) { throw 'Account capital tier lost from catalog' }
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
    if ($OnlineReport.observations.Count -ne 3 -or $OnlineReport.health.shared_market_feeds[0].consumers -ne 2) { throw 'Online multi-account discovery or shared feed diagnostics failed' }
    $Extra=$OnlineReport.observations | Where-Object { $_.profile_id -eq $ExtraId }
    if ($Extra.market -ne 'openai' -or $Extra.snapshot.closed_groups -ne 99) { throw 'Same-market account observation mixed up' }
    foreach ($Observation in $OnlineReport.observations) {
      if ($Observation.profile_id -eq 'anth') {
        if (!$Observation.available -or $Observation.loaded -or $Observation.flags -notcontains 'ACCOUNT_NOT_LOADED') { throw 'Unloaded account state was lost' }
        if ($Observation.startup_failure.operation -ne 'launch' -or $Observation.startup_failure.issue.stage -ne 'entropy_contract' -or $Observation.startup_failure.issue.code -ne 'margin_mode_unsupported') { throw 'Unloaded startup cause was lost' }
        if ($Observation.startup_failure.issue.message -notmatch 'margin mode unsupported') { throw 'Safe startup explanation missing' }
        if (!$OnlineReport.ledgers.anth.available -or $OnlineReport.ledgers.anth.snapshot.pending.first_filled -ne 700) { throw 'Unloaded account historical ledger evidence lost' }
        continue
      }
      if ($Observation.accounts[0].entry_margin.collateral -ne '0.1' -or $Observation.accounts[0].entry_margin.reported_available -ne '28' -or $Observation.flags -notcontains 'ENTRY_MARGIN_PROTECTION') { throw 'Raw entry margin evidence missing' }
      if ($Observation.risk_valuation.estimated_total_pnl -ne '-12.5' -or $Observation.risk_valuation.depth_complete -or $Observation.risk_valuation.unseen_exit_units[1] -ne 400 -or $Observation.flags -notcontains 'RISK_VALUATION_THIN_DEPTH') { throw 'Thin-depth risk valuation diagnostics missing' }
      if ($Observation.books[0].bid_levels -ne 1 -or $Observation.books[0].bid_units -ne 100 -or $Observation.books[1].ask_units -ne 200) { throw 'Visible depth totals missing' }
      if ($Observation.market_transport[1].source -ne 'rest' -or $Observation.market_transport[1].rest_books -ne 60 -or $Observation.market_transport[1].last_error -notmatch 'connect/TLS failed') { throw 'Online transport diagnosis missing' }
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
    foreach ($Market in $ProfileIds) {
      if ($null -ne $LockedReport.ledgers.$Market.snapshot -or !$LockedReport.ledgers.$Market.error) { throw 'Locked ledger was mistaken for a valid snapshot' }
    }
  } finally { if (!$Lock.HasExited) { Stop-Process -Id $Lock.Id -Force } }
  Write-Output 'PASS: Windows PowerShell 5.1 offline diagnostics, Unicode path, bounded historical evidence, redaction and unchanged input files.'
} finally { Remove-Item -LiteralPath $Root -Recurse -Force }
