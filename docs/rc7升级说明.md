# rc.7：30 分钟同价加仓与统一监控页面

适用：Windows 正在使用 rc.6，同时运行 OPENAI 和 ANTH。升级包不含账户、密钥或账本。

## 本次变化

- 同价加仓最短间隔从 60 分钟改为 30 分钟，两个标的一起生效；Mac 模拟和 Windows 实盘使用同一 Rust 策略核心。
- 间隔从最近一次开仓或加仓双腿成交配平完成时计算。达到 30 分钟只是满足时间条件，仍须满足价格、资金、风控及连续 5 秒确认条件。
- 网格仍为 5U；每个网格阶段最多 5 次同价加仓，网格双腿成交配平后重置；总持仓仍最多 20 组。
- 升级不补发额度、不重置最近成交时间。例如某档已用 4 次，升级后仍只剩 1 次；已经过去 40 分钟的仓位，在手动启动策略后可以按新规则参与判断。
- rc.6 已有持仓、网格档位、成交记录、费用、资金费、收益及账户绑定保留。完整复制 `data` 也会保留两标的飞书加密配置。
- 两个 inventory 页面共用收益、账户、持仓、成交组件及列宽。平台顺序固定为 Lighter、Entropy；OPENAI 数量仍显示 4 位小数，ANTH 显示 5 位。

## Windows 升级顺序

请逐步操作，上一项核对完再继续。准备安装包时，旧程序可以继续运行。

1. 把新的 `Cross-Platform-Arbitrage-Windows-x64.zip` 传到 Windows 桌面。
2. 在桌面新建空文件夹 `多平台套利-rc7`，把 ZIP 解压到这个新文件夹。找到里面的 `Cross-Platform-Arbitrage.exe`，先不要打开。这样不会和旧目录合并。
3. 在旧程序的 **OPENAI 实盘配置**页点击 **停止交易**，再到 **ANTH 实盘配置**页点击 **停止交易**。这是停止策略，不是“平掉全部持仓”。
4. 用下方只读命令核对两个标的：都显示 `stopped`，`pending`、`orphan` 都是 `false`，两平台挂单都是 0，平台数量分别与账本一致，账户数据新鲜。若有不一致，先处理，再升级。
5. 核对通过后，点一次 **退出后台程序**，两个页面的后台会一起退出。页面仍留在浏览器里不代表后台还在运行。
6. 把旧的整个 `Cross-Platform-Arbitrage-Windows-x64` 文件夹复制一份，命名为 `交易备份-rc7更新前`。必须在后台退出后复制，不能只复制运行中的 `.sqlite` 文件。
7. 从旧目录复制完整 `data` 文件夹，粘贴到新解压目录的 EXE 旁边。应看到同一层里有 `Cross-Platform-Arbitrage.exe` 和 `data`；不能变成 `data/data`。不要用 `config-templates` 替换旧账户配置。
8. 双击新目录里的 EXE。在 PowerShell 运行 `(Invoke-RestMethod 'http://127.0.0.1:18794/health') | ConvertTo-Json -Depth 4`，确认版本 `0.2.0-rc.7`、`build.dirty` 为 `false`、`build.source_commit` 与本次交付清单一致，`data_dir` 指向新目录。
9. 在 OPENAI 页输入原密码解锁，点 **只读检查两个账户**，再点 **加载账户和原账本**。加载会把旧 60 分钟参数迁移成 30 分钟。核对当前策略参数和原有持仓；先保持停止。
10. ANTH 页重复上一步。两个标的必须分别解锁、加载、核对。已有账户不需要重新填写 API 私钥或重新创建密钥库。
11. 再运行只读命令，确认两对账本与平台数量匹配，没有未完成操作，间隔都是 `1800000` 毫秒（30 分钟）。在两页分别发送飞书测试通知，确认手机都收到。
12. 先启动 OPENAI、核对状态，再启动 ANTH、核对状态。新版本产生交易后，不可用旧备份账本回滚继续交易。

### 只读检查命令

复制整段到 Windows PowerShell，不需要修改。这段只查询本机程序，不会下单。

```powershell
foreach ($market in @('openai','anth')) {
  $d = (Invoke-RestMethod "http://127.0.0.1:18794/api/$market-inventory").data
  $v = $d.view
  $s = $v.snapshot
  [ordered]@{
    market = $market
    version = $d.build.version
    loaded = ($null -ne $s)
    status = $s.status
    now_ms = [DateTimeOffset]::UtcNow.ToUnixTimeMilliseconds()
    interval_ms = $s.config.accumulation.interval_ms
    max_time_adds = $s.config.accumulation.max_time_adds
    time_adds_used = $s.time_adds_used
    lot_units_sum = ($s.lots | Measure-Object units -Sum).Sum
    ledger = @($s.positions | ForEach-Object { $_.units })
    accounts = @($v.accounts | Select-Object venue,position_units,open_orders,authenticated,observed_ms)
    pending = ($null -ne $s.pending)
    orphan = ($null -ne $s.live_orphan)
  } | ConvertTo-Json -Depth 5
}
```

## 验证与交付门槛

发布前运行前端回归、共用核心边界检查、两种运行模式的策略回归及签名测试。Windows 包还必须通过无凭据启动、页面资源、账户隔离及退出检查。

新增回归覆盖 30 分钟边界、边界后完整 5 秒确认、rc.6 账本迁移前后持仓/档位/已用次数/最近成交时间不变、运行中拒绝迁移、数据库锁、重复迁移及中途退出重试、交易和飞书密钥库文件原样保留。均使用合成数据，不连接真实账户。

仅交付 CI 中 `verified-mac-windows` 验证通过的包：Mac / Windows × 模拟 / 实盘四份报告的源码提交、核心哈希、参数和决策轨迹必须一致。具体提交、ZIP SHA256 和 CI 链接随本次本地交付清单提供。
