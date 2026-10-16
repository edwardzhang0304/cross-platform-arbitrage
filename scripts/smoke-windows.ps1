param([Parameter(Mandatory=$true)][string]$Exe)
$ErrorActionPreference = 'Stop'
$Root = Join-Path $env:RUNNER_TEMP ('openai-smoke-' + [guid]::NewGuid().ToString())
New-Item -ItemType Directory -Path $Root | Out-Null
$Port = 18884
$Base = "http://127.0.0.1:$Port"
$Process = Start-Process -FilePath (Resolve-Path $Exe) -ArgumentList @('--no-browser','--port',"$Port",'--data-dir',('"{0}"' -f $Root)) -PassThru
try {
  $Health = $null
  for ($i=0; $i -lt 60; $i++) {
    Start-Sleep -Milliseconds 250
    if ($Process.HasExited) { throw 'EXE exited during startup' }
    try { $Health = Invoke-RestMethod "$Base/health"; break } catch {}
  }
  if (!$Health -or $Health.application -ne 'openai-paired-trader' -or !$Health.sleep_prevention) { throw 'Health / power guard failed' }
  $Build = Get-Content (Join-Path (Split-Path $Exe) 'build-info.json') -Raw | ConvertFrom-Json
  foreach ($Key in @('source_commit','strategy_core_sha256','runtime','version')) {
    if ($Health.build.$Key -ne $Build.$Key) { throw "Running binary $Key differs from package" }
  }
  $Status = (Invoke-RestMethod "$Base/api/openai-inventory").data
  if ($Status.vault_unlocked -or $Status.configured -or $Status.view) { throw 'Clean package contains account state' }
  if ($Status.residual_recovery_version -ne 3) { throw 'Residual recovery version is incorrect' }
  $Policy=$Status.recovery_policy
  if ($Policy.retry_interval_ms -ne 3000 -or $Policy.initial_slippage_bps -ne 2 -or $Policy.step_slippage_bps -ne 1 -or $Policy.max_slippage_bps -ne 5 -or $Policy.unknown_order_action -ne 'lookup_only') { throw 'Recovery policy is incorrect' }
  # Add a second OPENAI slot, without credentials or any trading/network bootstrap.
  $List=(Invoke-RestMethod "$Base/api/accounts").data
  if ($List.profiles.Count -ne 2 -or $List.csrf -eq $Status.csrf) { throw 'Management identity is not independent' }
  $Create=@{id=[guid]::NewGuid().ToString();confirmation='CREATE_ACCOUNT_PROFILE';market='openai';label='OPENAI second'} | ConvertTo-Json -Compress
  $Denied=Invoke-RestMethod "$Base/api/accounts" -Method Post -ContentType 'application/json' -Headers @{'X-Inventory-Token'=$Status.csrf} -Body $Create
  if ($Denied.ok) { throw 'Trading token created an account' }
  $ManagementHeaders=@{'X-Inventory-Token'=$List.csrf}
  $Added=Invoke-RestMethod "$Base/api/accounts" -Method Post -ContentType 'application/json' -Headers $ManagementHeaders -Body $Create
  if (!$Added.ok) { throw 'Could not add account profile' }
  $ExtraProfile=$Added.data.profile
  $Again=Invoke-RestMethod "$Base/api/accounts" -Method Post -ContentType 'application/json' -Headers $ManagementHeaders -Body $Create
  if (!$Again.ok -or $Again.data.profile.id -ne $ExtraProfile.id -or (Invoke-RestMethod "$Base/api/accounts").data.profiles.Count -ne 3) { throw 'Creation is not idempotent' }
  $Extra=(Invoke-RestMethod "$Base$($ExtraProfile.inventory_api)").data
  if ($Extra.profile.id -ne $ExtraProfile.id -or $Extra.profile.market -ne 'openai' -or $Extra.configured -or $Extra.view -or $Extra.vault_unlocked -or $Extra.csrf -eq $Status.csrf) { throw 'Second OPENAI state is not isolated' }
  foreach ($Pair in @(@($Status,$Extra),@($Extra,$Status))) {
    foreach ($Command in @('start','stop','close_all','lock','quit','confirm_rollback_recovery','save_credentials','save_notifications')) {
      $Body=@{command=$Command;id=[guid]::NewGuid().ToString();confirmation='CLOSE_ALL_POSITIONS'} | ConvertTo-Json -Compress
      $Cross=Invoke-RestMethod "$Base$($Pair[1].profile.control_api)" -Method Post -ContentType 'application/json' -Headers @{'X-Inventory-Token'=$Pair[0].csrf} -Body $Body
      if ($Cross.ok) { throw "Same-market foreign token accepted for $Command" }
    }
  }
  $ExtraPage=(Invoke-WebRequest "$Base$($ExtraProfile.control_path)").Content
  if (!$ExtraPage.Contains($ExtraProfile.control_api) -or !$ExtraPage.Contains(('START_'+$ExtraProfile.id.ToUpperInvariant()+'_LIVE_STRATEGY')) -or $ExtraPage.Contains("fetch('/api/portable'")) { throw 'New account controls target wrong slot' }
  $ExtraMonitor=(Invoke-WebRequest "$Base$($ExtraProfile.monitor_path)").Content
  if (!$ExtraMonitor.Contains($ExtraProfile.inventory_api) -or !$ExtraMonitor.Contains('quantityDecimals":4')) { throw 'New account monitor target or quantity scale incorrect' }
  if ((Invoke-WebRequest "$Base/accounts").Content -notmatch 'addAccount') { throw 'Account management missing' }
  # Exercise the shipped collector with Windows PowerShell 5.1, not only pwsh.
  $Collector=Join-Path (Split-Path $Exe) 'Read-Diagnostics.ps1'
  & powershell.exe -NoProfile -ExecutionPolicy Bypass -File $Collector -Port $Port -DataDirectory $Root -OutputDirectory $Root -Samples 1 -IntervalSeconds 0 -SkipNetwork
  if ($LASTEXITCODE) { throw 'Packaged diagnostics failed on Windows PowerShell' }
  $Bundle=Get-ChildItem $Root -Filter 'CPA-Diagnostics-*.zip' | Select-Object -First 1
  Expand-Archive $Bundle.FullName (Join-Path $Root 'diagnostics-check')
  $ReportText=Get-Content (Join-Path $Root 'diagnostics-check/report.json') -Raw -Encoding utf8
  $Report=$ReportText | ConvertFrom-Json
  if ($ReportText.Contains($Status.csrf) -or !$Report.read_only -or $Report.observations.Count -ne 3) { throw 'Diagnostic redaction or schema failed' }
  foreach ($Observation in $Report.observations) {
    if (!$Observation.available -or $Observation.loaded -or $null -ne $Observation.snapshot) { throw 'Unloaded account misreported as a position' }
    if ($Observation.recovery_policy.max_slippage_bps -ne 5 -or $Observation.recovery_policy.retry_interval_ms -ne 3000) { throw 'Recovery policy missing from diagnostics' }
  }
  if ($Report.health.build.source_commit -ne $Build.source_commit) { throw 'Diagnostic build identity failed' }
  $Anth = (Invoke-RestMethod "$Base/api/anth-inventory").data
  if (($Anth.recovery_policy | ConvertTo-Json -Compress) -ne ($Policy | ConvertTo-Json -Compress)) { throw 'Markets have different recovery policies' }
  if ($Anth.profile.market -ne 'anth' -or $Anth.profile.mode -ne 'live' -or $Anth.view -or $Anth.configured -or $Anth.vault_unlocked) { throw 'ANTH profile is not empty/isolated' }
  if ($Anth.csrf -eq $Status.csrf) { throw 'Profiles share a control token' }
  $CrossProfile = Invoke-RestMethod "$Base/api/anth-portable" -Method Post -ContentType 'application/json' -Headers @{'X-Inventory-Token'=$Status.csrf} -Body '{"command":"start","id":"cross-profile","confirmation":"START_ANTH_LIVE_STRATEGY"}'
  if ($CrossProfile.ok) { throw 'OPENAI token controlled ANTH' }
  $AnthPage=(Invoke-WebRequest "$Base/anth").Content
  if ($AnthPage -notmatch 'Cross-Platform Arbitrage' -or $AnthPage -notmatch 'ANTH 账号1 实盘配置') { throw 'ANTH product/market title is incorrect' }
  if ($AnthPage -notmatch '/api/anth-portable' -or $AnthPage -notmatch 'START_ANTH_LIVE_STRATEGY') { throw 'ANTH control routing is incorrect' }
  if ($AnthPage -notmatch 'CONFIRM_ANTH_ROLLBACK_RECOVERY' -or $AnthPage -match 'CONFIRM_OPENAI_ROLLBACK_RECOVERY') { throw 'Manual recovery targets wrong market' }
  foreach ($Pair in @(@('openai',$Status,'/api/portable'),@('anth',$Anth,'/api/anth-portable'))) {
    $Label=$Pair[0].ToUpperInvariant()
    $RecoveryHeaders=@{'X-Inventory-Token'=$Pair[1].csrf}
    $Missing=Invoke-RestMethod "$Base$($Pair[2])" -Method Post -ContentType 'application/json' -Headers $RecoveryHeaders -Body '{"command":"confirm_rollback_recovery","id":"missing-confirmation"}'
    if ($Missing.ok) { throw 'Missing recovery confirmation accepted' }
    $Request=@{command='confirm_rollback_recovery';id='empty-recovery';confirmation="CONFIRM_${Label}_ROLLBACK_RECOVERY"} | ConvertTo-Json -Compress
    $Empty=Invoke-RestMethod "$Base$($Pair[2])" -Method Post -ContentType 'application/json' -Headers $RecoveryHeaders -Body $Request
    if ($Empty.ok) { throw 'Unloaded account recovered' }
  }
  $RecoveryScript=Join-Path (Split-Path $Exe) 'Confirm-Rollback-Recovery.ps1'
  $ParseErrors=$null; $Tokens=$null
  [System.Management.Automation.Language.Parser]::ParseFile($RecoveryScript,[ref]$Tokens,[ref]$ParseErrors) | Out-Null
  if ($ParseErrors.Count) { throw 'Packaged recovery script does not parse' }
  $RecoveryProbe=Start-Process -FilePath 'powershell.exe' -ArgumentList @('-NoProfile','-ExecutionPolicy','Bypass','-File',('"{0}"' -f $RecoveryScript),'-Market','openai','-Port',"$Port") -RedirectStandardOutput (Join-Path $Root 'recovery-probe.out') -RedirectStandardError (Join-Path $Root 'recovery-probe.err') -Wait -PassThru
  if ($RecoveryProbe.ExitCode -eq 0) { throw 'PowerShell 5.1 recovery script accepted an unloaded account' }
  if ((Get-Content (Join-Path $Root 'recovery-probe.err') -Raw) -notmatch 'Load the account and original ledger first') { throw 'PowerShell 5.1 recovery script failed before its account guard' }
  if ((Invoke-WebRequest "$Base/anth-inventory").Content -notmatch 'quantityDecimals":5') { throw 'ANTH quantity scale missing' }
  $Monitor = (Invoke-WebRequest "$Base/openai-inventory").Content
  if ($Monitor -notmatch '/assets/openai-market-visuals.js') { throw 'Chart asset missing' }
  if ($Monitor -notmatch '/assets/inventory-components.js') { throw 'Inventory components missing' }
  if ((Invoke-WebRequest "$Base/assets/inventory-components.js").Content -notmatch 'window.InventoryComponents') { throw 'Inventory component asset unavailable' }
  $ControlPage = (Invoke-WebRequest "$Base/").Content
  if ($ControlPage -notmatch '/api/portable') { throw 'Control page missing' }
  if ($ControlPage -notmatch 'Cross-Platform Arbitrage' -or $ControlPage -notmatch 'OPENAI 账号1 实盘配置') { throw 'OPENAI product/market title is incorrect' }
  $Rejected = Invoke-RestMethod "$Base/api/portable" -Method Post -ContentType 'application/json' -Body '{"command":"launch","id":"csrf-probe"}'
  if ($Rejected.ok) { throw 'Missing token was accepted' }
  $CrossSite = Invoke-RestMethod "$Base/api/openai-inventory" -Headers @{Origin='https://untrusted.example'}
  if ($CrossSite.ok) { throw 'Cross-origin status was accepted' }
  $Headers = @{'X-Inventory-Token'=$Status.csrf}
  $NoAccounts = Invoke-RestMethod "$Base/api/portable" -Method Post -ContentType 'application/json' -Headers $Headers -Body '{"command":"start","id":"no-account","confirmation":"START_OPENAI_LIVE_STRATEGY"}'
  if ($NoAccounts.ok) { throw 'Empty package started trading' }
  if (Test-Path (Join-Path $Root 'runtime/openai-inventory/live.sqlite')) { throw 'Ledger created without an account' }
  # Launching again must reuse the first process, not create another service.
  $Second = Start-Process -FilePath (Resolve-Path $Exe) -ArgumentList @('--no-browser','--port',"$Port",'--data-dir',('"{0}"' -f $Root)) -PassThru
  if (!$Second.WaitForExit(10000) -or $Second.ExitCode -ne 0) { throw 'Second launch did not reuse first instance' }
  $BadQuit = Invoke-RestMethod "$Base/api/portable" -Method Post -ContentType 'application/json' -Headers $Headers -Body '{"command":"quit","id":"no-confirm"}'
  if ($BadQuit.ok -or $Process.HasExited) { throw 'Quit confirmation was bypassed' }
  $Quit = Invoke-RestMethod "$Base/api/portable" -Method Post -ContentType 'application/json' -Headers $Headers -Body '{"command":"quit","id":"smoke-quit","confirmation":"QUIT_PROGRAM"}'
  if (!$Quit.ok -or !$Process.WaitForExit(10000)) { throw 'Graceful exit failed' }
  # Reopen the same data root: catalog survives, tokens do not.
  $Process=Start-Process -FilePath (Resolve-Path $Exe) -ArgumentList @('--no-browser','--port',"$Port",'--data-dir',('"{0}"' -f $Root)) -PassThru
  $Restored=$null
  for ($i=0;$i -lt 60;$i++) { Start-Sleep -Milliseconds 250; try { $Restored=(Invoke-RestMethod "$Base/api/accounts").data;break } catch {} }
  if (!$Restored -or $Restored.profiles.Count -ne 3 -or $Restored.csrf -eq $List.csrf) { throw 'Profile catalog restart failed' }
  $RestoredExtra=(Invoke-RestMethod "$Base$($ExtraProfile.inventory_api)").data
  if ($RestoredExtra.profile.id -ne $ExtraProfile.id -or $RestoredExtra.csrf -eq $Extra.csrf -or $RestoredExtra.configured) { throw 'Restored account lost identity or retained session token' }
  $Quit=Invoke-RestMethod "$Base$($ExtraProfile.control_api)" -Method Post -ContentType 'application/json' -Headers @{'X-Inventory-Token'=$RestoredExtra.csrf} -Body '{"command":"quit","id":"second-quit","confirmation":"QUIT_PROGRAM"}'
  if (!$Quit.ok -or !$Process.WaitForExit(10000)) { throw 'Dynamic profile cannot exit whole backend' }
  Write-Output 'PASS: Windows EXE, power request, pages, CSRF, origin, no trading, single instance and exit.'
} finally {
  if (!$Process.HasExited) { Stop-Process -Id $Process.Id -Force }
  Remove-Item -LiteralPath $Root -Recurse -Force
}
