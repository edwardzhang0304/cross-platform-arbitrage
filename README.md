# OPENAI 双平台交易 · Windows 免安装版

仅交付当前使用的 Lighter RH `OPENAI` 与 Entropy `io:OAI` 配对实盘策略、风险控制、双腿恢复、费用统计、监控页面与加密密钥管理。没有旧模拟盘、其他币种策略、跟单、研究数据或个人账户文件。

## 怎么用

1. 从本仓库 Releases 下载 `OPENAI-Trader-Windows-x64.zip`。
2. **解压整个文件夹**到本机，例如 `D:\OPENAI-Trader`。不要在压缩包内直接运行。
3. 双击 `OPENAI-Trader.exe`，会打开 `http://127.0.0.1:18794`。
4. 首次使用填两个平台的公开账户信息和**交易 API 私钥**，设定密钥库密码。已有 Mac 持仓请先按下面的迁移说明转移完整账本。
5. 解锁 → 只读检查 → 确认旧电脑程序已关闭 → 加载账户和账本 → 查看实盘监控 → 本人点击启动实盘策略。

用户电脑不需要安装 Rust、Python、Node 或 Visual C++ 运行库。目标系统为 Windows 10/11 **x64**（Intel/AMD）；ARM64 不作为本版本的已验证目标。

## 后台运行和不睡眠

- EXE 本身就是后台服务，关闭浏览器不会结束它。再次双击会打开已有程序，不重复运行。
- 运行期间通过 Windows 电源 API 持续申请“系统保持唤醒”；**允许锁屏和息屏，阻止闲置自动睡眠**。退出后自动释放申请，不永久修改电源计划。
- 手动选择“睡眠”、合盖、关机、断电和 Windows 更新重启仍可能中断程序。笔记本应接电，合盖行为请在 Windows 电源设置中选择“不采取任何操作”。
- 重启电脑后重新双击、解锁、核对并启用，不在无人确认的情况下自动恢复新增交易。
- 停止交易和退出程序是两件事。页面“停止交易”会等待正在执行的双腿操作处理结束；“退出后台程序”结束服务。持有仓位时退出会停止本机对仓位的管理。

## 文件分开存放

```text
OPENAI-Trader.exe                  程序，升级时替换此文件
config-templates/                 不含个人信息的模板
data/                             私人数据：不提交 Git，不放到公共网盘
  config/accounts.json            公开账户绑定；不存密码、API 私钥
  config/strategy.json            实际使用的完整策略参数
  keys/trading.vault              加密密钥库
  runtime/openai-inventory/
    live.sqlite                   持仓、成交、资金费、去重与恢复账本
    live.sqlite-wal / -shm        运行时可能出现，不能单独丢弃
```

程序仅监听本机 `127.0.0.1`；密钥只在本机密码加密文件与已解锁进程内使用。每次退出都会丢弃内存解锁状态。本版本不保存或迁移密码缓存，不提供资金划转钱包入口。

**升级程序不要覆盖或删除 `data`。持仓账本不是缓存，不能按“四小时数据”清理。** 图表行情使用原来的四小时滚动窗口。

## 从已有 Mac 实盘迁移

这一步由操作者选择切换时间，打包本身不会停止 Mac 实盘或改变任何订单。

1. 先准备好 Windows 包。不要同时在两台电脑加载同一个 API 账户。
2. 在 Mac 停止交易，等未完成双腿、未知成交、挂单问题全部处理完，再关闭旧服务，并停用它的自动启动/自动拉起。**只关闭浏览器不算退出旧程序。**
3. 使用本项目的 `export-legacy` 功能导出一致性账本和实际配置。导出器要求旧账本文件锁已释放且策略已停止，不会读取或复制 Vault。

   ```sh
   cargo run --locked --bin openai-paired-trader -- export-legacy \
     --source "/path/to/trade_xyz_local" \
     --account-config "/path/to/active-config.toml" \
     --output "/path/to/new-private-transfer/data"
   ```

4. **由本人**把旧的加密 Vault 文件复制到导出目录的 `keys/trading.vault`；不要把密码或明文私钥写进任何配置文件。
5. 把完整 `data` 目录复制到 Windows 的 EXE 旁。保留一份迁移前备份，Mac 保持关闭。
6. 在 Windows 输入原密码，只读检查两平台真实数量与账本组数。加载后检查双腿数量、挂单、组数和费用，再本人点击启动。

不可只复制运行中的 `.sqlite` 主文件。若 Windows 已经产生新成交，不可恢复旧 Mac 账本继续交易；必须迁回最新一致性账本再核对。

## 保持原策略

当前参数在 `config/strategy.example.json`，真实账户身份留空。迁移时以原账本内配置为准，配置不一致程序会拒绝加载。

- 首组门槛 `max(MA5m, 5)`；双向选择；网格间隔 2。
- 开仓连续确认至少 5 秒；决策每秒，保留原均线与盘口计算实现。
- 每平台每组约 15 USDC 名义仓位，逐仓 3 倍，最多 20 组。
- 同价加仓间隔 15 分钟，最多 5 次；保留现有网格锚点规则。
- 逐组止盈，目标缩小幅度 `max(该组实际开仓价差 × 50%, 2)`；没有加入未批准的 24 小时衰减。
- 滑点保护 0.01%；资金费计入收益统计，沿用现有成交与风险条件。
- 保留 v2 双腿残余恢复、订单去重、未知成交查询及一致性检查。

## 构建和验收

GitHub Actions 在 Windows 上编译、运行离线策略与签名测试、启动无凭据的程序做 HTTP/页面/退出检查，成功后生成 ZIP。CI 不接收交易凭据，也不下单。

```sh
cargo test --locked --workspace --lib
node --test tests/*.test.cjs
cargo build --locked --release --bin openai-paired-trader
```

签名组件从实际使用的 `nautilus-lighter 0.60.0` 中仅提取 Rust 签名模块，保留其密码学实现和公开测试向量，避免引入整个无关交易框架。来源、许可证和提取说明见 `vendor/lighter-signing`。补丁版 Hyperliquid SDK 保留在 `vendor/hyperliquid_rust_sdk`。

Windows 自动验收不等于用户账户上的真实交易验收；真实凭据和现有仓位仍由本人迁移并核对。程序未做商业代码签名，Windows 可能显示发行者未知；请核对下载来源及 SHA256。
