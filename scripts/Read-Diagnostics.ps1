# Read-only support bundle. No orders, controls, settings changes, raw ledgers or credentials are exported.
[CmdletBinding()]
param(
  [ValidateRange(1024,65535)][int]$Port=18794,
  [string]$DataDirectory=(Join-Path $PSScriptRoot 'data'),
  [string]$OutputDirectory=[Environment]::GetFolderPath('Desktop'),
  [ValidateRange(1,6)][int]$Samples=3,
  [ValidateRange(0,15)][int]$IntervalSeconds=5,
  [switch]$SkipNetwork
)
$ErrorActionPreference='Stop'
$script:Problems=New-Object 'System.Collections.Generic.List[string]'
function Ref($value) {
  if ($null -eq $value) { return $null }
  $hash=[Security.Cryptography.SHA256]::Create()
  try { return ([BitConverter]::ToString($hash.ComputeHash([Text.Encoding]::UTF8.GetBytes([string]$value)))).Replace('-','').Substring(0,20).ToLower() }
  finally { $hash.Dispose() }
}
function SafeText($value) {
  $s=[string]$value
  $s=$s -replace '(?i)https?://\S+','[url omitted]' -replace '(?i)Bearer\s+\S+','[token omitted]'
  $s=$s -replace '(?i)0x[0-9a-f]{40,}','[hex omitted]' -replace '[A-Za-z0-9_+/=-]{24,}','[long value omitted]'
  $s=$s -replace '[\w.+-]+@[\w.-]+\.[A-Za-z]{2,}','[email omitted]'
  if ($s.Length -gt 1200) { $s=$s.Substring(0,1200) }
  return $s
}
function Read-Local([string]$Path) {
  $req=[Net.HttpWebRequest]::Create("http://127.0.0.1:$Port$Path")
  $req.Proxy=$null; $req.Timeout=2500; $req.ReadWriteTimeout=2500
  $res=$req.GetResponse()
  try {
    $reader=New-Object IO.StreamReader($res.GetResponseStream(),[Text.Encoding]::UTF8)
    try { $text=$reader.ReadToEnd(); if ($text.Length -gt 16777216) { throw 'Response size limit' }; return ($text | ConvertFrom-Json) }
    finally { $reader.Dispose() }
  } finally { $res.Dispose() }
}
function Request-View($r) {
  if ($null -eq $r) { return $null }
  return [ordered]@{
    order_ref=(Ref $r.id); venue=$r.venue; side=$r.side; units=$r.units; limit=$r.limit
    reduce_only=$r.reduce_only; created_ms=$r.created_ms; expires_ms=$r.expires_ms; signed_expires_ms=$r.signed_expires_ms
  }
}
function Pending-View($p) {
  if ($null -eq $p) { return $null }
  $r=[ordered]@{}
  foreach ($phase in @('first','hedge','repair','align_close','unwind_hedge')) { $r[$phase]=Request-View $p.$phase }
  return [ordered]@{
    operation_ref=(Ref $p.id); action=$p.action; level=$p.level; min_entry_spread=$p.min_entry_spread; created_ms=$p.created_ms; requested_units=$p.requested_units
    first_venue=$p.first_venue; first_filled=$p.first_filled; hedge_filled=$p.hedge_filled; repair_filled=$p.repair_filled
    first_terminal=$p.first_terminal; hedge_terminal=$p.hedge_terminal; repair_terminal=$p.repair_terminal
    align_close_terminal=$p.align_close_terminal; unwind_hedge_terminal=$p.unwind_hedge_terminal
    align_close_filled=$p.align_close_filled; unwind_hedge_filled=$p.unwind_hedge_filled
    close_lot_ref=(Ref $p.close_lot_id); close_allocations=@($p.close_allocations | Where-Object { $null -ne $_ } | ForEach-Object { [ordered]@{lot_ref=(Ref $_.lot_id); units=$_.units} })
    failed=$p.failed; repair_attempt=$p.repair_attempt; repair_retry_after_ms=$p.repair_retry_after_ms
    quote_wait_started_ms=$p.quote_wait_started_ms; requests=$r
  }
}
function Snapshot-View($s) {
  if ($null -eq $s) { return $null }
  $fills=@($s.fills.PSObject.Properties.Value)
  $from=if ($null -ne $s.pending) { [long]$s.pending.created_ms-300000 } else { [long]0 }
  $to=if ($null -ne $s.pending) { [long]$s.pending.created_ms+600000 } else { [long]::MaxValue }
  $selected=@($fills | Where-Object { $null -ne $_ -and $_.time_ms -ge $from -and $_.time_ms -le $to } | Sort-Object time_ms -Descending | Select-Object -First 120)
  return [ordered]@{
    status=$s.status; reason=(SafeText $s.reason); reason_ref=(Ref $s.reason)
    paused=$s.paused; stop_requested=$s.stop_requested; close_requested=$s.close_requested; stop_after_close=$s.stop_after_close
    consecutive_rollbacks=$s.consecutive_rollbacks; recovery_after_ms=$s.recovery_after_ms
    orphan=($null -ne $s.live_orphan); loss_stop=($null -ne $s.loss_stop)
    held_groups=@($s.lots).Count; closed_groups=$s.closed_groups
    lot_units_sum=($s.lots | Measure-Object units -Sum).Sum
    ledger=@(for ($i=0; $i -lt @($s.positions).Count; $i++) { [ordered]@{venue=@('lighter','entropy')[$i]; units=$s.positions[$i].units} })
    lots=@($s.lots | Select-Object @{Name='lot_ref';Expression={Ref $_.id}},level,units,opened_ms,entry_spread)
    sequence=$s.sequence; last_sample_ms=$s.last_sample_ms; last_action_ms=$s.last_action_ms; armed=$s.armed; first_armed=$s.first_armed
    direction=$s.direction; anchor=$s.anchor; time_adds_used=$s.time_adds_used; last_open_completed=$s.last_open_completed
    pending=(Pending-View $s.pending)
    rules=($s.config | Select-Object market,mode,grid,max_groups,group_notional,entry_offset,entry_threshold_cap,entry_confirmation_ms,decision_ms,mean_window_ms,book_max_age_ms,account_max_age_ms,operation_timeout_ms,execution_slippage_bps,max_notional_per_venue,min_free_margin,max_loss_usdc,fee_lighter,fee_entropy,exit_profit_reserve,close_slice_notional,group_take_profit,exit_policy,auto_neutralize,@{Name='accumulation';Expression={$_.accumulation | Select-Object interval_ms,max_time_adds,quota_scope,entry_floor,contraction_ratio}})
    funding_synced_ms=$s.funding_synced_ms
    fills_in_operation_window=@($selected | ForEach-Object { [ordered]@{fill_ref=(Ref $_.id); order_ref=(Ref $_.order_id); venue=$_.venue; side=$_.side; units=$_.units; price=$_.price; fee=$_.fee; time_ms=$_.time_ms} })
    fills_capped=($selected.Count -eq 120)
  }
}
function Capture-Market([string]$Market) {
  $at=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
  try {
    $response=Read-Local "/api/$Market-inventory"
    if (!$response.ok) { throw 'Local endpoint rejected' }
    $at=[DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    $d=$response.data; $v=$d.view; $s=$v.snapshot
    $flags=New-Object 'System.Collections.Generic.List[string]'
    if ($null -eq $s) { $flags.Add('ACCOUNT_NOT_LOADED') }
    else {
      if ($null -ne $s.pending) { $flags.Add('PENDING_OPERATION') }
      if ($s.status -eq 'needs_attention') { $flags.Add('NEEDS_ATTENTION') }
      if ($null -ne $s.live_orphan) { $flags.Add('ORPHAN_INCIDENT') }
      if ($null -ne $s.loss_stop) { $flags.Add('LOSS_PROTECTION') }
      foreach ($venue in @('lighter','entropy')) {
        $a=@($v.accounts | Where-Object { $_.venue -eq $venue })
        $index=if ($venue -eq 'lighter') { 0 } else { 1 }
        if ($a.Count -ne 1) { $flags.Add("ACCOUNT_MISSING:$venue"); continue }
        if (!$a[0].authenticated) { $flags.Add("ACCOUNT_UNAUTHENTICATED:$venue") }
        if ($a[0].position_units -ne $s.positions[$index].units) { $flags.Add("LEDGER_MISMATCH:$venue") }
        if ($a[0].open_orders -gt 0) { $flags.Add("OPEN_ORDERS:$venue") }
        if ($a[0].observed_ms -gt $at -or $at-[long]$a[0].observed_ms -gt $s.config.account_max_age_ms) { $flags.Add("ACCOUNT_TIME_OR_AGE:$venue") }
      }
    }
    return [ordered]@{
      market=$Market; captured_ms=$at; available=$true; loaded=($null -ne $s); flags=$flags.ToArray()
      snapshot=(Snapshot-View $s)
      accounts=@($v.accounts | Select-Object venue,position_units,open_orders,authenticated,observed_ms,equity,free_margin,isolated,leverage)
      books=@($v.books | Where-Object { $null -ne $_ } | ForEach-Object { [ordered]@{connected=$_.connected; received_ms=$_.received_ms; bid=$(if (@($_.bids).Count -gt 0 -and $null -ne $_.bids) { $_.bids[0].price }); ask=$(if (@($_.asks).Count -gt 0 -and $null -ne $_.asks) { $_.asks[0].price })} })
      sampling=($v.sampling | Select-Object ready,continuity_active,covered_ms,required_ms); directional_means=$v.directional_means; submission_enabled=$v.submission_enabled
      warning=(SafeText $v.transient_warning); lookup_note=(SafeText $v.order_lookup_note)
      funding_complete=$v.profit_accounting.funding_complete
      notifications=[ordered]@{saved=$d.notifications.saved; unlocked=$d.notifications.unlocked; delivery=($d.notifications.delivery | Select-Object enabled,pending,dropped,last_sent_ms,@{Name='error';Expression={SafeText $_.error}})}
    }
  } catch {
    $script:Problems.Add("API unavailable: $Market ($($_.Exception.GetType().Name))")
    return [ordered]@{market=$Market; captured_ms=$at; available=$false; loaded=$null}
  }
}
function Read-Events([string]$Path,[long]$From=-1,[long]$To=-1) {
  $rows=@([CpaDiagnosticsSqlite]::Events($Path,$From,$To))
  $events=@(foreach ($row in $rows) {
    $part=$row -split "`t",3
    $e=$part[2] | ConvertFrom-Json
    [ordered]@{at_ms=[long]$part[0]; kind=(SafeText $part[1]); status=$e.status; reason=(SafeText $e.reason); reason_ref=(Ref $e.reason); pending=(Pending-View $e.pending); groups=$e.groups}
  })
  return [ordered]@{limit=120; capped=($rows.Count -eq 120); events=$events}
}
function Public-Clock([string]$Url) {
  $watch=[Diagnostics.Stopwatch]::StartNew(); $before=[DateTimeOffset]::UtcNow
  $req=[Net.HttpWebRequest]::Create($Url); $req.Method='HEAD'; $req.Timeout=3000; $req.AllowAutoRedirect=$false
  $res=$null
  try {
    try { $res=$req.GetResponse() } catch [Net.WebException] { $res=$_.Exception.Response; if ($null -eq $res) { throw } }
    $watch.Stop(); $date=[DateTimeOffset]::Parse($res.Headers['Date'])
    return [ordered]@{host=([uri]$Url).Host; http_status=[int]$res.StatusCode; rtt_ms=$watch.ElapsedMilliseconds; server_utc=$date.ToString('o'); apparent_clock_ahead_ms=[long](($before.AddMilliseconds($watch.ElapsedMilliseconds/2)-$date).TotalMilliseconds); note='HTTP Date reference only; CDN caching and system proxy may differ from venue worker connections'}
  } catch { return [ordered]@{host=([uri]$Url).Host; available=$false; error=$_.Exception.GetType().Name} }
  finally { if ($null -ne $res) { $res.Dispose() } }
}

$captured=New-Object 'System.Collections.Generic.List[object]'
$health=$null; $root=$DataDirectory
try {
  $h=Read-Local '/health'
  if ($h.application -ne 'openai-paired-trader') { throw 'Unexpected application' }
  $health=[ordered]@{version=$h.version; build=($h.build | Select-Object source_commit,strategy_core_sha256,version,runtime,target,dirty); data_dir=$h.data_dir; sleep_prevention=$h.sleep_prevention; entropy_info=($h.entropy_info | Select-Object available,limit,routine_limit,used_weight,routine_weight,local_deferrals,server_cooldown_remaining_ms)}
  $root=[string]$h.data_dir
} catch { $script:Problems.Add('Backend health unavailable; will also attempt readonly disk diagnostics') }
if ($root.StartsWith('\\?\')) { $root=$root.Substring(4) }
if ($root -notmatch '^[A-Za-z]:\\') { throw 'A local absolute Windows data directory is required. Use -DataDirectory.' }
$processes=@()
try {
  $processes=@(Get-NetTCPConnection -LocalPort $Port -State Listen -ErrorAction Stop | Select-Object -ExpandProperty OwningProcess -Unique | ForEach-Object {
    $p=Get-Process -Id $_ -ErrorAction Stop
    [ordered]@{pid=$p.Id; name=$p.ProcessName; path=$p.Path; executable_sha256=$(if ($p.Path) { (Get-FileHash -LiteralPath $p.Path -Algorithm SHA256).Hash.ToLower() })}
  })
} catch { $script:Problems.Add('Listening process unavailable (backend stopped or access restricted)') }
for ($i=0; $i -lt $Samples; $i++) {
  Write-Host "Collecting read-only snapshot $($i+1)/$Samples ..."
  foreach ($market in @('openai','anth')) { $captured.Add((Capture-Market $market)) }
  if ($i -lt $Samples-1) { Start-Sleep -Seconds $IntervalSeconds }
}
$disk=@{}
try {
  if (-not ('CpaDiagnosticsSqlite' -as [type])) { Add-Type -Path (Join-Path $PSScriptRoot 'DiagnosticsSqlite.cs') }
  foreach ($market in @('openai','anth')) {
    $relative=if ($market -eq 'openai') { 'runtime\openai-inventory\live.sqlite' } else { 'profiles\anth-live\runtime\inventory.sqlite' }
    $path=Join-Path $root $relative
    if (!(Test-Path -LiteralPath $path -PathType Leaf)) { $disk[$market]=@{available=$false; reason='ledger not found'}; continue }
    $entry=[ordered]@{available=$true; readonly=$true; bytes=(Get-Item -LiteralPath $path).Length}
    try {
      $raw=@([CpaDiagnosticsSqlite]::State($path)); $state=if ($raw.Count) { $raw[0] | ConvertFrom-Json } else { $null }
      $entry.snapshot=Snapshot-View $state
      if ($null -ne $state.pending) {
        try {
          # Explicit Int64 arithmetic avoids ambiguous Math.Max overload binding in PowerShell 5.1.
          $eventStart=[long]$state.pending.created_ms
          $eventFrom=[long]($eventStart-120000); if ($eventFrom -lt 0) { $eventFrom=[long]0 }
          $eventTo=[long]($eventStart+600000)
          $entry.operation_events=Read-Events -Path $path -From $eventFrom -To $eventTo
        }
        catch { $entry.operation_events_error='Window query incomplete or bounded timeout'; $entry.operation_events_error_type=$_.Exception.GetType().Name; $entry.operation_events_error_id=(SafeText $_.FullyQualifiedErrorId); $script:Problems.Add("Incident event window incomplete: $market") }
      }
      try { $entry.recent_events=Read-Events $path } catch { $entry.recent_events_error='Recent events incomplete or bounded timeout'; $entry.recent_events_error_type=$_.Exception.GetType().Name; $entry.recent_events_error_id=(SafeText $_.FullyQualifiedErrorId); $script:Problems.Add("Recent event query incomplete: $market") }
    } catch { $entry.error='Readonly ledger query failed'; $script:Problems.Add("Readonly ledger query failed: $market ($($_.Exception.GetType().Name))") }
    $disk[$market]=$entry
  }
} catch { $script:Problems.Add("System SQLite bridge unavailable ($($_.Exception.GetType().Name))") }
$clock=@{}
try {
  $pi=New-Object Diagnostics.ProcessStartInfo
  $pi.FileName=Join-Path $env:WINDIR 'System32\w32tm.exe'; $pi.Arguments='/query /status'; $pi.UseShellExecute=$false; $pi.RedirectStandardOutput=$true; $pi.CreateNoWindow=$true
  $p=[Diagnostics.Process]::Start($pi)
  try { if ($p.WaitForExit(3000)) { $clock.w32tm=SafeText $p.StandardOutput.ReadToEnd() } else { $p.Kill(); $clock.w32tm='Time status query timed out' } } finally { $p.Dispose() }
} catch { $clock.w32tm='Time service query unavailable' }
if (!$SkipNetwork) { $clock.public_references=@((Public-Clock 'https://api.hyperliquid.xyz/info'),(Public-Clock 'https://api.rh.lighter.xyz/')) }
$space=$null
try { $drive=New-Object IO.DriveInfo([IO.Path]::GetPathRoot($root)); $space=[ordered]@{available_bytes=$drive.AvailableFreeSpace; total_bytes=$drive.TotalSize} } catch {}
$report=[ordered]@{
  schema=1; tool_version='rc11'; generated_utc=[DateTimeOffset]::UtcNow.ToString('o'); read_only=$true
  health=$health; requested_data_dir=$DataDirectory; active_data_dir=$root; processes=$processes
  observations=$captured.ToArray(); ledgers=$disk; clock=$clock; disk_space=$space; collection_problems=$script:Problems.ToArray()
  limits='Separate readonly observations, not one atomic exchange snapshot. Event queries cap at 120 rows / 2 seconds; fills cap at 120. Null means unavailable, never zero. No private keys, wallet addresses, CSRF, notification credentials or raw ledger/config files exported.'
}
$name='CPA-Diagnostics-'+(Get-Date -Format 'yyyyMMdd-HHmmss')+'-'+[guid]::NewGuid().ToString('N').Substring(0,6)
$tmp=Join-Path ([IO.Path]::GetTempPath()) $name
$zip=Join-Path $OutputDirectory ($name+'.zip')
New-Item -ItemType Directory -Path $tmp | Out-Null
try {
  $json=$report | ConvertTo-Json -Depth 24
  if ([Text.Encoding]::UTF8.GetByteCount($json) -gt 8388608) { throw 'Diagnostic bundle exceeds 8 MiB limit' }
  [IO.File]::WriteAllText((Join-Path $tmp 'report.json'),$json,(New-Object Text.UTF8Encoding($false)))
  $lines=@('多平台套利：只读诊断包','将整个 ZIP 发给排查人员，不需要再发送 data 或密钥。',"生成时间：$($report.generated_utc)","版本：$($health.version)","数据目录：$root",'')
  foreach ($o in $captured) { $lines+="$($o.market): available=$($o.available), loaded=$($o.loaded), status=$($o.snapshot.status), flags=$($o.flags -join ',')" }
  $lines+=@('','采集问题：')+$script:Problems.ToArray()+@('','此脚本没有执行启动、停止、平仓、重试、校时或修改账本。缺失数据会标记，不能把空值当作零仓位。')
  [IO.File]::WriteAllLines((Join-Path $tmp '说明.txt'),[string[]]$lines,(New-Object Text.UTF8Encoding($true)))
  Compress-Archive -LiteralPath (Join-Path $tmp 'report.json'),(Join-Path $tmp '说明.txt') -DestinationPath $zip
  Write-Host "DONE: $zip"
  Write-Host 'Send this ZIP. No trading or ledger changes were requested.'
} finally { Remove-Item -LiteralPath $tmp -Recurse -Force }
