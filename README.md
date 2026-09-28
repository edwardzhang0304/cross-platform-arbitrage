# Cross-Platform Arbitrage · 多平台套利

同一套 Rust 策略核心支持 Lighter RH 与 Entropy 的 OPENAI、ANTH 配对交易，Windows 用于实盘，Mac 用于模拟。两个标的分别使用独立账户、配置和账本；安装包只包含程序和公开模板，不含个人账户文件。

源码仓库：[edwardzhang0304/cross-platform-arbitrage](https://github.com/edwardzhang0304/cross-platform-arbitrage)。原仓库名为 `openai-paired-trader`。

## 怎么用

1. 从本仓库成功的 GitHub Actions 运行中下载 `verified-mac-windows`，使用其中的 `Cross-Platform-Arbitrage-Windows-x64.zip`。仅通过双系统验证的产物用于交付。
2. **解压整个文件夹**到本机，例如 `D:\Cross-Platform-Arbitrage`。不要在压缩包内直接运行。
3. 双击 `Cross-Platform-Arbitrage.exe`，会打开 `http://127.0.0.1:18794`。
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
Cross-Platform-Arbitrage.exe       程序，升级时替换此文件
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

### 从旧名称升级

0.2.0-rc.5 将产品、Windows EXE 和安装包名称统一为 **Cross-Platform Arbitrage**。策略、账户绑定、密钥库与账本格式没有因改名而改变；仅改 GitHub 仓库名不会更新已经运行的 Windows 程序。

安排升级时，先分别停止两个策略，确认无待处理订单、挂单或未配平仓位，再退出后台并备份完整 `data`。把新 `Cross-Platform-Arbitrage.exe` 和对应 `build-info.json` 放入原安装目录，移走旧 `OPENAI-Trader.exe`，继续使用原目录里的 `data`。原安装目录可以仍叫 `OPENAI-Trader-Windows-x64`，无需改名。升级后由本人解锁、核对账户和账本，再启动策略。

服务识别字段和内部 Rust 库名保留旧值，确保重复启动检查、旧诊断工具和共享核心入口兼容；它们不作为产品显示名称。

监控汇总中的“净收益”包含剩余持仓的平仓估算；“平仓净收益”只计算已完成双腿平仓的部分，按对应组的实际开平仓成交扣除手续费，并计入对应已结算资金费。部分平仓按已平数量分摊开仓成本，剩余仓位浮动盈亏不计入此项。历史依据不完整时显示“—”，不会用零代替；延迟到账的资金费核对后会更新。

## 从已有 Mac 实盘迁移

这一步由操作者选择切换时间，打包本身不会停止 Mac 实盘或改变任何订单。

1. 先准备好 Windows 包。不要同时在两台电脑加载同一个 API 账户。
2. 在 Mac 停止交易，等未完成双腿、未知成交、挂单问题全部处理完，再关闭旧服务，并停用它的自动启动/自动拉起。**只关闭浏览器不算退出旧程序。**
3. 使用本项目的 `export-legacy` 功能导出一致性账本和实际配置。导出器要求旧账本文件锁已释放且策略已停止，不会读取或复制 Vault。

   ```sh
   cargo run --locked --bin cross-platform-arbitrage -- export-legacy \
     --source "/path/to/trade_xyz_local" \
     --account-config "/path/to/active-config.toml" \
     --output "/path/to/new-private-transfer/data"
   ```

4. **由本人**把旧的加密 Vault 文件复制到导出目录的 `keys/trading.vault`；不要把密码或明文私钥写进任何配置文件。
5. 把完整 `data` 目录复制到 Windows 的 EXE 旁。保留一份迁移前备份，Mac 保持关闭。
6. 在 Windows 输入原密码，只读检查两平台真实数量与账本组数。加载后检查双腿数量、挂单、组数和费用，再本人点击启动。

不可只复制运行中的 `.sqlite` 主文件。若 Windows 已经产生新成交，不可恢复旧 Mac 账本继续交易；必须迁回最新一致性账本再核对。

## 当前策略参数

当前参数在 `config/strategy.example.json`，真实账户身份留空。迁移时以原账本内配置为准，配置不一致程序会拒绝加载。

- 首组门槛 `max(MA5m, 5)`；双向选择；网格间隔 5U，锚点为本轮首次实际开仓价差。
- 开仓连续确认至少 5 秒；决策每秒，保留原均线与盘口计算实现。
- 每平台每组约 15 USDC 名义仓位，逐仓 3 倍，最多 20 组。
- 同价加仓距最近一次开仓或加仓完成至少 30 分钟，每个网格阶段最多 5 次。网格双腿成交配平后，重新给予 5 次额度，不结转未用次数；该标的全部平仓也重置。各阶段不强制用满，全部组数仍受最多 20 组约束。
- 逐组止盈，目标缩小幅度 `max(该组实际开仓价差 × 50%, 2)`；没有加入未批准的 24 小时衰减。
- 滑点保护 0.01%；资金费计入收益统计，沿用现有成交与风险条件。
- 保留 v2 双腿残余恢复、订单去重、未知成交查询及一致性检查。

2026-09-28（rc.7）：同价加仓由 60 分钟改为 30 分钟，OPENAI / ANTH、Mac 模拟 / Windows 实盘共用同一策略核心。升级前须停止旧程序并完成未结束的双腿操作，再退出后台、备份完整 `data`。新版加载账户和原账本时迁移参数，保留账户、持仓、成交、收益、已用同价次数及最近一次成交时间；不会因升级重置额度或自动启动。rc.6 的 5U 网格档位保持原样；更早的 2U 网格仍按原迁移规则转换。飞书加密配置随完整 `data` 保留，重新打开后仍需解锁。推送源码不会改变已运行的旧版本。

## 构建和验收

GitHub Actions 在 Windows 上编译、运行离线策略与签名测试、启动无凭据的程序做 HTTP/页面/退出检查，成功后生成 ZIP。CI 不接收交易凭据，也不下单。

```sh
cargo test --locked --workspace --lib
node --test tests/*.test.cjs
cargo build --locked --release --bin cross-platform-arbitrage
```

签名组件从实际使用的 `nautilus-lighter 0.60.0` 中仅提取 Rust 签名模块，保留其密码学实现和公开测试向量，避免引入整个无关交易框架。来源、许可证和提取说明见 `vendor/lighter-signing`。补丁版 Hyperliquid SDK 保留在 `vendor/hyperliquid_rust_sdk`。

Windows 自动验收不等于用户账户上的真实交易验收；真实凭据和现有仓位仍由本人迁移并核对。程序未做商业代码签名，Windows 可能显示发行者未知；请核对下载来源及 SHA256。


## OPENAI＋ANTH 双标的

Windows 同一 `18794` 程序下，OPENAI 和 ANTH 分别有独立监控页 `/openai-inventory`、`/anth-inventory`，配置入口分别为 `/`、`/anth`。原 OPENAI 数据保持原位置；ANTH 配置、加密密钥库及账本位于 `data/profiles/anth-live/`。两个策略独立启动、暂停和停止；退出整个程序前须分别停止两个策略。只有所有真实账户校验通过后才能加载实盘。

同一套 Rust 策略代码分别构建 Windows 实盘版和 Mac 模拟版，每个程序都同时管理 OPENAI、ANTH 两个标的。每台电脑只启动一个进程、使用一个 18794 入口。当前 Mac 验证使用模拟编译版本。它不编译实盘适配器、不加载 Vault，也不提供实盘控制接口。模拟和实盘两个 feature 不允许一起构建。

```sh
cargo run --locked --no-default-features --features paper-runtime --bin paired-paper -- \
  --data-dir /path/to/new-dual-paper-data --start
```

打开 `http://127.0.0.1:18794/`，在同一控制台管理 OPENAI 和 ANTH。默认同时加载两个模拟策略，`--start` 同时启动；两页分别为 `/openai-inventory`、`/anth-inventory`。不需要额外开关或第二个程序。每个标的有独立虚拟账户、配置、账本和控制令牌，与 Windows 实盘没有数据连接。新模拟数据目录必须为空，不复制实盘的 `data`。

策略参数沿用当前 OPENAI 模板。每个平台初始虚拟资金 100U，订单按公开盘口的可成交深度、数量精度和保护价格模拟 IOC；可能部分成交，恢复共用同一双腿执行状态机。手续费按模板费率计算。资金费按公开历史结算数据和当时的虚拟持仓估算；Entropy 的历史结算价格以结算前一小时的公开 K 线收盘价近似，不是真实账户到账金额。估算数据缺失时不冒充零值。模拟不包含排队和自身市场冲击；逐仓强平使用明确的全平近似，不能替代平台的真实强平引擎。

详情见 `docs/ANTH实施与隔离验收.md`。当前 Windows OPENAI 继续使用已交付版本，此分支不会自动替换运行中的 EXE。

跨平台源码与安装包一致性要求见 [同一核心与版本核对](docs/同一核心与版本核对.md)。Mac/Windows 构建必须通过同一批策略决策对比后才提供成对验证包。
