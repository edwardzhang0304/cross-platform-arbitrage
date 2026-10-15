param(
  [Parameter(Mandatory=$true)][ValidateSet('openai','anth')][string]$Market,
  [ValidateRange(1024,65535)][int]$Port=18794
)
$ErrorActionPreference='Stop'
$Base="http://127.0.0.1:$Port"
$Label=$Market.ToUpperInvariant()
$Before=Invoke-RestMethod "$Base/api/$Market-inventory" -TimeoutSec 10
if (!$Before.ok -or !$Before.data.view) { throw 'Load the account and original ledger first.' }
$State=$Before.data.view.snapshot
if ($State.status -ne 'stopped' -or !$State.stop_requested -or !$State.paused) { throw 'Click Stop trading first. Wait until Stopped.' }
if ($State.consecutive_rollbacks -lt 3) { throw 'No consecutive rollback lock to release.' }
Write-Output "This requests fresh authenticated checks on BOTH venues for $Label."
Write-Output 'The server must verify ledger/position agreement and no outstanding orders.'
Write-Output 'It releases only the rollback lock. Trading stays STOPPED. No orders are sent.'
$Answer=Read-Host "Type $Label to confirm (anything else cancels)"
if ($Answer -cne $Label) { throw 'Cancelled. Protection unchanged.' }
$Endpoint=if ($Market -eq 'openai') { '/api/portable' } else { '/api/anth-portable' }
$Body=@{command='confirm_rollback_recovery';id=[guid]::NewGuid().ToString();confirmation="CONFIRM_${Label}_ROLLBACK_RECOVERY"} | ConvertTo-Json -Compress
$Result=Invoke-RestMethod "$Base$Endpoint" -Method Post -ContentType 'application/json' -Headers @{'X-Inventory-Token'=$Before.data.csrf} -Body $Body -TimeoutSec 45
if (!$Result.ok) { throw $Result.error }
$After=Invoke-RestMethod "$Base/api/$Market-inventory" -TimeoutSec 10
if (!$After.ok -or !$After.data.view) { throw 'Request acknowledged; status unavailable. Run diagnostics before any further action.' }
$State=$After.data.view.snapshot
[ordered]@{market=$Market;status=$State.status;consecutive_rollbacks=$State.consecutive_rollbacks;pending=($null -ne $State.pending);stop_requested=$State.stop_requested} | ConvertTo-Json
if ($State.status -ne 'stopped' -or $State.consecutive_rollbacks -ne 0 -or !$State.stop_requested -or $null -ne $State.pending) { throw 'State changed or verification did not complete. Run diagnostics; do not start.' }
Write-Output 'VERIFIED: rollback protection released; trading remains STOPPED. Start separately from the page.'
