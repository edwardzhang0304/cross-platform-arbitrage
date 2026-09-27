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
  $Status = (Invoke-RestMethod "$Base/api/openai-inventory").data
  if ($Status.vault_unlocked -or $Status.configured -or $Status.view) { throw 'Clean package contains account state' }
  if ($Status.residual_recovery_version -ne 2) { throw 'Residual recovery version is incorrect' }
  $Anth = (Invoke-RestMethod "$Base/api/anth-inventory").data
  if ($Anth.profile.market -ne 'anth' -or $Anth.profile.mode -ne 'live' -or $Anth.view -or $Anth.configured -or $Anth.vault_unlocked) { throw 'ANTH profile is not empty/isolated' }
  if ($Anth.csrf -eq $Status.csrf) { throw 'Profiles share a control token' }
  $CrossProfile = Invoke-RestMethod "$Base/api/anth-portable" -Method Post -ContentType 'application/json' -Headers @{'X-Inventory-Token'=$Status.csrf} -Body '{"command":"start","id":"cross-profile","confirmation":"START_ANTH_LIVE_STRATEGY"}'
  if ($CrossProfile.ok) { throw 'OPENAI token controlled ANTH' }
  $AnthPage=(Invoke-WebRequest "$Base/anth").Content
  if ($AnthPage -notmatch '/api/anth-portable' -or $AnthPage -notmatch 'START_ANTH_LIVE_STRATEGY') { throw 'ANTH control routing is incorrect' }
  if ((Invoke-WebRequest "$Base/anth-inventory").Content -notmatch 'quantityDecimals":5') { throw 'ANTH quantity scale missing' }
  $Monitor = (Invoke-WebRequest "$Base/openai-inventory").Content
  if ($Monitor -notmatch '/assets/openai-market-visuals.js') { throw 'Chart asset missing' }
  if ((Invoke-WebRequest "$Base/").Content -notmatch '/api/portable') { throw 'Control page missing' }
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
  Write-Output 'PASS: Windows EXE, power request, pages, CSRF, origin, no trading, single instance and exit.'
} finally {
  if (!$Process.HasExited) { Stop-Process -Id $Process.Id -Force }
  Remove-Item -LiteralPath $Root -Recurse -Force
}
