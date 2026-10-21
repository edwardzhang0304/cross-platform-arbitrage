param(
  [Parameter(Mandatory=$true)][Alias('Profile')][string]$Market,
  [ValidateRange(1024,65535)][int]$Port=18794
)
$ErrorActionPreference='Stop'
if ($Market -notmatch '^(openai|anth|p-[0-9a-f]{32})$') { throw 'Invalid account profile ID.' }
$Api=if ($Market -in @('openai','anth')) { "/api/$Market-inventory" } else { "/api/accounts/$Market/inventory" }
$Base="http://127.0.0.1:$Port"
$Label=$Market.ToUpperInvariant()
$Before=Invoke-RestMethod "$Base$Api" -TimeoutSec 10
if (!$Before.ok -or !$Before.data.view) { throw 'Load the account and original ledger first.' }
if (($Before.data.profile.id -and $Before.data.profile.id -ne $Market) -or ($Market -notin @('openai','anth') -and $Before.data.profile.id -ne $Market)) { throw 'Account profile mismatch.' }
$State=$Before.data.view.snapshot
if ($State.status -ne 'stopped' -or !$State.stop_requested -or !$State.paused) { throw 'Click Stop trading first. Wait until Stopped.' }
if ($State.consecutive_rollbacks -lt 3 -and $null -eq $State.entry_margin_block) { throw 'No entry protection lock to release.' }
Write-Output "This requests fresh authenticated checks on BOTH venues for $Label."
Write-Output 'The server must verify ledger/position agreement and no outstanding orders.'
Write-Output 'It verifies funding too when margin protection is active. Trading stays STOPPED. No orders are sent.'
$Answer=Read-Host "Type $Label to confirm (anything else cancels)"
if ($Answer -cne $Label) { throw 'Cancelled. Protection unchanged.' }
$Endpoint=if ($Market -eq 'openai') { '/api/portable' } elseif ($Market -eq 'anth') { '/api/anth-portable' } else { "/api/accounts/$Market/control" }
$Body=@{command='confirm_rollback_recovery';id=[guid]::NewGuid().ToString();confirmation="CONFIRM_${Label}_ROLLBACK_RECOVERY"} | ConvertTo-Json -Compress
$Result=Invoke-RestMethod "$Base$Endpoint" -Method Post -ContentType 'application/json' -Headers @{'X-Inventory-Token'=$Before.data.csrf} -Body $Body -TimeoutSec 45
if (!$Result.ok) { throw $Result.error }
$After=Invoke-RestMethod "$Base$Api" -TimeoutSec 10
if (!$After.ok -or !$After.data.view) { throw 'Request acknowledged; status unavailable. Run diagnostics before any further action.' }
if ($Before.data.profile.id -and $After.data.profile.id -ne $Before.data.profile.id) { throw 'Account profile changed; run diagnostics.' }
$State=$After.data.view.snapshot
[ordered]@{profile_id=$Market;market=$Before.data.profile.market;status=$State.status;consecutive_rollbacks=$State.consecutive_rollbacks;entry_margin_block=($null -ne $State.entry_margin_block);pending=($null -ne $State.pending);stop_requested=$State.stop_requested} | ConvertTo-Json
if ($State.status -ne 'stopped' -or $State.consecutive_rollbacks -ne 0 -or !$State.stop_requested -or $null -ne $State.pending -or $null -ne $State.entry_margin_block) { throw 'State changed or verification did not complete. Run diagnostics; do not start.' }
Write-Output 'VERIFIED: rollback protection released; trading remains STOPPED. Start separately from the page.'
